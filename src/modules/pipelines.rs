//! CodePipeline dashboard: live status of every configured pipeline, stage progress
//! estimated from recent runs, CodeBuild logs, history, and deploy / approve / stop /
//! rollback (also promote, from the matrix). Pipelines are added by searching the account
//! (`n`) and saved to the config file.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::aws::{self, AwsError, codebuild, codepipeline as cp, logs};
use crate::config::{Hook, PipelineConfig, is_production};
use crate::pipeline::{self, Approval, Intent, StageView, Status};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{display_path, ellipsize, fmt_ago, fmt_duration, strip_ansi};

const AVERAGES_TTL: Duration = Duration::from_secs(15 * 60);

struct Row {
    cfg: PipelineConfig,
    state: Option<cp::PipelineState>,
    latest: Option<cp::ExecutionSummary>,
    error: Option<AwsError>,
    loading: bool,
}

impl Row {
    fn new(cfg: PipelineConfig) -> Row {
        Row { cfg, state: None, latest: None, error: None, loading: false }
    }

    fn name(&self) -> &str {
        &self.cfg.name
    }

    fn exec_status(&self) -> Option<Status> {
        self.latest.as_ref().map(|l| Status::parse(&l.status))
    }
}

struct LogChunk {
    stream: Option<(String, String)>,
    lines: Vec<String>,
    token: Option<String>,
    build_done: bool,
    phase: Option<String>,
}

enum LogsState {
    Loading,
    Live,
    Done,
    Error(String),
}

struct LogsPanel {
    stage: String,
    action: String,
    build_id: String,
    stream: Option<(String, String)>,
    view: LogView,
    token: Option<String>,
    state: LogsState,
    phase: Option<String>,
    in_flight: bool,
    last_fetch: Instant,
}

#[derive(PartialEq)]
enum Lower {
    None,
    Logs,
    History,
}

enum HookState {
    Running,
    Failed(String),
}

enum Dialog {
    /// `hooks`: (phase, command) to show before confirming; `local`: the config path when it
    /// was found in the current directory.
    Deploy {
        name: String,
        input: TextInput,
        needs_typing: bool,
        running: bool,
        hooks: Vec<(&'static str, String)>,
        local: Option<String>,
    },
    /// `context`: the execution and revision the approval belongs to.
    Approve { name: String, approval: Approval, context: String, approve: bool, input: TextInput },
    Stop { name: String, execution: String, abandon: bool, input: TextInput },
    Hooks { name: String, lines: Vec<String>, state: HookState },
    Busy(String),
    /// Searching the account's pipelines; the names match the picker's items.
    Add(Picker, Vec<String>),
    Remove { name: String },
    Release(Box<Release>),
}

/// Promote or roll back: deploy a given revision instead of the latest, or roll a stage
/// back (V2 pipelines). Confirmed like a deploy.
struct Release {
    name: String,
    kind: ReleaseKind,
    /// The pipeline's source actions; a pinned run fixes the revision of `sources[source]`.
    sources: Vec<cp::ActionDeclaration>,
    source: usize,
    /// False when several source actions could take the revision and none was matched.
    source_matched: bool,
    input: TextInput,
    needs_typing: bool,
    running: bool,
    hooks: Vec<(&'static str, String)>,
    local: Option<String>,
}

enum ReleaseKind {
    /// Deploy `revision`, which `from_env` runs.
    Promote { revision: String, from_env: String, summary: String, behind: Option<usize> },
    /// Back to `plan.targets[target]`: with `native`, by rolling a stage back; otherwise by
    /// re-running the pipeline on that revision.
    Rollback { plan: pipeline::RollbackPlan, target: usize, native: Option<NativeRollback> },
}

struct NativeRollback {
    stages: Vec<String>,
    stage: usize,
}

impl Release {
    fn source_name(&self) -> &str {
        self.sources.get(self.source).map_or("", |a| a.name.as_str())
    }

    /// The revision a pinned run deploys (`None` for a native rollback).
    fn revision(&self) -> Option<String> {
        match &self.kind {
            ReleaseKind::Promote { revision, .. } => Some(revision.clone()),
            ReleaseKind::Rollback { native: Some(_), .. } => None,
            ReleaseKind::Rollback { plan, target, .. } => {
                plan.targets.get(*target).and_then(|t| pipeline::revision_of(t, self.source_name())).map(str::to_string)
            }
        }
    }

    fn pin(&self) -> Option<cp::Pin> {
        Some(cp::Pin::new(self.sources.get(self.source)?, &self.revision()?))
    }

    /// Short form of what is deployed, for messages.
    fn label(&self) -> String {
        match &self.kind {
            ReleaseKind::Rollback { plan, target, native: Some(n) } => format!(
                "{} back to {}",
                n.stages[n.stage],
                plan.targets.get(*target).and_then(|t| pipeline::revision(t, &[]).commit).unwrap_or_default()
            ),
            _ => self.revision().map(|r| short(&r)).unwrap_or_default(),
        }
    }
}

fn short(revision: &str) -> String {
    revision.chars().take(7).collect()
}

enum Msg {
    Polled { name: String, result: aws::Result<(cp::PipelineState, Option<cp::ExecutionSummary>)> },
    Details { name: String, execution: String, result: aws::Result<Vec<cp::ActionExecutionDetail>> },
    Averages { name: String, result: aws::Result<HashMap<String, f64>> },
    History { name: String, result: aws::Result<Vec<cp::ExecutionSummary>> },
    Logs { build_id: String, result: aws::Result<LogChunk> },
    HookLine { name: String, line: String },
    HooksDone { name: String, pre_deploy: bool, result: Result<(), String> },
    /// `what`: the revision or rollback it runs, when not the latest; `pinned`: started on a
    /// given revision.
    Started { name: String, what: Option<String>, pinned: bool, result: aws::Result<String> },
    /// What a promote or rollback needs: the pipeline's definition and recent executions.
    Planned { intent: Intent, result: aws::Result<(cp::Declaration, Vec<cp::ExecutionSummary>)> },
    Acted { name: String, what: &'static str, result: aws::Result<()> },
    Discovered(aws::Result<Vec<cp::PipelineSummary>>),
    Report { name: String, full: bool, result: aws::Result<crate::report::Report> },
}

pub struct Pipelines {
    rows: Vec<Row>,
    selected: usize,
    list_state: ListState,
    /// Stage chosen with ←/→; `None` follows the most relevant stage.
    stage_cursor: Option<usize>,
    lower: Lower,
    poll_every: Duration,
    last_poll: Option<Instant>,
    polling: usize,
    details: HashMap<String, (String, Vec<cp::ActionExecutionDetail>)>,
    details_loading: HashSet<String>,
    averages: HashMap<String, (Instant, HashMap<String, f64>)>,
    averages_loading: HashSet<String>,
    history: HashMap<String, aws::Result<Vec<cp::ExecutionSummary>>>,
    history_loading: HashSet<String>,
    logs: Option<LogsPanel>,
    dialog: Option<Dialog>,
    env_name: String,
    region: String,
    /// Executions started from this UI → pipeline (for post-deploy hooks).
    started_by_us: HashMap<String, String>,
    last_seen: HashMap<String, (String, Status)>,
    seen_approvals: HashSet<String>,
    list_area: Rect,
    logs_area: Rect,
    /// The last deploy dialog fit on screen, so every hook it lists was visible.
    deploy_fits: bool,
    report_loading: bool,
    /// The revision the next start of this pipeline runs (promote, rollback), and its label.
    pinned: Option<(String, cp::Pin, String)>,
}

impl Pipelines {
    pub fn new() -> Pipelines {
        Pipelines {
            rows: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
            stage_cursor: None,
            lower: Lower::History,
            poll_every: Duration::from_secs(10),
            last_poll: None,
            polling: 0,
            details: HashMap::new(),
            details_loading: HashSet::new(),
            averages: HashMap::new(),
            averages_loading: HashSet::new(),
            history: HashMap::new(),
            history_loading: HashSet::new(),
            logs: None,
            dialog: None,
            env_name: String::new(),
            region: String::new(),
            started_by_us: HashMap::new(),
            last_seen: HashMap::new(),
            seen_approvals: HashSet::new(),
            list_area: Rect::default(),
            logs_area: Rect::default(),
            deploy_fits: false,
            report_loading: false,
            pinned: None,
        }
    }

    fn row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// "Since the last success …" for the detail panel, when the latest run did not succeed.
    fn since_success(&self, row: &Row) -> Option<String> {
        let latest = row.latest.as_ref()?;
        if Status::parse(&latest.status) == Status::Succeeded {
            return None;
        }
        let Some(Ok(history)) = self.history.get(row.name()) else { return None };
        let Some(i) = pipeline::last_success(history, 0) else {
            return Some(format!("No success in the last {} runs", history.len()));
        };
        let ok = &history[i];
        let rev = pipeline::revision(ok, &[]);
        let when = ok.last_update_time.map(fmt_ago).unwrap_or_default();
        let runs = if i == 1 { "1 run".to_string() } else { format!("{i} runs") };
        let compare = if self.compare_link(row).is_some() { " · g compare commits" } else { "" };
        Some(format!("Since the last success {} ({when}): {runs}{compare}", rev.commit.unwrap_or_default()))
    }

    /// Link comparing the last successful commit with the latest one.
    fn compare_link(&self, row: &Row) -> Option<String> {
        let Some(Ok(history)) = self.history.get(row.name()) else { return None };
        let ok = &history[pipeline::last_success(history, 0)?];
        let latest = row.latest.as_ref()?;
        let repo = pipeline::revision(latest, self.details_for(row)).repo;
        pipeline::compare_url(ok, latest, repo.as_deref())
    }

    /// The build log fills the screen (`z`).
    fn zoomed(&self) -> bool {
        self.logs.as_ref().is_some_and(|p| p.view.zoomed)
    }

    fn row_by_name(&self, name: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.name() == name)
    }

    fn details_for(&self, row: &Row) -> &[cp::ActionExecutionDetail] {
        match (self.details.get(row.name()), &row.latest) {
            (Some((exec, d)), Some(latest)) if *exec == latest.pipeline_execution_id => d,
            _ => &[],
        }
    }

    fn stages(&self, row: &Row) -> Vec<StageView> {
        match &row.state {
            Some(state) => pipeline::stages(state, row.latest.as_ref(), self.details_for(row)),
            None => Vec::new(),
        }
    }

    /// Exec status, upgraded to `Approval` when a gate is waiting.
    fn row_status(&self, row: &Row) -> Option<Status> {
        let status = row.exec_status()?;
        if status == Status::InProgress && self.stages(row).iter().any(|s| s.status == Status::Approval) {
            return Some(Status::Approval);
        }
        Some(status)
    }

    fn any_running(&self) -> bool {
        self.rows.iter().any(|r| r.exec_status().is_some_and(Status::is_running))
    }

    /// The stage to highlight: chosen by the user, else running / waiting / failed, else the last.
    fn current_stage(&self, stages: &[StageView]) -> usize {
        if let Some(c) = self.stage_cursor {
            return c.min(stages.len().saturating_sub(1));
        }
        stages
            .iter()
            .position(|s| matches!(s.status, Status::InProgress | Status::Approval | Status::Stopping))
            .or_else(|| stages.iter().position(|s| matches!(s.status, Status::Failed | Status::Stopped)))
            .unwrap_or(stages.len().saturating_sub(1))
    }

    // ---- loading --------------------------------------------------------------

    fn poll_all(&mut self, ctx: &mut Ctx) {
        self.last_poll = Some(Instant::now());
        let aws = ctx.env().aws.clone();
        for row in &mut self.rows {
            row.loading = true;
            self.polling += 1;
            let aws = aws.clone();
            let name = row.cfg.name.clone();
            ctx.spawn(move || {
                let result = cp::get_pipeline_state(&aws, &name)
                    .and_then(|state| Ok((state, cp::list_executions(&aws, &name, 1)?.into_iter().next())));
                Msg::Polled { name, result }
            });
        }
    }

    fn ensure_details(&mut self, ctx: &mut Ctx) {
        let Some(row) = self.row() else { return };
        let Some(latest) = &row.latest else { return };
        let name = row.name().to_string();
        let execution = latest.pipeline_execution_id.clone();
        let fresh = self.details.get(&name).is_some_and(|(e, _)| *e == execution);
        let running = Status::parse(&latest.status).is_running();
        if (fresh && !running) || self.details_loading.contains(&name) {
            return;
        }
        self.details_loading.insert(name.clone());
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = cp::list_action_executions(&aws, &name, Some(&execution), 100);
            Msg::Details { name, execution, result }
        });
    }

    fn ensure_averages(&mut self, ctx: &mut Ctx) {
        let Some(row) = self.row() else { return };
        let name = row.name().to_string();
        let fresh = self.averages.get(&name).is_some_and(|(t, _)| t.elapsed() < AVERAGES_TTL);
        if fresh || self.averages_loading.contains(&name) {
            return;
        }
        self.averages_loading.insert(name.clone());
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = cp::list_action_executions(&aws, &name, None, 100).map(|d| pipeline::stage_averages(&d));
            Msg::Averages { name, result }
        });
    }

    fn load_history(&mut self, ctx: &mut Ctx) {
        let Some(row) = self.row() else { return };
        let name = row.name().to_string();
        if !self.history_loading.insert(name.clone()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = cp::list_executions(&aws, &name, 25);
            Msg::History { name, result }
        });
    }

    fn select(&mut self, index: usize, ctx: &mut Ctx) {
        let index = index.min(self.rows.len().saturating_sub(1));
        if index != self.selected {
            self.selected = index;
            self.stage_cursor = None;
            if self.lower == Lower::Logs {
                self.lower = Lower::History;
                self.logs = None;
            }
        }
        self.list_state.select(Some(index));
        self.ensure_details(ctx);
        self.ensure_averages(ctx);
        if self.lower == Lower::History {
            self.load_history(ctx);
        }
    }

    // ---- logs -------------------------------------------------------------------

    fn toggle_logs(&mut self, ctx: &mut Ctx) {
        if self.lower == Lower::Logs {
            self.lower = Lower::History;
            self.logs = None;
            return;
        }
        let Some(row) = self.row() else { return };
        let stages = self.stages(row);
        let current = self.current_stage(&stages);
        let target = stages
            .get(current)
            .and_then(|s| s.build_action().map(|a| (s, a)))
            .or_else(|| stages.iter().rev().find_map(|s| s.build_action().map(|a| (s, a))));
        let Some((stage, action)) = target else {
            ctx.toast(Level::Warn, "No CodeBuild action with logs in this execution");
            return;
        };
        self.logs = Some(LogsPanel {
            stage: stage.name.clone(),
            action: action.name.clone(),
            build_id: action.build_id.clone().unwrap_or_default(),
            stream: None,
            view: LogView::default(),
            token: None,
            state: LogsState::Loading,
            phase: None,
            in_flight: false,
            last_fetch: Instant::now(),
        });
        self.lower = Lower::Logs;
        self.fetch_logs(ctx);
    }

    fn fetch_logs(&mut self, ctx: &mut Ctx) {
        let Some(panel) = &mut self.logs else { return };
        if panel.in_flight {
            return;
        }
        panel.in_flight = true;
        panel.last_fetch = Instant::now();
        let aws = ctx.env().aws.clone();
        let build_id = panel.build_id.clone();
        let known_stream = panel.stream.clone();
        let start_token = panel.token.clone();
        ctx.spawn(move || {
            let result = (|| -> aws::Result<LogChunk> {
                let build = codebuild::get_build(&aws, &build_id)?;
                let build_done = build.as_ref().is_none_or(|b| b.build_status != "IN_PROGRESS");
                let phase = build.as_ref().and_then(|b| b.current_phase.clone());
                let stream = known_stream.or_else(|| {
                    let l = build.as_ref()?.logs.as_ref()?;
                    Some((l.group_name.clone()?, l.stream_name.clone()?))
                });
                let mut chunk = LogChunk { stream: stream.clone(), lines: Vec::new(), token: start_token, build_done, phase };
                let Some((group, name)) = stream else { return Ok(chunk) };
                for _ in 0..20 {
                    let page = match logs::get_log_events(&aws, &group, &name, chunk.token.as_deref()) {
                        // The stream appears a few seconds after the build starts.
                        Err(AwsError::Api { code, .. }) if code == "ResourceNotFoundException" => break,
                        other => other?,
                    };
                    let empty = page.events.is_empty();
                    for event in page.events {
                        chunk.lines.extend(event.message.lines().map(|l| strip_ansi(l.trim_end())));
                    }
                    let same = page.next_forward_token == chunk.token;
                    chunk.token = page.next_forward_token;
                    if empty || same {
                        break;
                    }
                }
                Ok(chunk)
            })();
            Msg::Logs { build_id, result }
        });
    }

    // ---- actions ----------------------------------------------------------------

    fn open_deploy(&mut self, ctx: &mut Ctx) {
        self.pinned = None;
        let Some(row) = self.row() else { return };
        let phases = [("pre-deploy", &row.cfg.hooks.pre_deploy), ("post-deploy", &row.cfg.hooks.post_deploy)];
        let hooks: Vec<(&'static str, String)> = phases
            .into_iter()
            .flat_map(|(phase, list)| list.iter().map(move |h| (phase, crate::util::show_invisible(&h.command))))
            .collect();
        let local = if hooks.is_empty() { None } else { ctx.local_config() };
        self.dialog = Some(Dialog::Deploy {
            name: row.name().to_string(),
            input: TextInput::default(),
            needs_typing: is_production(&self.env_name),
            running: row.exec_status().is_some_and(Status::is_running),
            hooks,
            local,
        });
    }

    fn begin_deploy(&mut self, name: String, ctx: &mut Ctx) {
        let hooks = self.row_by_name(&name).map(|r| r.cfg.hooks.pre_deploy.clone()).unwrap_or_default();
        if hooks.is_empty() {
            self.start_pipeline(name, ctx);
            return;
        }
        self.dialog = Some(Dialog::Hooks { name: name.clone(), lines: Vec::new(), state: HookState::Running });
        run_hooks(ctx, name, hooks, true);
    }

    fn start_pipeline(&mut self, name: String, ctx: &mut Ctx) {
        let pinned = self.pinned.take().filter(|(n, ..)| *n == name);
        let what = pinned.as_ref().map(|(.., label)| label.clone());
        self.dialog = Some(Dialog::Busy(match &what {
            Some(w) => format!("Starting {name} at {w}…"),
            None => format!("Starting {name}…"),
        }));
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = cp::start_pinned(&aws, &name, pinned.as_ref().map(|(_, pin, _)| pin));
            Msg::Started { name, pinned: what.is_some(), what, result }
        });
    }

    /// Reads what a promote or a rollback of a pipeline needs, then shows its dialog.
    fn plan(&mut self, intent: Intent, ctx: &mut Ctx) {
        let name = intent.pipeline().to_string();
        let Some(index) = self.rows.iter().position(|r| r.name() == name) else {
            return ctx.toast(Level::Warn, format!("{name} is not in {}'s pipelines", self.env_name));
        };
        self.select(index, ctx);
        let what = if matches!(intent, Intent::Promote { .. }) { "promote" } else { "rollback" };
        self.dialog = Some(Dialog::Busy(format!("Preparing the {what} of {name}…")));
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let runs = if matches!(intent, Intent::Rollback { .. }) { 25 } else { 1 };
            let result = cp::get_pipeline(&aws, &name).and_then(|d| Ok((d, cp::list_executions(&aws, &name, runs)?)));
            Msg::Planned { intent, result }
        });
    }

    fn on_planned(
        &mut self,
        intent: Intent,
        result: aws::Result<(cp::Declaration, Vec<cp::ExecutionSummary>)>,
        ctx: &mut Ctx,
    ) {
        // Closed while it was being prepared: cancelled.
        if !matches!(self.dialog, Some(Dialog::Busy(_))) {
            return;
        }
        self.dialog = None;
        let name = intent.pipeline().to_string();
        let (declaration, history) = match result {
            Ok(r) => r,
            Err(e) => return self.report(ctx, &format!("Could not read {name}"), e),
        };
        let sources: Vec<cp::ActionDeclaration> = declaration.source_actions().into_iter().cloned().collect();
        let pick = |wanted: &str| pipeline::pick_source(&sources.iter().collect::<Vec<_>>(), wanted);
        let (kind, source, matched) = match intent {
            Intent::Promote { revision, source_action, from_env, summary, behind, .. } => {
                if sources.is_empty() {
                    return ctx.toast(Level::Error, format!("{name} has no source action to deploy {} with", short(&revision)));
                }
                let chosen = pick(&source_action);
                (ReleaseKind::Promote { revision, from_env, summary, behind }, chosen.unwrap_or(0), chosen.is_some())
            }
            Intent::Rollback { .. } => {
                let Some(plan) = pipeline::rollback_plan(&history) else {
                    return ctx.toast(
                        Level::Info,
                        format!("Nothing to roll back to: no earlier successful run of {name} with another revision in its last {} runs", history.len()),
                    );
                };
                let native = declaration
                    .is_v2()
                    .then(|| {
                        let stages: Vec<String> = declaration.rollback_stages().into_iter().map(str::to_string).collect();
                        let stage = declaration.deploy_stage().and_then(|d| stages.iter().position(|s| s == d));
                        NativeRollback { stage: stage.unwrap_or(stages.len().saturating_sub(1)), stages }
                    })
                    .filter(|n| !n.stages.is_empty());
                if native.is_none() && sources.is_empty() {
                    return ctx.toast(Level::Error, format!("{name} has no source action to re-run at an earlier revision"));
                }
                let wanted = plan.current.source_revisions.first().map(|r| r.action_name.clone()).unwrap_or_default();
                let chosen = pick(&wanted);
                let matched = chosen.is_some() || native.is_some();
                (ReleaseKind::Rollback { plan, target: 0, native }, chosen.unwrap_or(0), matched)
            }
        };
        let hooks = self.row_by_name(&name).map(|r| r.cfg.hooks.clone()).unwrap_or_default();
        // A stage rollback builds nothing: only the checks after a deploy apply.
        let native = matches!(kind, ReleaseKind::Rollback { native: Some(_), .. });
        let mut phases = vec![("pre-deploy", &hooks.pre_deploy), ("post-deploy", &hooks.post_deploy)];
        if native {
            phases.remove(0);
        }
        let hooks: Vec<(&'static str, String)> = phases
            .into_iter()
            .flat_map(|(phase, list)| list.iter().map(move |h| (phase, crate::util::show_invisible(&h.command))))
            .collect();
        let local = if hooks.is_empty() { None } else { ctx.local_config() };
        self.dialog = Some(Dialog::Release(Box::new(Release {
            name,
            kind,
            sources,
            source,
            source_matched: matched,
            input: TextInput::default(),
            needs_typing: is_production(&self.env_name),
            running: history.first().is_some_and(|e| Status::parse(&e.status).is_running()),
            hooks,
            local,
        })));
    }

    /// Runs a confirmed promote or rollback.
    fn release(&mut self, r: Release, ctx: &mut Ctx) {
        let label = r.label();
        if let ReleaseKind::Rollback { plan, target, native: Some(n) } = &r.kind {
            let (name, stage) = (r.name.clone(), n.stages[n.stage].clone());
            let Some(execution) = plan.targets.get(*target).map(|t| t.pipeline_execution_id.clone()) else { return };
            self.dialog = Some(Dialog::Busy(format!("Rolling back {stage} of {name}…")));
            let aws = ctx.env().aws.clone();
            ctx.spawn(move || {
                let result = cp::rollback_stage(&aws, &name, &stage, &execution);
                Msg::Started { name, what: Some(label), pinned: false, result }
            });
            return;
        }
        match r.pin() {
            Some(pin) => {
                self.pinned = Some((r.name.clone(), pin, label));
                self.begin_deploy(r.name, ctx);
            }
            None => ctx.toast(Level::Error, "No revision to deploy"),
        }
    }

    fn open_approve(&mut self, ctx: &mut Ctx) {
        let Some(row) = self.row() else { return };
        let approval = pipeline::approvals(&self.stages(row)).into_iter().next();
        match approval {
            Some(approval) => {
                let context = row
                    .latest
                    .as_ref()
                    .map(|l| {
                        let id = &l.pipeline_execution_id;
                        format!("execution {} · {}", &id[..8.min(id.len())], pipeline::revision(l, self.details_for(row)).one_line())
                    })
                    .unwrap_or_default();
                self.dialog = Some(Dialog::Approve {
                    name: row.name().to_string(),
                    approval,
                    context,
                    approve: true,
                    input: TextInput::default(),
                })
            }
            None => ctx.toast(Level::Info, format!("{} has no pending approval", row.name())),
        }
    }

    fn open_stop(&mut self, ctx: &mut Ctx) {
        let Some(row) = self.row() else { return };
        match &row.latest {
            Some(l) if Status::parse(&l.status) == Status::InProgress => {
                self.dialog = Some(Dialog::Stop {
                    name: row.name().to_string(),
                    execution: l.pipeline_execution_id.clone(),
                    abandon: false,
                    input: TextInput::default(),
                })
            }
            _ => ctx.toast(Level::Info, format!("{} is not running", row.name())),
        }
    }

    fn dialog_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(dialog) = &mut self.dialog else { return };
        match dialog {
            Dialog::Deploy { name, input, needs_typing, hooks, .. } => match key.code {
                KeyCode::Esc => self.dialog = None,
                KeyCode::Enter => {
                    if !hooks.is_empty() && !self.deploy_fits {
                        ctx.toast(Level::Warn, "Enlarge the terminal to review every hook before deploying");
                        return;
                    }
                    if *needs_typing && input.value.trim() != self.env_name {
                        ctx.toast(Level::Warn, format!("Type '{}' to confirm", self.env_name));
                        return;
                    }
                    let name = name.clone();
                    self.begin_deploy(name, ctx);
                }
                _ if *needs_typing => {
                    input.handle(key);
                }
                _ => {}
            },
            Dialog::Approve { name, approval, approve, input, .. } => match key.code {
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => *approve = !*approve,
                _ => match input.handle(key) {
                    InputEvent::Cancel => self.dialog = None,
                    InputEvent::Submit => {
                        let (name, a, ok) = (name.clone(), approval.clone(), *approve);
                        let summary = match input.value.trim() {
                            "" if ok => "Approved via dcli".to_string(),
                            "" => "Rejected via dcli".to_string(),
                            text => text.to_string(),
                        };
                        self.dialog = Some(Dialog::Busy(format!(
                            "{} {}…",
                            if ok { "Approving" } else { "Rejecting" },
                            a.action
                        )));
                        let aws = ctx.env().aws.clone();
                        ctx.spawn(move || {
                            let result = cp::put_approval(&aws, &name, &a.stage, &a.action, &a.token, ok, &summary);
                            Msg::Acted { name, what: if ok { "approved" } else { "rejected" }, result }
                        });
                    }
                    _ => {}
                },
            },
            Dialog::Stop { name, execution, abandon, input } => match key.code {
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => *abandon = !*abandon,
                _ => match input.handle(key) {
                    InputEvent::Cancel => self.dialog = None,
                    InputEvent::Submit => {
                        let (name, execution, abandon) = (name.clone(), execution.clone(), *abandon);
                        let reason = match input.value.trim() {
                            "" => "Stopped via dcli".to_string(),
                            text => text.to_string(),
                        };
                        self.dialog = Some(Dialog::Busy(format!("Stopping {name}…")));
                        let aws = ctx.env().aws.clone();
                        ctx.spawn(move || {
                            let result = cp::stop(&aws, &name, &execution, abandon, &reason);
                            Msg::Acted { name, what: "stopped", result }
                        });
                    }
                    _ => {}
                },
            },
            Dialog::Hooks { .. } | Dialog::Busy(_) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                    self.dialog = None;
                }
            }
            Dialog::Add(picker, names) => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.dialog = None,
                Outcome::Done(chosen) => {
                    let chosen = chosen.into_iter().map(|i| names[i].clone()).collect();
                    self.dialog = None;
                    self.add_pipelines(chosen, ctx);
                }
            },
            Dialog::Remove { name } => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    let name = name.clone();
                    self.dialog = None;
                    self.remove_pipeline(name, ctx);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => self.dialog = None,
                _ => {}
            },
            Dialog::Release(r) => match key.code {
                KeyCode::Esc => self.dialog = None,
                KeyCode::Tab | KeyCode::BackTab if r.sources.len() > 1 && r.revision().is_some() => {
                    let n = r.sources.len();
                    r.source = if key.code == KeyCode::Tab { (r.source + 1) % n } else { (r.source + n - 1) % n };
                    r.source_matched = true;
                }
                KeyCode::Up | KeyCode::Down => {
                    if let ReleaseKind::Rollback { plan, target, .. } = &mut r.kind {
                        *target = if key.code == KeyCode::Up {
                            target.saturating_sub(1)
                        } else {
                            (*target + 1).min(plan.targets.len().saturating_sub(1))
                        };
                    }
                }
                KeyCode::Left | KeyCode::Right => {
                    if let ReleaseKind::Rollback { native: Some(n), .. } = &mut r.kind {
                        let len = n.stages.len();
                        n.stage = if key.code == KeyCode::Left { (n.stage + len - 1) % len } else { (n.stage + 1) % len };
                    }
                }
                KeyCode::Enter => {
                    if !r.hooks.is_empty() && !self.deploy_fits {
                        ctx.toast(Level::Warn, "Enlarge the terminal to review every hook before deploying");
                        return;
                    }
                    if r.needs_typing && r.input.value.trim() != self.env_name {
                        ctx.toast(Level::Warn, format!("Type '{}' to confirm", self.env_name));
                        return;
                    }
                    if let Some(Dialog::Release(r)) = self.dialog.take() {
                        self.release(*r, ctx);
                    }
                }
                _ if r.needs_typing => {
                    r.input.handle(key);
                }
                _ => {}
            },
        }
    }

    /// Builds the failure report of the selected pipeline: `c` copies it (log excerpt), `C`
    /// saves it with the whole log to a file and copies the path.
    fn copy_report(&mut self, full: bool, ctx: &mut Ctx) {
        let Some(name) = self.row().map(|r| r.name().to_string()) else { return };
        if self.report_loading {
            return;
        }
        self.report_loading = true;
        ctx.toast(Level::Info, format!("Collecting the failure report of {name}…"));
        let env = ctx.env();
        ctx.spawn(move || {
            let result = crate::report::failure_report(&env.aws, &env.name, &name, full);
            Msg::Report { name, full, result }
        });
    }

    fn on_report(&mut self, name: String, full: bool, result: aws::Result<crate::report::Report>, ctx: &mut Ctx) {
        self.report_loading = false;
        let report = match result {
            Ok(r) => r,
            Err(e) => return self.report(ctx, &format!("Could not build the report of {name}"), e),
        };
        let Some(execution) = &report.execution else {
            return ctx.toast(Level::Info, report.markdown.trim().to_string());
        };
        let lines = match (report.log_lines, full) {
            (0, _) => "no build log".to_string(),
            (n, true) => format!("{n} log lines"),
            (n, false) => format!("{} of {n} log lines", report.shown_lines),
        };
        if !full && crate::platform::copy_to_clipboard(&report.markdown) {
            ctx.toast(Level::Success, format!("Copied the failure report of {name} ({lines}). Paste it into your AI agent."));
            return;
        }
        // The whole log (or a failed copy) goes to a file; agents can read it from there.
        match crate::platform::write_temp_file(&format!("{name}-{}", &execution[..8.min(execution.len())]), &report.markdown) {
            Ok(path) => {
                // The temp folder path is long: show the file name, copy the full path.
                let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                let text = if crate::platform::copy_to_clipboard(&path.display().to_string()) {
                    format!("Saved the report of {name} ({lines}) as {file}. Path copied: give it to your AI agent.")
                } else {
                    format!("Saved the report of {name} ({lines}) to {}", path.display())
                };
                ctx.toast(Level::Success, text);
            }
            Err(e) => ctx.toast(Level::Error, format!("Could not save the report: {e}")),
        }
    }

    fn open_console(&self, ctx: &mut Ctx) {
        let Some(row) = self.row() else { return };
        let url = aws::console_url(
            &self.region,
            &format!("codesuite/codepipeline/pipelines/{}/view?region={}", row.name(), self.region),
        );
        if !crate::platform::open_url(&url) {
            ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
        }
    }

    fn open_add(&mut self, ctx: &mut Ctx) {
        self.dialog = Some(Dialog::Busy(format!("Listing pipelines in {}…", self.region)));
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Discovered(cp::list_pipelines(&aws)));
    }

    fn on_discovered(&mut self, result: aws::Result<Vec<cp::PipelineSummary>>, ctx: &mut Ctx) {
        if !matches!(self.dialog, Some(Dialog::Busy(_))) {
            return; // closed while loading
        }
        self.dialog = None;
        let all = match result {
            Ok(all) => all,
            Err(e) => return self.report(ctx, "Could not list pipelines", e),
        };
        let available: Vec<cp::PipelineSummary> =
            all.iter().filter(|p| self.row_by_name(&p.name).is_none()).cloned().collect();
        if available.is_empty() {
            let text = match all.len() {
                0 => format!("No pipelines in this account ({})", self.region),
                n => format!("All {n} pipelines in this account are already in {}", self.env_name),
            };
            ctx.toast(Level::Info, text);
            return;
        }
        let items = available
            .iter()
            .map(|p| Item::new(p.name.as_str(), p.updated.map(|t| format!("updated {}", fmt_ago(t))).unwrap_or_default()))
            .collect();
        let title = format!("Add pipelines to {}", self.env_name);
        let names = available.into_iter().map(|p| p.name).collect();
        self.dialog = Some(Dialog::Add(Picker::new(title, "add", items, true), names));
    }

    fn add_pipelines(&mut self, names: Vec<String>, ctx: &mut Ctx) {
        let (path, added) = match ctx.add_pipelines(&names) {
            Ok(out) => out,
            Err(e) => return ctx.toast(Level::Error, format!("Could not save the config: {e:#}")),
        };
        let env = ctx.env();
        let first_new = self.rows.len();
        for cfg in env.pipelines.iter().filter(|p| names.contains(&p.name)) {
            if self.row_by_name(&cfg.name).is_none() {
                self.rows.push(Row::new(cfg.clone()));
            }
        }
        let plural = if added == 1 { "" } else { "s" };
        ctx.toast(Level::Success, format!("Added {added} pipeline{plural} to {} · {}", self.env_name, display_path(&path)));
        self.last_poll = None;
        self.select(first_new, ctx);
    }

    fn remove_pipeline(&mut self, name: String, ctx: &mut Ctx) {
        let path = match ctx.remove_pipeline(&name) {
            Ok(path) => path,
            Err(e) => return ctx.toast(Level::Error, format!("Could not save the config: {e:#}")),
        };
        self.rows.retain(|r| r.name() != name);
        self.details.remove(&name);
        self.history.remove(&name);
        self.last_seen.remove(&name);
        ctx.toast(Level::Success, format!("Removed {name} from {} · {}", self.env_name, display_path(&path)));
        if !self.rows.is_empty() {
            let index = self.selected.min(self.rows.len() - 1);
            self.selected = usize::MAX; // force a fresh selection
            self.select(index, ctx);
        }
    }

    // ---- results ----------------------------------------------------------------

    fn on_polled(
        &mut self,
        name: String,
        result: aws::Result<(cp::PipelineState, Option<cp::ExecutionSummary>)>,
        ctx: &mut Ctx,
    ) {
        self.polling = self.polling.saturating_sub(1);
        let Some(index) = self.rows.iter().position(|r| r.name() == name) else { return };
        let row = &mut self.rows[index];
        row.loading = false;
        match result {
            Ok((state, latest)) => {
                row.state = Some(state);
                row.latest = latest;
                row.error = None;
            }
            Err(e) => {
                row.error = Some(e);
                return;
            }
        }
        let changed = self.detect_changes(index, ctx);
        if index == self.selected {
            self.ensure_details(ctx);
            self.ensure_averages(ctx);
            if changed && self.lower == Lower::History {
                self.load_history(ctx);
            }
        }
    }

    /// Notifications on finished executions and new approvals; post-deploy hooks.
    /// Returns true when the latest execution or its status changed.
    fn detect_changes(&mut self, index: usize, ctx: &mut Ctx) -> bool {
        let row = &self.rows[index];
        let Some(latest) = row.latest.clone() else { return false };
        let name = row.name().to_string();
        let status = Status::parse(&latest.status);
        let exec = latest.pipeline_execution_id.clone();
        let approvals = pipeline::approvals(&self.stages(row));
        let post_hooks = row.cfg.hooks.post_deploy.clone();
        let seen_before = self.last_seen.contains_key(&name);

        if let Some((prev_exec, prev_status)) = self.last_seen.get(&name)
            && *prev_exec == exec
            && prev_status.is_running()
            && !status.is_running()
        {
            let level = if status == Status::Succeeded { Level::Success } else { Level::Error };
            let took = latest
                .start_time
                .zip(latest.last_update_time)
                .map(|(s, e)| format!(" in {}", fmt_duration(e - s)))
                .unwrap_or_default();
            ctx.notify(
                level,
                &format!("{name} {}", status.label().to_lowercase()),
                &format!("{} · {}{took}", self.env_name, &exec[..8.min(exec.len())]),
            );
        }
        for a in &approvals {
            if self.seen_approvals.insert(a.token.clone()) && seen_before {
                ctx.notify(Level::Warn, &format!("{name} needs approval"), &format!("{} / {}", a.stage, a.action));
            }
        }
        if status == Status::Succeeded && self.started_by_us.remove(&exec).is_some() && !post_hooks.is_empty() {
            ctx.toast(Level::Info, format!("Running post-deploy hooks for {name}"));
            run_hooks(ctx, name.clone(), post_hooks, false);
        } else if !status.is_running() {
            self.started_by_us.remove(&exec);
        }
        let previous = self.last_seen.insert(name, (exec.clone(), status));
        previous.is_none_or(|(e, s)| e != exec || s != status)
    }

    fn handle(&mut self, msg: Msg, ctx: &mut Ctx) {
        match msg {
            Msg::Polled { name, result } => self.on_polled(name, result, ctx),
            Msg::Details { name, execution, result } => {
                self.details_loading.remove(&name);
                if let Ok(d) = result {
                    self.details.insert(name, (execution, d));
                }
            }
            Msg::Averages { name, result } => {
                self.averages_loading.remove(&name);
                if let Ok(avg) = result {
                    self.averages.insert(name, (Instant::now(), avg));
                }
            }
            Msg::History { name, result } => {
                self.history_loading.remove(&name);
                self.history.insert(name, result);
            }
            Msg::Logs { build_id, result } => {
                let Some(panel) = self.logs.as_mut().filter(|p| p.build_id == build_id) else { return };
                panel.in_flight = false;
                match result {
                    Ok(chunk) => {
                        let got_lines = !chunk.lines.is_empty();
                        panel.stream = chunk.stream.or(panel.stream.take());
                        panel.view.extend(chunk.lines);
                        panel.token = chunk.token;
                        panel.phase = chunk.phase;
                        panel.state = if chunk.build_done && !got_lines && panel.stream.is_some() {
                            LogsState::Done
                        } else {
                            LogsState::Live
                        };
                    }
                    Err(e) => panel.state = LogsState::Error(e.short()),
                }
            }
            Msg::HookLine { name, line } => {
                if let Some(Dialog::Hooks { name: n, lines, .. }) = &mut self.dialog
                    && *n == name
                {
                    lines.push(line);
                    if lines.len() > 500 {
                        lines.remove(0);
                    }
                }
            }
            Msg::HooksDone { name, pre_deploy: true, result: Ok(()) } => {
                ctx.toast(Level::Success, format!("Pre-deploy hooks passed for {name}"));
                if matches!(&self.dialog, Some(Dialog::Hooks { name: n, .. }) if *n == name) || self.dialog.is_none() {
                    self.start_pipeline(name, ctx);
                }
            }
            Msg::HooksDone { name, pre_deploy: true, result: Err(e) } => {
                self.pinned = None;
                match &mut self.dialog {
                    Some(Dialog::Hooks { name: n, state, .. }) if *n == name => *state = HookState::Failed(e),
                    _ => ctx.toast(Level::Error, format!("Pre-deploy hook failed for {name}: {e}")),
                }
            }
            Msg::HooksDone { name, pre_deploy: false, result } => match result {
                Ok(()) => ctx.toast(Level::Success, format!("Post-deploy hooks passed for {name}")),
                Err(e) => ctx.notify(Level::Error, &format!("{name}: post-deploy hook failed"), &e),
            },
            Msg::Started { name, what, pinned, result } => {
                self.dialog = None;
                match result {
                    Ok(exec) => {
                        let at = what.map(|w| format!(" · {w}")).unwrap_or_default();
                        ctx.toast(Level::Success, format!("Started {name}{at} · {}", &exec[..8.min(exec.len())]));
                        self.started_by_us.insert(exec, name);
                        self.last_poll = None;
                    }
                    Err(e) => {
                        let what = what.map(|w| format!(" ({w})")).unwrap_or_default();
                        let hint = if pinned { pipeline::pin_hint(&e.short()) } else { "" };
                        ctx.toast(Level::Error, format!("Could not start {name}{what}: {}{hint}", e.short()));
                    }
                }
            }
            Msg::Planned { intent, result } => self.on_planned(intent, result, ctx),
            Msg::Acted { name, what, result } => {
                self.dialog = None;
                match result {
                    Ok(()) => {
                        ctx.toast(Level::Success, format!("{name} {what}"));
                        self.last_poll = None;
                    }
                    Err(e) => self.report(ctx, &format!("{name}: action failed"), e),
                }
            }
            Msg::Discovered(result) => self.on_discovered(result, ctx),
            Msg::Report { name, full, result } => self.on_report(name, full, result, ctx),
        }
    }

    fn report(&self, ctx: &mut Ctx, what: &str, e: AwsError) {
        ctx.toast(Level::Error, format!("{what}: {}", e.short()));
    }

    // ---- rendering --------------------------------------------------------------

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let inner_width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| {
                let status = self.row_status(row);
                let (symbol, style) = match (status, &row.error) {
                    (_, Some(_)) => ("!", Style::new().fg(theme::ERR)),
                    (Some(s), None) => (theme::status_symbol(s, view.tick), theme::status_style(s)),
                    (None, None) if row.loading => (theme::spinner(view.tick), theme::muted()),
                    (None, None) => ("○", theme::muted()),
                };
                let right = match (&row.latest, status) {
                    (Some(l), Some(s)) if s.is_running() => {
                        l.start_time.map(|t| fmt_duration(view.now - t)).unwrap_or_default()
                    }
                    (Some(l), _) => l.last_update_time.map(fmt_ago).unwrap_or_default(),
                    (None, _) if row.error.is_some() => "error".into(),
                    _ => String::new(),
                };
                let name_width = inner_width.saturating_sub(right.chars().count() + 3);
                let name = ellipsize(row.name(), name_width);
                let pad = inner_width.saturating_sub(name.chars().count() + right.chars().count() + 2);
                let subtitle = row
                    .cfg
                    .description
                    .clone()
                    .or_else(|| row.latest.as_ref().map(|l| pipeline::revision(l, &[]).one_line()))
                    .unwrap_or_default();
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(format!("{symbol} "), style),
                        Span::styled(name, theme::bold()),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(right, theme::muted()),
                    ]),
                    Line::styled(format!("  {}", ellipsize(&subtitle, inner_width.saturating_sub(2))), theme::muted()),
                ])
            })
            .collect();
        let title = format!(" Pipelines · {} ", self.rows.len());
        let list = List::new(items)
            .block(widgets::panel(title, self.dialog.is_none()))
            .highlight_style(theme::selected());
        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(row) = self.row() else { return };
        let status = self.row_status(row);
        let mut block = widgets::panel(Line::from(format!(" {} ", row.name())).bold(), false);
        if let Some(s) = status {
            let elapsed = row
                .latest
                .as_ref()
                .and_then(|l| Some(l.last_update_time.filter(|_| !s.is_running()).unwrap_or(view.now) - l.start_time?))
                .map(|d| format!(" · {}", fmt_duration(d)))
                .unwrap_or_default();
            block = block.title_top(
                Line::from(vec![
                    Span::styled(format!(" {} {}", theme::status_symbol(s, view.tick), s.label()), theme::status_style(s).bold()),
                    Span::styled(format!("{elapsed} "), theme::muted()),
                ])
                .right_aligned(),
            );
        }

        let mut lines: Vec<Line> = Vec::new();
        if let Some(e) = &row.error {
            let color = if e.is_login_required() { theme::WARN } else { theme::ERR };
            lines.push(Line::raw(""));
            lines.push(Line::styled(format!(" {}", e.short()), Style::new().fg(color)));
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
            return;
        }
        let (Some(_), Some(latest)) = (&row.state, &row.latest) else {
            let text = if row.loading || row.state.is_none() {
                Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" Loading…")])
            } else {
                Line::styled("No executions yet — press d to deploy", theme::muted())
            };
            f.render_widget(widgets::placeholder(text).block(block), area);
            return;
        };

        if let Some(d) = &row.cfg.description {
            lines.push(Line::styled(format!(" {d}"), theme::muted()));
        }
        let revision = pipeline::revision(latest, self.details_for(row));
        let short_exec = &latest.pipeline_execution_id[..8.min(latest.pipeline_execution_id.len())];
        lines.push(Line::from(vec![
            Span::styled(" ▸ ", theme::accent()),
            Span::raw(short_exec.to_string()),
            Span::styled(
                format!(
                    " · {} · started {}",
                    pipeline::trigger_label(latest),
                    latest.start_time.map(fmt_ago).unwrap_or_else(|| "—".into())
                ),
                theme::muted(),
            ),
        ]));
        if revision.commit.is_some() || revision.message.is_some() {
            let mut spans = vec![Span::raw("   ")];
            if let Some(c) = &revision.commit {
                spans.push(Span::styled(format!("{c} "), Style::new().fg(theme::WARN)));
            }
            if let Some(m) = &revision.message {
                spans.push(Span::raw(format!("\"{}\" ", ellipsize(m, 70))));
            }
            if let Some(b) = &revision.branch {
                spans.push(Span::styled(format!("({b}) "), Style::new().fg(theme::APPROVAL)));
            }
            if let Some(r) = &revision.repo {
                spans.push(Span::styled(r.clone(), theme::muted()));
            }
            lines.push(Line::from(spans));
        }
        if let Some(text) = self.since_success(row) {
            lines.push(Line::from(vec![Span::styled(format!("    {text}"), theme::muted())]));
        }
        lines.push(Line::raw(""));

        let stages = self.stages(row);
        let cursor = self.current_stage(&stages);
        let mut chain = vec![Span::raw(" ")];
        for (i, s) in stages.iter().enumerate() {
            if i > 0 {
                chain.push(Span::styled(" ─ ", theme::muted()));
            }
            let mut style = theme::status_style(s.status);
            if i == cursor {
                style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
            }
            chain.push(Span::styled(format!("{} {}", theme::status_symbol(s.status, view.tick), s.name), style));
        }
        lines.push(Line::from(chain));
        lines.push(Line::raw(""));

        let name_w = stages.iter().map(|s| s.name.chars().count()).max().unwrap_or(5).clamp(5, 22);
        let detail_w = (area.width as usize).saturating_sub(name_w + 34).max(10);
        let averages = self.averages.get(row.name()).map(|(_, a)| a);
        for (i, s) in stages.iter().enumerate() {
            let marker = if i == cursor { "▶ " } else { "  " };
            let time = s.elapsed(view.now).map(fmt_duration).unwrap_or_default();
            let mut spans = vec![
                Span::styled(format!(" {marker}"), theme::accent()),
                Span::styled(format!("{:<name_w$} ", ellipsize(&s.name, name_w)), if i == cursor { theme::bold() } else { Style::new() }),
                Span::styled(
                    format!("{} {:<14}", theme::status_symbol(s.status, view.tick), s.status.label()),
                    theme::status_style(s.status),
                ),
                Span::styled(format!("{time:>8}  "), theme::muted()),
            ];
            spans.extend(stage_detail(s, averages.and_then(|a| a.get(&s.name)).copied(), view, detail_w));
            lines.push(Line::from(spans));
        }
        if let Some((text, color)) = stages.get(cursor).and_then(stage_note) {
            lines.push(Line::raw(""));
            lines.push(Line::styled(format!(" {text}"), Style::new().fg(color)));
        }
        if row.exec_status() == Some(Status::Failed) {
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled("c", theme::key()),
                Span::styled(" copy a failure report for an AI agent  ", theme::muted()),
                Span::styled("C", theme::key()),
                Span::styled(" save it with the full log  ", theme::muted()),
                Span::styled("l", theme::key()),
                Span::styled(" build log", theme::muted()),
            ]));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
    }

    /// Height of the top panel: header, stage chain, one row per stage, plus the note.
    fn detail_height(&self, width: u16) -> u16 {
        let Some(row) = self.row() else { return 0 };
        let stages = self.stages(row);
        let note = stages
            .get(self.current_stage(&stages))
            .and_then(stage_note)
            .map_or(0, |(text, _)| 2 + text.chars().count() as u16 / width.saturating_sub(4).max(1));
        let report_hint = u16::from(row.exec_status() == Some(Status::Failed));
        let since = u16::from(self.since_success(row).is_some());
        stages.len() as u16 + 9 + note + report_hint + since
    }

    fn render_logs(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.logs_area = area;
        let Some(panel) = &self.logs else { return };
        let (state_text, state_style) = match &panel.state {
            LogsState::Loading => (format!("{} loading", theme::spinner(view.tick)), theme::accent()),
            LogsState::Live if !panel.view.following() => (
                format!("↑ {} lines · End to follow", panel.view.lines_above_end().unwrap_or(0)),
                theme::muted(),
            ),
            LogsState::Live => (
                format!("{} following{}", theme::spinner(view.tick), panel.phase.as_deref().map(|p| format!(" · {p}")).unwrap_or_default()),
                Style::new().fg(theme::RUNNING),
            ),
            LogsState::Done => ("build finished".to_string(), theme::muted()),
            LogsState::Error(e) => (ellipsize(e, 60), Style::new().fg(theme::ERR)),
        };
        let block = widgets::panel(format!(" Logs · {} / {} ", panel.stage, panel.action), false)
            .title_top(Line::styled(format!(" {state_text} "), state_style).right_aligned());
        let empty = match panel.state {
            LogsState::Loading | LogsState::Live => "Waiting for log output…",
            _ => "No log output",
        };
        if let Some(panel) = &mut self.logs {
            panel.view.render_panel(f, area, block, empty);
        }
    }

    fn render_history(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(row) = self.row() else { return };
        let block = widgets::panel(" History ", false);
        let lines: Vec<Line> = match self.history.get(row.name()) {
            None => vec![Line::from(vec![
                Span::styled(format!(" {} ", theme::spinner(view.tick)), theme::accent()),
                Span::raw("Loading…"),
            ])],
            Some(Err(e)) => vec![Line::styled(format!(" {}", e.short()), Style::new().fg(theme::ERR))],
            Some(Ok(execs)) if execs.is_empty() => vec![Line::styled(" No executions", theme::muted())],
            Some(Ok(execs)) => {
                let last_ok = pipeline::last_success(execs, 0);
                execs
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let s = Status::parse(&e.status);
                        let duration =
                            e.start_time.zip(e.last_update_time).map(|(a, b)| fmt_duration(b - a)).unwrap_or_default();
                        let rev = pipeline::revision(e, &[]);
                        let marker = if Some(i) == last_ok { "  ← last success" } else { "" };
                        Line::from(vec![
                            Span::styled(format!(" {} ", theme::status_symbol(s, view.tick)), theme::status_style(s)),
                            Span::raw(format!("{:<9}", &e.pipeline_execution_id[..8.min(e.pipeline_execution_id.len())])),
                            Span::styled(format!("{:<12}", s.label()), theme::status_style(s)),
                            Span::styled(format!("{:<9}", pipeline::trigger_label(e)), theme::muted()),
                            Span::styled(format!("{:<10}", e.start_time.map(fmt_ago).unwrap_or_default()), theme::muted()),
                            Span::raw(format!("{duration:<9}")),
                            Span::styled(rev.commit.unwrap_or_default(), Style::new().fg(theme::WARN)),
                            Span::raw(format!(" {}", ellipsize(&rev.message.unwrap_or_default(), 60))),
                            Span::styled(marker, Style::new().fg(theme::OK)),
                        ])
                    })
                    .collect()
            }
        };
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_dialog(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some(Dialog::Add(picker, _)) = &mut self.dialog {
            let rect = widgets::popup_area(area, 84, picker.height(area.height.saturating_sub(4)));
            picker.render(f, rect, theme::ACCENT);
            return;
        }
        let Some(dialog) = &self.dialog else { return };
        match dialog {
            Dialog::Deploy { name, input, needs_typing, running, hooks, local } => {
                let env_color = theme::env_color(&self.env_name);
                let mut lines = vec![
                    Line::raw(""),
                    Line::from(vec![
                        Span::raw("Start "),
                        Span::styled(name.clone(), theme::bold()),
                        Span::raw(" in "),
                        Span::styled(self.env_name.to_uppercase(), Style::new().fg(env_color).bold()),
                        Span::raw("?"),
                    ]),
                    Line::raw(""),
                ];
                if *running {
                    lines.push(Line::styled(
                        "! It is running now; a new execution will supersede it.",
                        Style::new().fg(theme::WARN),
                    ));
                }
                if !hooks.is_empty() {
                    // Every command in full: they run on this machine once confirmed.
                    lines.push(Line::styled("These hooks run on your machine:", theme::muted()));
                    for (phase, command) in hooks {
                        lines.push(Line::from(vec![
                            Span::styled(format!("{phase}: "), theme::muted()),
                            Span::raw(command.replace('\n', " ⏎ ")),
                        ]));
                    }
                    if let Some(path) = local {
                        lines.push(Line::styled(
                            format!("! Defined in {path} (current folder)"),
                            Style::new().fg(theme::WARN),
                        ));
                    }
                }
                let mut cursor = None;
                if *needs_typing {
                    if *running || !hooks.is_empty() {
                        lines.push(Line::raw(""));
                    }
                    lines.push(Line::raw(format!("Type '{}' to confirm:", self.env_name)));
                    let (line, col) = input.line("› ", "");
                    cursor = Some((lines.len() as u16, col));
                    lines.push(line);
                }
                lines.push(Line::raw(""));
                lines.push(widgets::hints_line(&[("Enter", "deploy"), ("Esc", "cancel")]));
                let height = widgets::wrapped_height(&lines, 60) + 2;
                self.deploy_fits = height <= area.height.saturating_sub(2);
                let rect = widgets::popup_area(area, 64, height);
                widgets::popup(f, rect, "Deploy", env_color, Text::from(lines));
                if let Some((row, col)) = cursor {
                    f.set_cursor_position((rect.x + 1 + col, rect.y + 1 + row));
                }
            }
            Dialog::Approve { name, approval, context, approve, input } => {
                let choice = |label: &'static str, on: bool, color| {
                    if on {
                        Span::styled(format!(" {label} "), Style::new().fg(ratatui::style::Color::Black).bg(color).bold())
                    } else {
                        Span::styled(format!(" {label} "), theme::muted())
                    }
                };
                let mut lines = vec![
                    Line::raw(""),
                    Line::from(vec![
                        Span::styled(name.clone(), theme::bold()),
                        Span::styled(format!(" · {} / {}", approval.stage, approval.action), theme::muted()),
                    ]),
                    Line::styled(ellipsize(context, 66), theme::muted()),
                ];
                if let Some(s) = &approval.summary {
                    lines.push(Line::raw(""));
                    lines.push(Line::raw(s.clone()));
                }
                if let Some(u) = &approval.url {
                    lines.push(Line::styled(u.clone(), theme::accent()));
                }
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    choice("✓ Approve", *approve, theme::OK),
                    Span::raw("  "),
                    choice("✗ Reject", !*approve, theme::ERR),
                    Span::styled("   Tab to switch", theme::muted()),
                ]));
                lines.push(Line::raw(""));
                let (line, col) = input.line("Comment › ", "optional");
                let input_row = widgets::wrapped_height(&lines, 68);
                lines.push(line);
                lines.push(Line::raw(""));
                lines.push(widgets::hints_line(&[("Enter", "submit"), ("Esc", "cancel")]));
                let rect = widgets::popup_area(area, 72, widgets::wrapped_height(&lines, 68) + 2);
                widgets::popup(f, rect, "Manual approval", theme::APPROVAL, Text::from(lines));
                f.set_cursor_position((rect.x + 1 + col, rect.y + 1 + input_row));
            }
            Dialog::Stop { name, execution, abandon, input } => {
                let mode = if *abandon {
                    Span::styled("Abandon: stop now, without waiting for running actions", Style::new().fg(theme::ERR))
                } else {
                    Span::styled("Stop and wait: let running actions finish", Style::new().fg(theme::WARN))
                };
                let mut lines = vec![
                    Line::raw(""),
                    Line::from(vec![
                        Span::raw("Stop "),
                        Span::styled(name.clone(), theme::bold()),
                        Span::styled(format!(" · {}", &execution[..8.min(execution.len())]), theme::muted()),
                    ]),
                    Line::raw(""),
                    Line::from(vec![mode, Span::styled("   Tab to switch", theme::muted())]),
                    Line::raw(""),
                ];
                let (line, col) = input.line("Reason › ", "optional");
                let input_row = lines.len() as u16;
                lines.push(line);
                lines.push(Line::raw(""));
                lines.push(widgets::hints_line(&[("Enter", "stop"), ("Esc", "cancel")]));
                let rect = widgets::popup_area(area, 72, lines.len() as u16 + 2);
                widgets::popup(f, rect, "Stop execution", theme::WARN, Text::from(lines));
                f.set_cursor_position((rect.x + 1 + col, rect.y + 1 + input_row));
            }
            Dialog::Hooks { name, lines, state } => {
                let height = area.height.saturating_sub(6).min(24);
                let mut text = vec![match state {
                    HookState::Running => Line::from(vec![
                        Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()),
                        Span::raw(format!("Running pre-deploy hooks for {name}…")),
                    ]),
                    HookState::Failed(e) => Line::styled(
                        format!("✗ Pre-deploy hook failed ({e}). Deployment aborted."),
                        Style::new().fg(theme::ERR),
                    ),
                }];
                text.push(Line::raw(""));
                let room = height.saturating_sub(5) as usize;
                for l in lines.iter().skip(lines.len().saturating_sub(room)) {
                    let style = if l.starts_with('✓') {
                        Style::new().fg(theme::OK)
                    } else if l.starts_with('✗') {
                        Style::new().fg(theme::ERR)
                    } else if l.starts_with('▸') {
                        theme::accent()
                    } else {
                        theme::muted()
                    };
                    text.push(Line::styled(l.clone(), style));
                }
                let color = if matches!(state, HookState::Failed(_)) { theme::ERR } else { theme::ACCENT };
                widgets::popup(f, widgets::popup_area(area, 100, height), "Hooks", color, Text::from(text));
            }
            Dialog::Busy(text) => {
                let body = Text::from(vec![
                    Line::raw(""),
                    Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw(text.clone())]),
                ]);
                widgets::popup(f, widgets::popup_area(area, 50, 5), "Working", theme::ACCENT, body);
            }
            Dialog::Add(..) => {}
            Dialog::Remove { name } => {
                let lines = vec![
                    Line::raw(""),
                    Line::from(vec![
                        Span::raw("Remove "),
                        Span::styled(name.clone(), theme::bold()),
                        Span::raw(" from "),
                        Span::styled(self.env_name.to_uppercase(), Style::new().fg(theme::env_color(&self.env_name)).bold()),
                        Span::raw(" in your config?"),
                    ]),
                    Line::styled("The pipeline in AWS is not touched; add it back with n.", theme::muted()),
                    Line::raw(""),
                    widgets::hints_line(&[("Enter", "remove"), ("Esc", "cancel")]),
                ];
                let rect = widgets::popup_area(area, 72, lines.len() as u16 + 2);
                widgets::popup(f, rect, "Remove from config", theme::WARN, Text::from(lines));
            }
            Dialog::Release(r) => {
                let (title, lines, cursor) = self.release_lines(r);
                let height = widgets::wrapped_height(&lines, 76) + 2;
                self.deploy_fits = height <= area.height.saturating_sub(2);
                let rect = widgets::popup_area(area, 80, height);
                widgets::popup(f, rect, title, theme::env_color(&self.env_name), Text::from(lines));
                if let Some((row, col)) = cursor {
                    f.set_cursor_position((rect.x + 1 + col, rect.y + 1 + row));
                }
            }
        }
    }

    /// The promote / rollback dialog: title, lines, and where the confirmation cursor is.
    fn release_lines(&self, r: &Release) -> (&'static str, Vec<Line<'static>>, Option<(u16, u16)>) {
        let kw = 9;
        let env = Span::styled(self.env_name.to_uppercase(), Style::new().fg(theme::env_color(&self.env_name)).bold());
        let describe = |e: &cp::ExecutionSummary| {
            let when = e.last_update_time.or(e.start_time).map(|t| format!(" · deployed {}", fmt_ago(t))).unwrap_or_default();
            format!("{}{when}", pipeline::revision(e, &[]).one_line())
        };
        let mut lines = vec![Line::raw("")];
        let title = match &r.kind {
            ReleaseKind::Promote { from_env, summary, behind, .. } => {
                lines.push(Line::from(vec![
                    Span::raw("Deploy to "),
                    Span::styled(r.name.clone(), theme::bold()),
                    Span::raw(" in "),
                    env,
                    Span::raw(format!(" the revision {from_env} runs:")),
                ]));
                lines.push(Line::raw(""));
                lines.push(widgets::kv("Revision", Span::styled(summary.clone(), theme::bold()), kw));
                let moves = match behind {
                    Some(n) => format!("{n} deploy{} forward", if *n == 1 { "" } else { "s" }),
                    None => format!("replaces a revision that is not in {from_env}'s recent history (a hotfix?)"),
                };
                lines.push(widgets::kv("Moves", moves, kw));
                "Promote"
            }
            ReleaseKind::Rollback { plan, target, native } => {
                lines.push(Line::from(vec![Span::raw("Roll "), Span::styled(r.name.clone(), theme::bold()), Span::raw(" back in "), env]));
                lines.push(Line::raw(""));
                lines.push(widgets::kv("Now", describe(&plan.current), kw));
                for (i, t) in plan.targets.iter().enumerate() {
                    let chosen = i == *target;
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:<kw$} ", if i == 0 { "Back to" } else { "" }), theme::muted()),
                        Span::styled(if chosen { "▶ " } else { "  " }, theme::accent()),
                        Span::styled(describe(t), if chosen { theme::bold() } else { theme::muted() }),
                    ]));
                }
                if let Some(n) = native {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:<kw$} ", "Stage"), theme::muted()),
                        Span::styled(format!("◀ {} ▶", n.stages[n.stage]), theme::bold()),
                    ]));
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(
                        format!(
                            "Method: CodePipeline's stage rollback (a V2 pipeline). {} gets again what that execution \
                             deployed there; nothing is rebuilt and the other stages do not run.",
                            n.stages[n.stage]
                        ),
                        theme::muted(),
                    ));
                } else {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(
                        "Method: run the whole pipeline again at that revision (a V1 pipeline cannot roll a stage back): \
                         build, tests and approvals run again.",
                        theme::muted(),
                    ));
                }
                "Rollback"
            }
        };
        if r.revision().is_some()
            && let Some(source) = r.sources.get(r.source)
        {
            let c = &source.configuration;
            let place: Vec<&str> = ["FullRepositoryId", "RepositoryName", "S3Bucket", "S3ObjectKey", "BranchName", "ImageTag"]
                .iter()
                .filter_map(|k| c.get(*k).map(String::as_str))
                .collect();
            let mut text = format!("{} ({}", source.name, source.action_type_id.provider);
            if !place.is_empty() {
                text.push_str(&format!(" · {}", place.join(" · ")));
            }
            text.push(')');
            let mut spans = vec![Span::styled(format!("{:<kw$} ", "Source"), theme::muted()), Span::raw(text)];
            if r.sources.len() > 1 {
                spans.push(Span::styled("   Tab: another source", theme::muted()));
            }
            lines.push(Line::from(spans));
            if !r.source_matched {
                lines.push(Line::styled(
                    "! It has several source actions: check which one takes this revision (Tab).",
                    Style::new().fg(theme::WARN),
                ));
            }
        }
        if r.running {
            let text = if r.revision().is_some() {
                "! It is running now; this execution will supersede it."
            } else {
                "! It is running now: CodePipeline may refuse to roll back a stage that is running."
            };
            lines.push(Line::styled(text, Style::new().fg(theme::WARN)));
        }
        if !r.hooks.is_empty() {
            lines.push(Line::raw(""));
            // Every command in full: they run on this machine once confirmed.
            lines.push(Line::styled("These hooks run on your machine:", theme::muted()));
            for (phase, command) in &r.hooks {
                lines.push(Line::from(vec![Span::styled(format!("{phase}: "), theme::muted()), Span::raw(command.replace('\n', " ⏎ "))]));
            }
            if let Some(path) = &r.local {
                lines.push(Line::styled(format!("! Defined in {path} (current folder)"), Style::new().fg(theme::WARN)));
            }
        }
        let mut cursor = None;
        if r.needs_typing {
            lines.push(Line::raw(""));
            lines.push(Line::raw(format!("Type '{}' to confirm:", self.env_name)));
            let (line, col) = r.input.line("› ", "");
            cursor = Some((widgets::wrapped_height(&lines, 76), col));
            lines.push(line);
        }
        lines.push(Line::raw(""));
        lines.push(widgets::hints_line(&release_hints(r)));
        (title, lines, cursor)
    }
}

fn release_hints(r: &Release) -> Vec<(&'static str, &'static str)> {
    let mut hints = vec![("Enter", if matches!(r.kind, ReleaseKind::Promote { .. }) { "promote" } else { "roll back" })];
    if let ReleaseKind::Rollback { plan, native, .. } = &r.kind {
        if plan.targets.len() > 1 {
            hints.push(("↑↓", "revision"));
        }
        if native.as_ref().is_some_and(|n| n.stages.len() > 1) {
            hints.push(("←→", "stage"));
        }
    }
    if r.sources.len() > 1 && r.revision().is_some() {
        hints.push(("Tab", "source"));
    }
    hints.push(("Esc", "cancel"));
    hints
}

/// Last column of a stage row: progress, approval prompt, error or provider.
fn stage_detail(s: &StageView, average: Option<f64>, view: &View, width: usize) -> Vec<Span<'static>> {
    match s.status {
        Status::InProgress => {
            let elapsed = s.elapsed(view.now).unwrap_or(0.0);
            let bar_w = 16.min(width.saturating_sub(4)).max(4);
            if let Some(pct) = s.actions.iter().find_map(|a| a.percent) {
                let mut spans = widgets::progress_bar(f64::from(pct) / 100.0, bar_w, theme::RUNNING);
                spans.push(Span::styled(format!(" {pct}%"), theme::muted()));
                return spans;
            }
            match average {
                Some(avg) if avg > 0.0 && elapsed <= avg => {
                    let mut spans = widgets::progress_bar(elapsed / avg, bar_w, theme::RUNNING);
                    spans.push(Span::styled(
                        format!(" ~{}% · ≈{} left", ((elapsed / avg) * 100.0) as u32, fmt_duration(avg - elapsed)),
                        theme::muted(),
                    ));
                    spans
                }
                Some(avg) if avg > 0.0 => {
                    let mut spans = widgets::progress_bar(1.0, bar_w, theme::WARN);
                    spans.push(Span::styled(format!(" slower than usual (avg {})", fmt_duration(avg)), Style::new().fg(theme::WARN)));
                    spans
                }
                _ => widgets::indeterminate_bar(view.tick, bar_w, theme::RUNNING),
            }
        }
        Status::Approval => vec![Span::styled("waiting for approval · press a", Style::new().fg(theme::APPROVAL))],
        Status::Failed => {
            vec![Span::styled(ellipsize(s.error().unwrap_or("failed"), width), Style::new().fg(theme::ERR))]
        }
        _ => {
            let providers: Vec<&str> = s.actions.iter().filter_map(|a| a.provider.as_deref()).collect();
            let text = if !providers.is_empty() {
                providers.join(", ")
            } else if s.actions.len() > 1 {
                format!("{} actions", s.actions.len())
            } else {
                String::new()
            };
            vec![Span::styled(ellipsize(&text, width), theme::muted())]
        }
    }
}

/// Full text worth showing under the stage table: the error or the approval request.
fn stage_note(s: &StageView) -> Option<(String, ratatui::style::Color)> {
    match s.status {
        Status::Failed => s.error().map(|e| (format!("✗ {e}"), theme::ERR)),
        Status::Approval => s
            .actions
            .iter()
            .find_map(|a| a.summary.clone())
            .map(|summary| (format!("◆ {summary}"), theme::APPROVAL)),
        _ => None,
    }
}

fn run_hooks(ctx: &mut Ctx, name: String, hooks: Vec<Hook>, pre_deploy: bool) {
    ctx.spawn_stream(move |em: Emitter<Msg>| {
        let line = |em: &Emitter<Msg>, line: String| em.emit(Msg::HookLine { name: name.clone(), line });
        for hook in &hooks {
            line(&em, format!("▸ {}", hook.label()));
            let outcome = crate::hooks::run_captured(hook, |l| {
                line(&em, format!("  {l}"));
            });
            let took = fmt_duration(outcome.duration.as_secs_f64());
            if !outcome.success {
                line(&em, format!("✗ {} — {} ({took})", hook.label(), outcome.detail));
                em.emit(Msg::HooksDone {
                    name: name.clone(),
                    pre_deploy,
                    result: Err(format!("{}: {}", hook.label(), outcome.detail)),
                });
                return;
            }
            line(&em, format!("✓ {} ({took})", hook.label()));
        }
        em.emit(Msg::HooksDone { name: name.clone(), pre_deploy, result: Ok(()) });
    });
}

impl Module for Pipelines {
    fn title(&self) -> &'static str {
        "Pipelines"
    }

    fn description(&self) -> &'static str {
        "CodePipeline · status, deploy, approve, logs, history"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        let env = ctx.env();
        let poll = ctx.config().defaults.poll_interval_seconds.max(3);
        *self = Pipelines::new();
        self.poll_every = Duration::from_secs(poll);
        self.env_name = env.name.clone();
        self.region = env.region.clone();
        self.rows = env.pipelines.iter().cloned().map(Row::new).collect();
        self.list_state.select(Some(0));
    }

    /// Selects pipeline `target`, or promotes / rolls it back when the matrix handed over an
    /// [`Intent`] with the jump ([`Ctx::jump_intent`]).
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        if let Some(intent) = ctx.take_intent() {
            return self.plan(intent, ctx);
        }
        if let Some(i) = self.rows.iter().position(|r| r.name() == target) {
            self.select(i, ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() {
            self.dialog_key(key, ctx);
            return Handled::Yes;
        }
        if let Some(p) = self.logs.as_mut().filter(|p| p.view.zoomed) {
            if let LogKey::Copy = p.view.key(key) {
                let ok = crate::platform::copy_to_clipboard(&p.view.lines().join("\n"));
                let n = p.view.lines().len();
                ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { format!("Copied {n} log lines") } else { "Clipboard not available".into() });
            }
            return Handled::Yes;
        }
        let stage_count = self.row().map(|r| self.stages(r).len()).unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1), ctx),
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected + 1, ctx),
            KeyCode::Left | KeyCode::Right if stage_count > 0 => {
                let stages = self.row().map(|r| self.stages(r)).unwrap_or_default();
                let current = self.current_stage(&stages);
                self.stage_cursor = Some(if key.code == KeyCode::Left {
                    current.saturating_sub(1)
                } else {
                    (current + 1).min(stage_count - 1)
                });
            }
            KeyCode::Char('d') => self.open_deploy(ctx),
            KeyCode::Char('R') => {
                if let Some(name) = self.row().map(|r| r.name().to_string()) {
                    self.plan(Intent::Rollback { pipeline: name }, ctx);
                }
            }
            KeyCode::Char('a') => self.open_approve(ctx),
            KeyCode::Char('s') => self.open_stop(ctx),
            KeyCode::Char('l') => self.toggle_logs(ctx),
            KeyCode::Char('h') => {
                if self.lower == Lower::History {
                    self.lower = Lower::None;
                } else {
                    self.lower = Lower::History;
                    self.logs = None;
                    self.load_history(ctx);
                }
            }
            KeyCode::Char('o') => self.open_console(ctx),
            KeyCode::Char('g') => match self.row().and_then(|r| self.compare_link(r)) {
                Some(url) => {
                    if !crate::platform::open_url(&url) {
                        ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                    }
                }
                None => ctx.toast(Level::Info, "No commit comparison: needs a failed or running run after a success, on GitHub, GitLab or Bitbucket"),
            },
            KeyCode::Char('r') => {
                self.last_poll = None;
                self.averages.clear();
                if self.lower == Lower::History {
                    self.load_history(ctx);
                }
            }
            KeyCode::Char('n') | KeyCode::Char('+') => self.open_add(ctx),
            KeyCode::Char('c') => self.copy_report(false, ctx),
            KeyCode::Char('C') => self.copy_report(true, ctx),
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(row) = self.row() {
                    self.dialog = Some(Dialog::Remove { name: row.name().to_string() });
                }
            }
            KeyCode::Esc if self.lower != Lower::None => {
                self.lower = Lower::None;
                self.logs = None;
            }
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::End => {
                if let Some(p) = &mut self.logs {
                    p.view.key(key);
                }
            }
            KeyCode::Char('z') => {
                if self.logs.is_none() {
                    self.toggle_logs(ctx);
                }
                if let Some(p) = &mut self.logs {
                    p.view.zoomed = true;
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() {
            return Handled::No;
        }
        let pos = (event.column, event.row).into();
        let in_logs = self.lower == Lower::Logs && self.logs_area.contains(pos);
        match event.kind {
            MouseEventKind::ScrollUp if in_logs => {
                if let Some(p) = &mut self.logs {
                    p.view.scroll(-3);
                }
            }
            MouseEventKind::ScrollDown if in_logs => {
                if let Some(p) = &mut self.logs {
                    p.view.scroll(3);
                }
            }
            MouseEventKind::ScrollUp if self.list_area.contains(pos) => self.select(self.selected.saturating_sub(1), ctx),
            MouseEventKind::ScrollDown if self.list_area.contains(pos) => self.select(self.selected + 1, ctx),
            MouseEventKind::Down(MouseButton::Left) if self.list_area.contains(pos) => {
                let offset = self.list_state.offset();
                let index = offset + (event.row.saturating_sub(self.list_area.y + 1) / 2) as usize;
                if index < self.rows.len() {
                    self.select(index, ctx);
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        if let Ok(msg) = payload.downcast::<Msg>() {
            self.handle(*msg, ctx);
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if self.rows.is_empty() {
            return;
        }
        let every = if self.any_running() { self.poll_every.min(Duration::from_secs(5)) } else { self.poll_every };
        if self.polling == 0 && self.last_poll.is_none_or(|t| t.elapsed() >= every) {
            self.poll_all(ctx);
        }
        let follow = self.logs.as_ref().is_some_and(|p| {
            matches!(p.state, LogsState::Live) && !p.in_flight && p.last_fetch.elapsed() >= Duration::from_secs(2)
        });
        if active && follow {
            self.fetch_logs(ctx);
        }
        let history_missing = self.row().is_some_and(|r| !self.history.contains_key(r.name()));
        if active && self.lower == Lower::History && history_missing {
            self.load_history(ctx);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let pipeline = self.row().map(|r| r.name().to_string()).unwrap_or_default();
        if let Some(p) = self.logs.as_mut().filter(|p| p.view.zoomed) {
            let title = format!("{pipeline} · {} / {}", p.stage, p.action);
            p.view.render_zoomed(f, area, &title);
            return;
        }
        if self.rows.is_empty() {
            let text = Text::from(vec![
                Line::raw(""),
                Line::styled("No pipelines configured for this environment.", theme::bold()).centered(),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("Press ", theme::muted()),
                    Span::styled("n", theme::key()),
                    Span::styled(" to search this AWS account's pipelines and add them.", theme::muted()),
                ])
                .centered(),
            ]);
            f.render_widget(Paragraph::new(text).block(widgets::panel(" Pipelines ", true)), area);
            self.render_dialog(f, area, view);
            return;
        }
        let narrow = area.width < 90;
        let (list_area, detail_area) = if narrow {
            let list_h = (self.rows.len() as u16 * 2 + 2).min(area.height / 3).max(4);
            let [a, b] = area.layout(&Layout::vertical([Constraint::Length(list_h), Constraint::Fill(1)]));
            (a, b)
        } else {
            let list_w = (area.width / 3).clamp(30, 46);
            let [a, b] = area.layout(&Layout::horizontal([Constraint::Length(list_w), Constraint::Fill(1)]));
            (a, b)
        };
        self.render_list(f, list_area, view);

        let top_h = self.detail_height(detail_area.width).min(detail_area.height);
        if self.lower == Lower::None {
            self.render_detail(f, detail_area, view);
        } else {
            let [top, bottom] =
                detail_area.layout(&Layout::vertical([Constraint::Length(top_h), Constraint::Fill(1)]));
            self.render_detail(f, top, view);
            match self.lower {
                Lower::Logs => self.render_logs(f, bottom, view),
                Lower::History => self.render_history(f, bottom, view),
                Lower::None => {}
            }
        }
        self.render_dialog(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.dialog {
            Some(Dialog::Deploy { .. }) => vec![("Enter", "deploy"), ("Esc", "cancel")],
            Some(Dialog::Approve { .. }) | Some(Dialog::Stop { .. }) => {
                vec![("Tab", "switch"), ("Enter", "submit"), ("Esc", "cancel")]
            }
            Some(Dialog::Add(..)) => vec![("Space", "mark"), ("Enter", "add"), ("Esc", "cancel")],
            None if self.zoomed() => vec![("/", "search"), ("e", "next error"), ("y", "copy all"), ("Esc", "back")],
            Some(Dialog::Remove { .. }) => vec![("Enter", "remove"), ("Esc", "cancel")],
            Some(Dialog::Release(r)) => release_hints(r),
            Some(_) => vec![("Esc", "close")],
            None if self.rows.is_empty() => vec![("n", "add pipelines")],
            None => {
                let mut h = vec![
                    ("↑↓", "pipeline"),
                    ("←→", "stage"),
                    ("d", "deploy"),
                    ("a", "approve"),
                    ("s", "stop"),
                    ("R", "rollback"),
                    ("l", "logs"),
                    ("h", "history"),
                    ("o", "console"),
                    ("r", "refresh"),
                    ("n", "add"),
                    ("x", "remove"),
                ];
                if self.row().and_then(Row::exec_status) == Some(Status::Failed) {
                    h.insert(2, ("c", "copy for AI"));
                }
                if self.lower == Lower::Logs {
                    h.insert(h.iter().position(|(k, _)| *k == "l").map_or(h.len(), |i| i + 1), ("z", "full screen"));
                    h.push(("PgUp/PgDn", "scroll"));
                }
                h
            }
        }
    }

    fn animating(&self) -> bool {
        self.any_running()
            || self.rows.iter().any(|r| r.loading && r.state.is_none())
            || self.report_loading
            || matches!(self.dialog, Some(Dialog::Busy(_)) | Some(Dialog::Hooks { state: HookState::Running, .. }))
            || self.logs.as_ref().is_some_and(|p| matches!(p.state, LogsState::Loading | LogsState::Live))
            || (self.lower == Lower::History && self.row().is_some_and(|r| !self.history.contains_key(r.name())))
    }

    fn capturing(&self) -> bool {
        self.dialog.is_some() || self.zoomed()
    }

    fn wants_mouse(&self) -> bool {
        !self.zoomed()
    }
}

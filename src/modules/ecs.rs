//! ECS explorer: clusters → services → tasks. A service shows its deployments (rollout state,
//! circuit breaker), CPU and memory charts and its recent events; its tasks show why each one
//! stopped (stop code, reason, every container's exit code, a likely cause) and their
//! containers' logs. Actions: a new deployment, the desired count, deploying another task
//! definition revision (a rollback); each followed until it completes. Plus a report of the
//! service for an AI agent.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::ecs::{self, Change, Cluster, EcsService, Metrics, Progress, State, Task, TaskDefinition};
use crate::aws::lambda::Range;
use crate::aws::{self, AwsError};
use crate::config::is_production;
use crate::tui::chart::{Agg, Chart};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, now_secs, sanitize};

/// Selection settles this long before its details are read.
const DEBOUNCE: Duration = Duration::from_millis(200);
/// Metrics older than this are read again.
const METRICS_FRESH: Duration = Duration::from_secs(120);
/// Lists refresh on their own this often while shown.
const REFRESH: Duration = Duration::from_secs(20);
/// A change is checked this often until it completes.
const FOLLOW_EVERY: Duration = Duration::from_secs(3);
/// Changes are followed at most this long.
const FOLLOW_FOR: Duration = Duration::from_secs(30 * 60);
/// Revisions offered when deploying another one.
const REVISIONS: usize = 15;
/// Log lines read of a container.
const LOG_LINES: usize = 500;

enum Msg {
    Clusters(aws::Result<Vec<Cluster>>),
    Services { cluster: String, result: aws::Result<Vec<EcsService>> },
    Tasks { cluster: String, service: String, result: aws::Result<Vec<Task>> },
    Definition { arn: String, result: Box<aws::Result<TaskDefinition>> },
    Metrics { key: MetricKey, result: Box<aws::Result<Metrics>> },
    Revisions { cluster: String, service: String, result: aws::Result<Vec<TaskDefinition>> },
    Updated { cluster: String, change: Change, result: Box<aws::Result<EcsService>> },
    Followed { id: u64, result: Box<aws::Result<Option<EcsService>>> },
    Log { title: String, result: aws::Result<Vec<String>> },
    Report { service: String, result: aws::Result<String> },
}

type MetricKey = (String, String, Range);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level3 {
    Clusters,
    Services,
    Tasks,
}

/// A change to apply once confirmed.
#[derive(Clone)]
struct Action {
    cluster: String,
    service: String,
    change: Change,
}

enum Dialog {
    Confirm { title: String, lines: Vec<String>, typed: Option<(String, TextInput)>, action: Action, busy: bool, error: Option<String> },
    Desired { form: Form, cluster: String, service: String, busy: bool },
    Revisions { picker: Picker, cluster: String, service: String, defs: Vec<TaskDefinition>, current: String },
    Container { picker: Picker, task: Box<Task>, names: Vec<String> },
}

/// A change being followed until its deployment completes (or its tasks run).
struct Follow {
    id: u64,
    cluster: String,
    service: String,
    deployment: Option<String>,
    desired: Option<i64>,
    what: String,
    since: Instant,
    checked: Instant,
    in_flight: bool,
}

pub struct Ecs {
    env_name: String,
    region: String,
    level: Level3,
    clusters: Vec<Cluster>,
    clusters_load: LoadState,
    /// The cluster whose services are shown.
    cluster: Option<String>,
    services: Vec<EcsService>,
    services_load: LoadState,
    /// The service whose tasks are shown.
    service: Option<String>,
    tasks: Vec<Task>,
    tasks_load: LoadState,
    loaded_at: Instant,
    defs: HashMap<String, aws::Result<TaskDefinition>>,
    defs_in_flight: HashSet<String>,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    list_state: ListState,
    list_area: Rect,
    detail_scroll: u16,
    pending: Option<Instant>,
    range: Range,
    metrics: HashMap<MetricKey, (Instant, aws::Result<Metrics>)>,
    metrics_in_flight: HashSet<MetricKey>,
    dialog: Option<Dialog>,
    follows: Vec<Follow>,
    next_follow: u64,
    reporting: HashSet<String>,
    /// A container's log, full screen.
    log: Option<(String, LogView)>,
    log_loading: bool,
    /// What another module asked to show: (cluster, service), once the lists have it.
    open_target: Option<(String, Option<String>)>,
    /// Revisions being listed for a rollback.
    revisions_loading: bool,
}

impl Ecs {
    pub fn new() -> Ecs {
        Ecs {
            env_name: String::new(),
            region: String::new(),
            level: Level3::Clusters,
            clusters: Vec::new(),
            clusters_load: LoadState::Idle,
            cluster: None,
            services: Vec::new(),
            services_load: LoadState::Idle,
            service: None,
            tasks: Vec::new(),
            tasks_load: LoadState::Idle,
            loaded_at: Instant::now(),
            defs: HashMap::new(),
            defs_in_flight: HashSet::new(),
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            list_area: Rect::default(),
            detail_scroll: 0,
            pending: None,
            range: Range::Day,
            metrics: HashMap::new(),
            metrics_in_flight: HashSet::new(),
            dialog: None,
            follows: Vec::new(),
            next_follow: 0,
            reporting: HashSet::new(),
            log: None,
            log_loading: false,
            open_target: None,
            revisions_loading: false,
        }
    }

    // ---- loading ------------------------------------------------------------------------------

    fn load_clusters(&mut self, ctx: &mut Ctx) {
        self.clusters_load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Clusters(ecs::clusters(&aws)));
    }

    fn load_services(&mut self, ctx: &mut Ctx) {
        let Some(cluster) = self.cluster.clone() else { return };
        self.services_load = LoadState::Loading;
        self.loaded_at = Instant::now();
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = ecs::services(&aws, &cluster);
            Msg::Services { cluster, result }
        });
    }

    fn load_tasks(&mut self, ctx: &mut Ctx) {
        let (Some(cluster), Some(service)) = (self.cluster.clone(), self.service.clone()) else { return };
        self.tasks_load = LoadState::Loading;
        self.loaded_at = Instant::now();
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = ecs::tasks(&aws, &cluster, &service);
            Msg::Tasks { cluster, service, result }
        });
    }

    fn fetch_definition(&mut self, arn: &str, ctx: &mut Ctx) {
        if arn.is_empty() || self.defs.contains_key(arn) || !self.defs_in_flight.insert(arn.to_string()) {
            return;
        }
        let (aws, arn) = (ctx.env().aws.clone(), arn.to_string());
        ctx.spawn(move || {
            let result = Box::new(ecs::task_definition(&aws, &arn));
            Msg::Definition { arn, result }
        });
    }

    fn fetch_metrics(&mut self, ctx: &mut Ctx) {
        let Some(s) = self.selected_service() else { return };
        let key = (s.cluster_name().to_string(), s.service_name.clone(), self.range);
        if self.metrics.get(&key).is_some_and(|(at, _)| at.elapsed() < METRICS_FRESH) || !self.metrics_in_flight.insert(key.clone()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = Box::new(ecs::metrics(&aws, &key.0, &key.1, key.2, now_secs()));
            Msg::Metrics { key, result }
        });
    }

    /// Reads what the selection shows once it settles: the task definition, the metrics.
    fn settle(&mut self, ctx: &mut Ctx) {
        match self.level {
            Level3::Clusters => {}
            Level3::Services => {
                if let Some(td) = self.selected_service().map(|s| s.task_definition.clone()) {
                    self.fetch_definition(&td, ctx);
                }
                self.fetch_metrics(ctx);
            }
            Level3::Tasks => {
                if let Some(td) = self.selected_task().map(|t| t.task_definition_arn.clone()) {
                    self.fetch_definition(&td, ctx);
                }
            }
        }
    }

    // ---- selection ----------------------------------------------------------------------------

    fn len(&self) -> usize {
        match self.level {
            Level3::Clusters => self.clusters.len(),
            Level3::Services => self.services.len(),
            Level3::Tasks => self.tasks.len(),
        }
    }

    /// The name the filter matches and the selection is kept by.
    fn key_of(&self, i: usize) -> String {
        match self.level {
            Level3::Clusters => self.clusters[i].cluster_name.clone(),
            Level3::Services => self.services[i].service_name.clone(),
            Level3::Tasks => self.tasks[i].task_arn.clone(),
        }
    }

    fn matches(&self, i: usize, query: &str) -> bool {
        match self.level {
            Level3::Clusters => matches_filter(&self.clusters[i].cluster_name, query),
            Level3::Services => {
                let s = &self.services[i];
                matches_filter(&s.service_name, query) || matches_filter(ecs::revision_of(&s.task_definition), query)
            }
            Level3::Tasks => {
                let t = &self.tasks[i];
                matches_filter(t.id(), query)
                    || matches_filter(ecs::revision_of(&t.task_definition_arn), query)
                    || matches_filter(&t.last_status, query)
                    || t.stopped_reason.as_deref().is_some_and(|r| matches_filter(r, query))
            }
        }
    }

    fn selected_key(&self) -> Option<String> {
        let i = *self.visible.get(self.list_state.selected()?)?;
        Some(self.key_of(i))
    }

    /// Rebuilds the visible rows, keeping the selection on the same item (by name, not row).
    fn rebuild(&mut self) {
        let keep = self.selected_key();
        let query = self.filter.value.trim().to_lowercase();
        self.visible = (0..self.len()).filter(|&i| query.is_empty() || self.matches(i, &query)).collect();
        let index = keep.and_then(|k| self.visible.iter().position(|&i| self.key_of(i) == k)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    fn select(&mut self, index: usize) {
        if self.visible.is_empty() {
            return;
        }
        let index = index.min(self.visible.len() - 1);
        if self.list_state.selected() != Some(index) {
            self.detail_scroll = 0;
        }
        self.list_state.select(Some(index));
        self.pending = Some(Instant::now());
    }

    fn select_key(&mut self, key: &str) -> bool {
        if !self.visible.iter().any(|&i| self.key_of(i) == key) && !self.filter.value.is_empty() {
            self.filter.clear();
            self.filtering = false;
            self.rebuild();
        }
        match self.visible.iter().position(|&i| self.key_of(i) == key) {
            Some(pos) => {
                self.select(pos);
                true
            }
            None => false,
        }
    }

    fn selected_cluster(&self) -> Option<&Cluster> {
        if self.level != Level3::Clusters {
            return None;
        }
        self.clusters.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn selected_service(&self) -> Option<&EcsService> {
        match self.level {
            Level3::Services => self.services.get(*self.visible.get(self.list_state.selected()?)?),
            Level3::Tasks => self.services.iter().find(|s| Some(&s.service_name) == self.service.as_ref()),
            Level3::Clusters => None,
        }
    }

    fn selected_task(&self) -> Option<&Task> {
        if self.level != Level3::Tasks {
            return None;
        }
        self.tasks.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn definition(&self, arn: &str) -> Option<&TaskDefinition> {
        self.defs.get(arn).and_then(|d| d.as_ref().ok())
    }

    fn go(&mut self, level: Level3, select: Option<String>) {
        self.level = level;
        self.filter.clear();
        self.filtering = false;
        self.detail_scroll = 0;
        self.list_state.select(None);
        self.rebuild();
        if let Some(k) = select {
            self.select_key(&k);
        } else {
            self.select(0);
        }
    }

    fn open_cluster(&mut self, name: String, ctx: &mut Ctx) {
        if self.cluster.as_ref() != Some(&name) {
            self.services.clear();
            self.cluster = Some(name);
            self.load_services(ctx);
        }
        self.go(Level3::Services, None);
    }

    fn open_service(&mut self, ctx: &mut Ctx) {
        let Some(name) = self.selected_service().map(|s| s.service_name.clone()) else { return };
        if self.service.as_ref() != Some(&name) {
            self.tasks.clear();
            self.service = Some(name);
        }
        self.load_tasks(ctx);
        self.go(Level3::Tasks, None);
    }

    fn back(&mut self) {
        match self.level {
            Level3::Tasks => {
                let keep = self.service.clone();
                self.go(Level3::Services, keep);
            }
            Level3::Services => {
                let keep = self.cluster.clone();
                self.go(Level3::Clusters, keep);
            }
            Level3::Clusters => {}
        }
    }

    /// Opens what `open_target` names once the lists have it. True when done.
    fn try_open(&mut self, ctx: &mut Ctx) -> bool {
        let Some((cluster, service)) = self.open_target.clone() else { return false };
        if !self.clusters.iter().any(|c| c.cluster_name == cluster) {
            if matches!(self.clusters_load, LoadState::Loaded | LoadState::Failed(_)) {
                self.open_target = None;
                ctx.toast(Level::Warn, format!("No ECS cluster {cluster} in {}", self.env_name));
            }
            return false;
        }
        if self.cluster.as_ref() != Some(&cluster) || self.level == Level3::Clusters {
            self.open_cluster(cluster.clone(), ctx);
        }
        let Some(service) = service else {
            self.open_target = None;
            return true;
        };
        if self.services.is_empty() {
            return false;
        }
        if self.level == Level3::Tasks {
            self.go(Level3::Services, None);
        }
        if !self.select_key(&service) {
            ctx.toast(Level::Warn, format!("No service {service} in cluster {cluster}"));
        }
        self.open_target = None;
        true
    }

    // ---- actions ------------------------------------------------------------------------------

    fn confirm(&self, title: String, lines: Vec<String>, action: Action) -> Dialog {
        let typed = is_production(&self.env_name).then(|| (self.env_name.clone(), TextInput::default()));
        Dialog::Confirm { title, lines, typed, action, busy: false, error: None }
    }

    fn open_redeploy(&mut self, ctx: &mut Ctx) {
        let Some(s) = self.selected_service() else { return };
        if let Some(why) = s.redeploy_refusal() {
            return ctx.toast(Level::Warn, why);
        }
        let td = ecs::revision_of(&s.task_definition).to_string();
        let image = self.definition(&s.task_definition).map(|d| format!(" (image {})", d.main_image())).unwrap_or_default();
        let (min, max) = s
            .deployment_configuration
            .as_ref()
            .map_or((100, 200), |c| (c.minimum_healthy_percent.unwrap_or(100), c.maximum_percent.unwrap_or(200)));
        let lines = vec![
            format!("{} keeps task definition {td}{image}.", s.service_name),
            format!(
                "New tasks start (pulling the image again, so a pushed tag is picked up) and replace the {} running, keeping at least {min} % and at most {max} % of {} desired.",
                s.running_count, s.desired_count
            ),
            breaker_line(s),
        ];
        let action = Action { cluster: s.cluster_name().to_string(), service: s.service_name.clone(), change: Change::ForceDeploy };
        self.dialog = Some(self.confirm(format!("New deployment of {} · {}", s.service_name, self.env_name), lines, action));
    }

    fn open_desired(&mut self) {
        let Some(s) = self.selected_service() else { return };
        let mut form = Form::new(
            format!("Desired tasks of {} · {}", s.service_name, self.env_name),
            vec![Field::fixed("Now", format!("{} running of {} desired", s.running_count, s.desired_count)), Field::text("Desired", s.desired_count.to_string())],
        )
        .submit_label("apply");
        form.note = Some(match s.controller() {
            "ECS" => "ECS starts or stops tasks until the count matches; no new deployment.".to_string(),
            other => format!("A {other} service: only its count changes; its task sets stay as they are."),
        });
        if is_production(&self.env_name) {
            form = form.confirm_with(&self.env_name);
        }
        self.dialog = Some(Dialog::Desired { form, cluster: s.cluster_name().to_string(), service: s.service_name.clone(), busy: false });
    }

    fn submit_desired(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Desired { form, cluster, service, busy }) = &mut self.dialog else { return };
        let value = form.get("Desired").trim().to_string();
        let n = match value.parse::<i64>() {
            Ok(n) if (0..=5000).contains(&n) => n,
            _ => return form.fail("Desired is a whole number from 0"),
        };
        let current = self.services.iter().find(|s| s.service_name == *service).map(|s| s.desired_count);
        if current == Some(n) {
            return form.fail(format!("{service} already wants {n} tasks"));
        }
        *busy = true;
        let action = Action { cluster: cluster.clone(), service: service.clone(), change: Change::Desired(n) };
        self.run(action, ctx);
    }

    fn open_revisions(&mut self, ctx: &mut Ctx) {
        let Some(s) = self.selected_service() else { return };
        if let Some(why) = s.redeploy_refusal() {
            return ctx.toast(Level::Warn, why);
        }
        if self.revisions_loading {
            return;
        }
        let family = ecs::revision_of(&s.task_definition).rsplit_once(':').map_or(String::new(), |(f, _)| f.to_string());
        if family.is_empty() {
            return ctx.toast(Level::Warn, format!("{} has no task definition", s.service_name));
        }
        let (aws, cluster, service) = (ctx.env().aws.clone(), s.cluster_name().to_string(), s.service_name.clone());
        self.revisions_loading = true;
        ctx.toast(Level::Info, format!("Listing the revisions of {family}…"));
        ctx.spawn(move || {
            let result = ecs::revisions(&aws, &family, REVISIONS);
            Msg::Revisions { cluster, service, result }
        });
    }

    fn on_revisions(&mut self, cluster: String, service: String, result: aws::Result<Vec<TaskDefinition>>, ctx: &mut Ctx) {
        self.revisions_loading = false;
        let defs = match result {
            Ok(d) if d.is_empty() => return ctx.toast(Level::Warn, "No active revisions"),
            Ok(d) => d,
            Err(e) => return ctx.toast(Level::Error, format!("Could not list the revisions: {}", e.short())),
        };
        let Some(current) = self.services.iter().find(|s| s.service_name == service).map(|s| s.task_definition.clone()) else { return };
        let now = now_secs();
        let items: Vec<Item> = defs
            .iter()
            .map(|d| {
                let mut detail = d.main_image();
                if let Some(t) = d.registered_at {
                    detail.push_str(&format!(" · registered {}", crate::util::fmt_ago(t.min(now))));
                }
                if let (Some(cpu), Some(mem)) = (&d.cpu, &d.memory) {
                    detail.push_str(&format!(" · cpu {cpu} · {mem} MiB"));
                }
                if d.task_definition_arn == current {
                    detail.push_str(" · deployed now");
                }
                Item::new(format!("revision {}", d.revision), detail).current(d.task_definition_arn == current)
            })
            .collect();
        let cursor = defs.iter().position(|d| d.task_definition_arn != current && ecs::revision_number(&d.task_definition_arn) < ecs::revision_number(&current)).unwrap_or(0);
        let picker = Picker::new(format!("Deploy a revision of {service} (a rollback, say)"), "choose", items, false).with_cursor(cursor);
        self.dialog = Some(Dialog::Revisions { picker, cluster, service, defs, current });
    }

    fn confirm_revision(&mut self, cluster: String, service: String, def: TaskDefinition, current: String) {
        let Some(s) = self.services.iter().find(|s| s.service_name == service) else { return };
        let now_image = self.definition(&current).map(|d| format!(" · {}", d.main_image())).unwrap_or_default();
        let back = ecs::revision_number(&def.task_definition_arn) < ecs::revision_number(&current);
        let lines = vec![
            format!("now  {}{now_image}", ecs::revision_of(&current)),
            format!("new  {} · {}", ecs::revision_of(&def.task_definition_arn), def.main_image()),
            format!("A deployment of the {} replaces the {} running tasks.", if back { "older revision" } else { "revision" }, s.running_count),
            breaker_line(s),
        ];
        let verb = if back { "Roll back" } else { "Deploy" };
        let title = format!("{verb} {service} to revision {} · {}", def.revision, self.env_name);
        let action = Action { cluster, service, change: Change::TaskDefinition(def.task_definition_arn.clone()) };
        self.dialog = Some(self.confirm(title, lines, action));
    }

    fn run(&mut self, action: Action, ctx: &mut Ctx) {
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = Box::new(ecs::update_service(&aws, &action.cluster, &action.service, &action.change));
            Msg::Updated { cluster: action.cluster, change: action.change, result }
        });
    }

    fn on_updated(&mut self, cluster: String, change: Change, result: aws::Result<EcsService>, ctx: &mut Ctx) {
        let s = match result {
            Ok(s) => s,
            Err(e) => {
                let text = refusal(&e);
                match &mut self.dialog {
                    Some(Dialog::Confirm { busy, error, .. }) => {
                        *busy = false;
                        *error = Some(text);
                    }
                    Some(Dialog::Desired { form, busy, .. }) => {
                        *busy = false;
                        form.fail(text);
                    }
                    _ => ctx.toast(Level::Error, text),
                }
                return;
            }
        };
        self.dialog = None;
        let what = format!("{} · {}", s.service_name, change.describe());
        let deployment = change.deploys().then(|| s.primary().map(|d| d.id.clone())).flatten();
        let desired = match change {
            Change::Desired(n) => Some(n),
            _ => None,
        };
        ctx.toast(Level::Success, if change.deploys() { format!("Deployment started: {what}") } else { format!("Changed {what}") });
        let now = Instant::now();
        self.next_follow += 1;
        // A newer change of the same kind replaces the one followed; a deployment and a count both stay.
        self.follows.retain(|f| !(f.cluster == cluster && f.service == s.service_name && f.deployment.is_some() == deployment.is_some()));
        self.follows.push(Follow {
            id: self.next_follow,
            cluster: cluster.clone(),
            service: s.service_name.clone(),
            deployment,
            desired,
            what,
            since: now,
            checked: now,
            in_flight: false,
        });
        self.put_service(&cluster, s);
    }

    /// Replaces a service in the list with a newer read of it.
    fn put_service(&mut self, cluster: &str, s: EcsService) {
        if self.cluster.as_deref() != Some(cluster) {
            return;
        }
        if let Some(slot) = self.services.iter_mut().find(|x| x.service_name == s.service_name) {
            *slot = s;
        }
    }

    fn poll_follows(&mut self, ctx: &mut Ctx) {
        let aws = ctx.env().aws.clone();
        for f in self.follows.iter_mut().filter(|f| !f.in_flight && f.checked.elapsed() >= FOLLOW_EVERY) {
            f.in_flight = true;
            f.checked = Instant::now();
            let (aws, id, cluster, service) = (aws.clone(), f.id, f.cluster.clone(), f.service.clone());
            ctx.spawn(move || Msg::Followed { id, result: Box::new(ecs::service(&aws, &cluster, &service)) });
        }
    }

    fn on_followed(&mut self, id: u64, result: aws::Result<Option<EcsService>>, ctx: &mut Ctx) {
        let Some(pos) = self.follows.iter().position(|f| f.id == id) else { return };
        self.follows[pos].in_flight = false;
        let s = match result {
            Ok(Some(s)) => s,
            Ok(None) => {
                let f = self.follows.remove(pos);
                return ctx.toast(Level::Warn, format!("{} is gone", f.service));
            }
            Err(e) => {
                let f = self.follows.remove(pos);
                return ctx.toast(Level::Warn, format!("Could not follow {}: {}", f.what, e.short()));
            }
        };
        let f = &self.follows[pos];
        let progress = ecs::progress(&s, f.deployment.as_deref(), f.desired);
        let cluster = f.cluster.clone();
        match progress {
            Progress::Done(text) => {
                self.follows.remove(pos);
                ctx.notify(Level::Success, "ECS", &text);
                self.reload_after_change(&cluster, &s.service_name, ctx);
            }
            Progress::Failed(text) => {
                self.follows.remove(pos);
                ctx.notify(Level::Error, "ECS", &format!("{text}. c copies a report for your AI agent"));
                self.reload_after_change(&cluster, &s.service_name, ctx);
            }
            Progress::Waiting(_) if f.since.elapsed() > FOLLOW_FOR => {
                let f = self.follows.remove(pos);
                ctx.toast(Level::Warn, format!("{} still not done after 30 minutes; ECS keeps at it", f.what));
            }
            Progress::Waiting(_) => {}
        }
        self.put_service(&cluster, s);
    }

    fn reload_after_change(&mut self, cluster: &str, service: &str, ctx: &mut Ctx) {
        if self.level == Level3::Tasks && self.cluster.as_deref() == Some(cluster) && self.service.as_deref() == Some(service) {
            self.load_tasks(ctx);
        }
    }

    fn follow_of(&self, service: &str) -> Option<&Follow> {
        self.follows.iter().find(|f| f.service == service && self.cluster.as_deref() == Some(f.cluster.as_str()))
    }

    // ---- logs and reports -----------------------------------------------------------------------

    /// The log group of the main container of what is selected (`l`).
    fn jump_to_logs(&mut self, ctx: &mut Ctx) {
        let arn = match self.level {
            Level3::Services => self.selected_service().map(|s| s.task_definition.clone()),
            Level3::Tasks => self.selected_task().map(|t| t.task_definition_arn.clone()),
            Level3::Clusters => None,
        };
        let Some(arn) = arn else { return };
        let Some(def) = self.definition(&arn) else {
            self.fetch_definition(&arn, ctx);
            return ctx.toast(Level::Info, "Reading the task definition… press l again");
        };
        let groups = def.log_groups();
        match groups.first() {
            Some(group) => ctx.jump("Logs", group.clone()),
            None => {
                let driver = def.container_definitions.iter().find_map(|c| c.log_driver()).unwrap_or("none");
                ctx.toast(Level::Warn, format!("{} does not log to CloudWatch Logs (log driver {driver})", ecs::revision_of(&arn)));
            }
        }
    }

    /// Enter on a task: the last lines of a container's log (asks which one when several).
    fn open_task_log(&mut self, ctx: &mut Ctx) {
        let Some(task) = self.selected_task().cloned() else { return };
        let Some(def) = self.definition(&task.task_definition_arn).cloned() else {
            self.fetch_definition(&task.task_definition_arn, ctx);
            return ctx.toast(Level::Info, "Reading the task definition… press Enter again");
        };
        let names: Vec<String> = def.container_definitions.iter().map(|c| c.name.clone()).collect();
        if names.len() == 1 {
            return self.read_log(&task, &def, &names[0], ctx);
        }
        let items: Vec<Item> = def
            .container_definitions
            .iter()
            .map(|c| {
                let state = task.containers.iter().find(|x| x.name == c.name);
                let mut detail = c.image.rsplit('/').next().unwrap_or(&c.image).to_string();
                if let Some(code) = state.and_then(|s| s.exit_code) {
                    detail.push_str(&format!(" · exit {code}"));
                }
                if !c.essential() {
                    detail.push_str(" · not essential");
                }
                Item::new(c.name.clone(), detail)
            })
            .collect();
        // The failing container first, else the first essential one.
        let cursor = def
            .container_definitions
            .iter()
            .position(|c| task.failed_containers().any(|f| f.name == c.name))
            .or_else(|| def.container_definitions.iter().position(|c| c.essential()))
            .unwrap_or(0);
        let picker = Picker::new(format!("Logs of task {}", short_id(task.id())), "read", items, false).with_cursor(cursor);
        self.dialog = Some(Dialog::Container { picker, task: Box::new(task), names });
    }

    fn read_log(&mut self, task: &Task, def: &TaskDefinition, container: &str, ctx: &mut Ctx) {
        let Some(stream) = def.log_stream(container, task.id()) else {
            let c = def.container(container);
            let why = match c.and_then(|c| c.log_driver()) {
                Some("awslogs") => "has no awslogs-stream-prefix: its streams are named after Docker container ids. l opens the log group".to_string(),
                Some(driver) => format!("logs with {driver}, not to CloudWatch Logs"),
                None => "has no log configuration".to_string(),
            };
            return ctx.toast(Level::Warn, format!("{container} {why}"));
        };
        self.log_loading = true;
        let title = format!("{container} · task {} · {}", short_id(task.id()), stream.group);
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = ecs::tail_log(&aws, &stream, LOG_LINES);
            Msg::Log { title, result }
        });
    }

    fn copy_report(&mut self, ctx: &mut Ctx) {
        let Some(s) = self.selected_service() else { return };
        let (cluster, name) = (s.cluster_name().to_string(), s.service_name.clone());
        if !self.reporting.insert(name.clone()) {
            return ctx.toast(Level::Info, format!("Still writing the report of {name}"));
        }
        ctx.toast(Level::Info, format!("Writing the report of {name}: deployments, events, stopped tasks and their logs…"));
        let env = ctx.env();
        let (aws, env_name, region) = (env.aws.clone(), env.name.clone(), env.region.clone());
        ctx.spawn(move || {
            let result = ecs::gather_report(&aws, &env_name, &region, &cluster, &name, now_secs());
            Msg::Report { service: name, result }
        });
    }

    fn on_report(&mut self, service: String, result: aws::Result<String>, ctx: &mut Ctx) {
        self.reporting.remove(&service);
        let md = match result {
            Ok(md) => md,
            Err(e) => return ctx.toast(Level::Error, format!("Could not write the report: {}", e.short())),
        };
        if crate::platform::copy_to_clipboard(&md) {
            return ctx.toast(Level::Success, format!("Copied the report of {service}. Paste it into your AI agent."));
        }
        match crate::platform::write_temp_file(&format!("ecs-{service}"), &md) {
            Ok(path) => ctx.toast(Level::Success, format!("Saved the report to {}", path.display())),
            Err(e) => ctx.toast(Level::Error, format!("Could not save the report: {e}")),
        }
    }

    fn copy(&self, text: &str, what: &str, ctx: &mut Ctx) {
        let ok = crate::platform::copy_to_clipboard(text);
        ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { format!("Copied {what}") } else { "Clipboard not available".into() });
    }

    fn open_console(&self, ctx: &mut Ctx) {
        let path = match self.level {
            Level3::Clusters => self.selected_cluster().map(|c| format!("ecs/v2/clusters/{}/services?region={}", c.cluster_name, self.region)),
            Level3::Services => self.selected_service().map(|s| s.console_path(&self.region)),
            Level3::Tasks => self.selected_task().zip(self.cluster.as_ref()).map(|(t, c)| format!("ecs/v2/clusters/{c}/tasks/{}/configuration?region={}", t.id(), self.region)),
        };
        let Some(path) = path else { return };
        let url = aws::console_url(&self.region, &path);
        if !crate::platform::open_url(&url) {
            ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
        }
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        match self.level {
            Level3::Clusters => self.load_clusters(ctx),
            Level3::Services => {
                self.metrics.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(5));
                self.metrics.clear();
                self.load_services(ctx);
                self.pending = Some(Instant::now());
            }
            Level3::Tasks => self.load_tasks(ctx),
        }
    }

    // ---- dialogs ----------------------------------------------------------------------------------

    fn dialog_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(dialog) = self.dialog.take() else { return };
        match dialog {
            Dialog::Confirm { busy: true, .. } => self.dialog = Some(dialog),
            Dialog::Confirm { title, lines, typed: Some((name, mut input)), action, busy, mut error } => match input.handle(key) {
                InputEvent::Cancel => {}
                InputEvent::Submit if input.value.trim() == name => {
                    self.dialog = Some(Dialog::Confirm { title, lines, typed: Some((name, input)), action: action.clone(), busy: true, error: None });
                    self.run(action, ctx);
                }
                InputEvent::Submit => {
                    error = Some(format!("Type '{name}' to confirm"));
                    self.dialog = Some(Dialog::Confirm { title, lines, typed: Some((name, input)), action, busy, error });
                }
                _ => self.dialog = Some(Dialog::Confirm { title, lines, typed: Some((name, input)), action, busy, error }),
            },
            Dialog::Confirm { title, lines, typed: None, action, busy, error } => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.dialog = Some(Dialog::Confirm { title, lines, typed: None, action: action.clone(), busy: true, error: None });
                    self.run(action, ctx);
                }
                KeyCode::Esc | KeyCode::Char('n') => {}
                _ => self.dialog = Some(Dialog::Confirm { title, lines, typed: None, action, busy, error }),
            },
            Dialog::Desired { busy: true, .. } => self.dialog = Some(dialog),
            Dialog::Desired { mut form, cluster, service, busy } => match form.handle(key) {
                FormEvent::Cancel => {}
                FormEvent::Submit => {
                    self.dialog = Some(Dialog::Desired { form, cluster, service, busy });
                    self.submit_desired(ctx);
                }
                FormEvent::Pending => self.dialog = Some(Dialog::Desired { form, cluster, service, busy }),
            },
            Dialog::Revisions { mut picker, cluster, service, defs, current } => match picker.handle(key) {
                Outcome::Pending => self.dialog = Some(Dialog::Revisions { picker, cluster, service, defs, current }),
                Outcome::Cancel => {}
                Outcome::Done(chosen) => {
                    let Some(def) = chosen.first().and_then(|&i| defs.get(i)).cloned() else { return };
                    if def.task_definition_arn == current {
                        return ctx.toast(Level::Info, format!("{service} runs revision {} already: D starts a new deployment of it", def.revision));
                    }
                    self.confirm_revision(cluster, service, def, current);
                }
            },
            Dialog::Container { mut picker, task, names } => match picker.handle(key) {
                Outcome::Pending => self.dialog = Some(Dialog::Container { picker, task, names }),
                Outcome::Cancel => {}
                Outcome::Done(chosen) => {
                    let Some(name) = chosen.first().and_then(|&i| names.get(i)).cloned() else { return };
                    if let Some(def) = self.definition(&task.task_definition_arn).cloned() {
                        self.read_log(&task, &def, &name, ctx);
                    }
                }
            },
        }
    }

    // ---- rendering ----------------------------------------------------------------------------------

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let width = area.width.saturating_sub(4) as usize;
        let now = view.now;
        let items: Vec<ListItem> = self.visible.iter().map(|&i| self.row(i, width, now, view.tick)).collect();
        let (title, load, empty, placeholder) = match self.level {
            Level3::Clusters => (" ECS clusters ".to_string(), &self.clusters_load, "No ECS clusters in this region", "type / to search clusters"),
            Level3::Services => (
                format!(" {} · {} services ", self.cluster.as_deref().unwrap_or_default(), self.services.len()),
                &self.services_load,
                "No services in this cluster",
                "type / to search services or task definitions",
            ),
            Level3::Tasks => {
                let stopped = self.tasks.iter().filter(|t| t.stopped()).count();
                (
                    format!(" {} · {} running · {stopped} stopped ", self.service.as_deref().unwrap_or_default(), self.tasks.len() - stopped),
                    &self.tasks_load,
                    "No tasks running or stopped in the last hour",
                    "type / to search tasks, revisions or reasons",
                )
            }
        };
        let title = match load {
            LoadState::Loading if self.len() == 0 => format!(" {} Loading… ", theme::spinner(view.tick)),
            LoadState::Loading => format!("{}{} ", title, theme::spinner(view.tick)),
            _ => title,
        };
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (line, cursor) = filter_bar(&self.filter, self.filtering, placeholder);
        f.render_widget(Paragraph::new(line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        if self.visible.is_empty() {
            let text = match load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading | LoadState::Idle => Line::styled("Loading…", theme::muted()),
                LoadState::Loaded if self.len() > 0 => Line::styled("Nothing matches", theme::muted()),
                LoadState::Loaded => Line::styled(empty, theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn row(&self, i: usize, width: usize, now: f64, tick: usize) -> ListItem<'static> {
        let line = |symbol: Span<'static>, name: String, right: String| {
            let right_w = right.chars().count();
            let name = ellipsize(&name, width.saturating_sub(right_w + 4));
            let pad = width.saturating_sub(name.chars().count() + right_w + 2);
            ListItem::new(Line::from(vec![symbol, Span::raw(" "), Span::raw(name), Span::raw(" ".repeat(pad)), Span::styled(right, theme::muted())]))
        };
        match self.level {
            Level3::Clusters => {
                let c = &self.clusters[i];
                let right = format!("{} services · {} tasks", c.active_services_count, c.running_tasks_count);
                let color = if c.status == "ACTIVE" { theme::OK } else { theme::WARN };
                line(Span::styled("●", Style::new().fg(color)), c.cluster_name.clone(), right)
            }
            Level3::Services => {
                let s = &self.services[i];
                let state = s.state(now);
                let following = self.follow_of(&s.service_name).is_some();
                let symbol = if following || state == State::Deploying { theme::spinner(tick) } else { state.symbol() };
                let right = format!("{}/{}  {:>7}", s.running_count, s.desired_count, ellipsize(&s.launch(), 7));
                line(Span::styled(symbol, Style::new().fg(state_color(state))), s.service_name.clone(), right)
            }
            Level3::Tasks => {
                let t = &self.tasks[i];
                let (symbol, color) = task_symbol(t);
                let when = t.when().map(|w| crate::util::fmt_ago(w.min(now))).unwrap_or_default();
                let rev = ecs::revision_number(&t.task_definition_arn).map(|r| format!(":{r}")).unwrap_or_default();
                let status = if t.stopped() { "stopped".to_string() } else { t.last_status.to_lowercase() };
                line(Span::styled(symbol, Style::new().fg(color)), format!("{} {rev}", short_id(t.id())), format!("{status} {when}"))
            }
        }
    }

    fn render_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let width = area.width.saturating_sub(4) as usize;
        let (title, lines) = match self.level {
            Level3::Clusters => match self.selected_cluster() {
                Some(c) => (c.cluster_name.clone(), self.cluster_lines(c)),
                None => ("Cluster".to_string(), vec![Line::styled("Select a cluster", theme::muted())]),
            },
            Level3::Services => match self.selected_service() {
                Some(s) => (s.service_name.clone(), self.service_lines(s, width, view)),
                None => ("Service".to_string(), vec![Line::styled("Select a service", theme::muted())]),
            },
            Level3::Tasks => match self.selected_task() {
                Some(t) => (format!("task {}", short_id(t.id())), self.task_lines(t, view)),
                None => ("Task".to_string(), vec![Line::styled("Select a task", theme::muted())]),
            },
        };
        let mut block = widgets::panel(Line::from(format!(" {title} ")).bold(), false);
        let busy = self.selected_service().is_some_and(|s| self.reporting.contains(&s.service_name)) && self.level != Level3::Clusters;
        if busy || self.log_loading {
            let what = if busy { " writing the report " } else { " reading the log " };
            block = block.title_top(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::styled(what, theme::muted())]).right_aligned());
        }
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
    }

    fn cluster_lines(&self, c: &Cluster) -> Vec<Line<'static>> {
        let kw = 20;
        let mut lines = vec![
            widgets::kv("Status", c.status.clone(), kw),
            widgets::kv("Services", c.active_services_count.to_string(), kw),
            widgets::kv("Tasks", format!("{} running · {} pending", c.running_tasks_count, c.pending_tasks_count), kw),
        ];
        if c.registered_container_instances_count > 0 {
            lines.push(widgets::kv("EC2 instances", c.registered_container_instances_count.to_string(), kw));
        }
        if !c.capacity_providers.is_empty() {
            lines.push(widgets::kv("Capacity", c.capacity_providers.join(", "), kw));
        }
        lines.push(widgets::kv("Container Insights", c.container_insights().to_string(), kw));
        lines.push(widgets::kv("ARN", Span::styled(c.cluster_arn.clone(), theme::muted()), kw));
        lines.push(Line::raw(""));
        lines.push(Line::styled("Enter shows its services", theme::muted()));
        lines
    }

    fn service_lines(&self, s: &EcsService, width: usize, view: &View) -> Vec<Line<'static>> {
        let kw = 16;
        let now = view.now;
        let state = s.state(now);
        let mut lines = vec![Line::from(vec![
            Span::styled(format!("{} {}", state.symbol(), state.label()), Style::new().fg(state_color(state)).bold()),
            Span::styled(format!("   {} running of {} desired · {} pending", s.running_count, s.desired_count, s.pending_count), theme::muted()),
        ])];
        if let Some(f) = self.follow_of(&s.service_name) {
            let doing = match ecs::progress(s, f.deployment.as_deref(), f.desired) {
                Progress::Waiting(t) => t,
                Progress::Done(t) | Progress::Failed(t) => t,
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()),
                Span::styled(format!("following {} · {doing}", f.what), theme::accent()),
            ]));
        }
        lines.push(Line::raw(""));
        let def = self.definition(&s.task_definition);
        let image = def.map(|d| format!(" · {}", d.main_image())).unwrap_or_default();
        lines.push(widgets::kv("Task definition", format!("{}{image}", ecs::revision_of(&s.task_definition)), kw));
        if let Some(d) = def
            && let (Some(cpu), Some(mem)) = (&d.cpu, &d.memory)
        {
            lines.push(widgets::kv("Size", format!("cpu {cpu} · memory {mem} MiB"), kw));
        }
        lines.push(widgets::kv("Launch", s.launch(), kw));
        let controller = match s.controller() {
            "ECS" => Span::raw("ECS (rolling)"),
            other => Span::styled(format!("{other} · D and R go through it, s works"), Style::new().fg(theme::WARN)),
        };
        lines.push(widgets::kv("Deploys with", controller, kw));
        lines.push(widgets::kv("Circuit breaker", breaker_span(s), kw));
        for lb in &s.load_balancers {
            let tg = lb.target_group_arn.as_deref().map(|a| a.split('/').nth(1).unwrap_or(a).to_string()).or(lb.load_balancer_name.clone()).unwrap_or_default();
            lines.push(widgets::kv("Target group", format!("{tg} → {}:{}", lb.container_name.clone().unwrap_or_default(), lb.container_port.unwrap_or_default()), kw));
        }

        lines.push(Line::raw(""));
        lines.push(widgets::section(&format!("Deployments ({})", s.deployments.len())));
        if s.deployments.is_empty() {
            lines.push(Line::styled(format!("None listed: {} deploys this service.", s.controller()), theme::muted()));
        }
        for d in &s.deployments {
            let (label, color) = match d.rollout() {
                "FAILED" => ("failed", theme::ERR),
                "IN_PROGRESS" => ("in progress", theme::RUNNING),
                "COMPLETED" => ("completed", theme::OK),
                _ => ("—", theme::MUTED),
            };
            let mut spans = vec![
                Span::styled(format!("{:<8}", d.status.to_lowercase()), theme::bold()),
                Span::styled(format!("{label:<12}"), Style::new().fg(color)),
                Span::raw(format!("{:<20}", ellipsize(ecs::revision_of(&d.task_definition), 20))),
                Span::styled(format!("{}/{} running", d.running_count, d.desired_count), theme::muted()),
            ];
            if d.failed_tasks > 0 {
                spans.push(Span::styled(format!(" · {} failed", d.failed_tasks), Style::new().fg(theme::ERR)));
            }
            if let Some(t) = d.created_at {
                spans.push(Span::styled(format!(" · {}", fmt_ago(t.min(now))), theme::muted()));
            }
            lines.push(Line::from(spans));
            if d.rollout() == "FAILED"
                && let Some(r) = &d.rollout_state_reason
            {
                lines.push(Line::styled(format!("  {}", sanitize(r)), Style::new().fg(theme::ERR)));
            }
        }

        lines.push(Line::raw(""));
        lines.extend(self.metric_lines(s, width, view));

        lines.push(Line::raw(""));
        lines.push(widgets::section("Events"));
        for e in s.events.iter().take(10) {
            let color = if e.message.contains("failed") || e.message.contains("unable") || e.message.contains("rolling back") {
                theme::WARN
            } else {
                Color::Reset
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{:>8}  ", e.created_at.map(|t| fmt_ago(t.min(now))).unwrap_or_default()), theme::muted()),
                Span::styled(sanitize(&e.message), Style::new().fg(color)),
            ]));
        }
        if s.events.is_empty() {
            lines.push(Line::styled("none", theme::muted()));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("Enter shows its tasks and why they stopped", theme::muted()));
        lines
    }

    fn metric_lines(&self, s: &EcsService, width: usize, view: &View) -> Vec<Line<'static>> {
        let mut ranges = vec![widgets::section("CPU and memory").spans.remove(0), Span::raw("   ")];
        for r in [Range::Hour, Range::Day, Range::Week] {
            let style = if r == self.range { theme::accent().bold() } else { theme::muted() };
            ranges.push(Span::styled(format!("{} ", r.label()), style));
        }
        ranges.push(Span::styled("  m changes", theme::muted()));
        let mut lines = vec![Line::from(ranges)];
        let key = (s.cluster_name().to_string(), s.service_name.clone(), self.range);
        let m = match self.metrics.get(&key) {
            Some((_, Ok(m))) => m,
            Some((_, Err(e))) => {
                lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR)));
                return lines;
            }
            None => {
                lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" loading metrics…")]));
                return lines;
            }
        };
        if m.points("cpu").is_empty() && m.points("memory").is_empty() {
            lines.push(Line::styled(format!("No data in the last {} (no tasks running?).", self.range.label()), theme::muted()));
            return lines;
        }
        for (id, max_id, title, color) in [("cpu", "cpu_max", "CPU", theme::ACCENT), ("memory", "memory_max", "Memory", theme::RUNNING)] {
            let mut chart = Chart::new(title, m.points(id), m.window);
            chart.agg = Agg::Max;
            chart.color = color;
            chart.fmt = ecs::fmt_percent;
            chart.threshold = Some(100.0);
            chart.rows = 3;
            chart.axis = id == "memory";
            let peak = m.max(max_id).or(m.max(id)).unwrap_or_default();
            let hot = peak >= 90.0;
            let summary = format!("avg {} · peak {} of what the tasks reserve", ecs::fmt_percent(m.mean(id).unwrap_or_default()), ecs::fmt_percent(peak));
            chart.summary = vec![Span::styled(summary, if hot { Style::new().fg(theme::ERR) } else { theme::muted() })];
            lines.extend(chart.lines(width));
        }
        lines
    }

    fn task_lines(&self, t: &Task, view: &View) -> Vec<Line<'static>> {
        let kw = 14;
        let now = view.now;
        let (symbol, color) = task_symbol(t);
        let status = match t.last_status.as_str() {
            "STOPPED" => "stopped".to_string(),
            last if t.stopped() => format!("stopping ({})", last.to_lowercase()),
            last => last.to_lowercase(),
        };
        let mut lines = vec![Line::from(vec![Span::styled(format!("{symbol} {status}"), Style::new().fg(color).bold())])];
        lines.push(Line::raw(""));
        lines.push(widgets::kv("Task", t.id().to_string(), kw));
        lines.push(widgets::kv("Revision", ecs::revision_of(&t.task_definition_arn).to_string(), kw));
        if let Some(at) = t.started_at {
            lines.push(widgets::kv("Started", format!("{} · {}", crate::util::fmt_utc(at), fmt_ago(at.min(now))), kw));
        }
        if let Some(at) = t.stopped_at {
            lines.push(widgets::kv("Stopped", format!("{} · {}", crate::util::fmt_utc(at), fmt_ago(at.min(now))), kw));
        }
        let place = [t.launch_type.clone(), t.availability_zone.clone(), t.private_ip().map(str::to_string)].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        lines.push(widgets::kv("Runs on", place, kw));
        if let (Some(cpu), Some(mem)) = (&t.cpu, &t.memory) {
            lines.push(widgets::kv("Size", format!("cpu {cpu} · memory {mem} MiB"), kw));
        }
        if let Some(h) = t.health_status.as_deref().filter(|h| *h != "UNKNOWN") {
            lines.push(widgets::kv("Health", h.to_string(), kw));
        }

        if t.stopped() {
            lines.push(Line::raw(""));
            lines.push(widgets::section("Why it stopped"));
            lines.push(widgets::kv("Stop code", t.stop_code.clone().unwrap_or_else(|| "—".into()), kw));
            lines.push(widgets::kv("Reason", Span::styled(sanitize(t.stopped_reason.as_deref().unwrap_or("—")), Style::new().fg(theme::WARN)), kw));
            if let Some(h) = t.hint() {
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![Span::styled("→ ", theme::accent()), Span::styled(h, theme::accent())]));
            }
        }

        lines.push(Line::raw(""));
        lines.push(widgets::section(&format!("Containers ({})", t.containers.len())));
        let def = self.definition(&t.task_definition_arn);
        for c in &t.containers {
            let failed = c.exit_code.is_some_and(|e| e != 0) || c.reason.is_some();
            let mut spans = vec![
                Span::styled(format!("{:<16}", ellipsize(&c.name, 16)), theme::bold()),
                Span::styled(c.last_status.to_lowercase(), theme::muted()),
            ];
            if let Some(code) = c.exit_code {
                spans.push(Span::styled(format!(" · exit {code}"), Style::new().fg(if code == 0 { theme::OK } else { theme::ERR })));
            }
            if let Some(h) = c.health_status.as_deref().filter(|h| *h != "UNKNOWN") {
                spans.push(Span::styled(format!(" · {}", h.to_lowercase()), Style::new().fg(if h == "HEALTHY" { theme::OK } else { theme::WARN })));
            }
            if def.and_then(|d| d.container(&c.name)).is_some_and(|d| !d.essential()) {
                spans.push(Span::styled(" · not essential", theme::muted()));
            }
            lines.push(Line::from(spans));
            if let Some(r) = &c.reason {
                lines.push(Line::styled(format!("  {}", sanitize(r)), Style::new().fg(if failed { theme::ERR } else { theme::WARN })));
            }
            if let Some(img) = &c.image {
                lines.push(Line::styled(format!("  {img}"), theme::muted()));
            }
            if let Some(d) = def {
                let log = match d.log_stream(&c.name, t.id()) {
                    Some(l) => format!("  logs {} › {}", l.group, l.stream),
                    None => match d.container(&c.name).and_then(|x| x.log_driver()) {
                        Some("awslogs") => "  logs: awslogs without a stream prefix (l opens the group)".to_string(),
                        Some(driver) => format!("  logs with {driver}, not CloudWatch Logs"),
                        None => "  no log configuration".to_string(),
                    },
                };
                lines.push(Line::styled(log, theme::muted()));
            }
        }

        lines.push(Line::raw(""));
        match self.defs.get(&t.task_definition_arn) {
            None => lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" reading the task definition…")])),
            Some(Err(e)) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            Some(Ok(d)) => lines.extend(definition_lines(d)),
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("Enter reads a container's last log lines", theme::muted()));
        lines
    }
}

/// The task definition: its containers' images, sizes, ports, variables (by name), secrets
/// (where they come from) and health checks.
fn definition_lines(d: &TaskDefinition) -> Vec<Line<'static>> {
    let kw = 14;
    let mut lines = vec![widgets::section(&format!("Task definition {}", ecs::revision_of(&d.task_definition_arn)))];
    let size = [d.cpu.as_ref().map(|c| format!("cpu {c}")), d.memory.as_ref().map(|m| format!("memory {m} MiB")), d.network_mode.clone()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    if !size.is_empty() {
        lines.push(widgets::kv("Size", size, kw));
    }
    if let Some(r) = &d.task_role_arn {
        lines.push(widgets::kv("Task role", Span::styled(r.rsplit('/').next().unwrap_or(r).to_string(), theme::muted()), kw));
    }
    for c in &d.container_definitions {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(c.name.clone(), theme::bold()),
            Span::styled(if c.essential() { "  essential" } else { "  not essential" }, theme::muted()),
        ]));
        lines.push(widgets::kv("  Image", c.image.clone(), kw));
        let limits = [c.cpu.map(|v| format!("cpu {v}")), c.memory.map(|v| format!("memory {v} MiB")), c.memory_reservation.map(|v| format!("reserves {v} MiB"))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        if !limits.is_empty() {
            lines.push(widgets::kv("  Limits", limits, kw));
        }
        if !c.port_mappings.is_empty() {
            let ports = c.port_mappings.iter().filter_map(|p| p.container_port.map(|n| format!("{n}/{}", p.protocol.as_deref().unwrap_or("tcp")))).collect::<Vec<_>>().join(", ");
            lines.push(widgets::kv("  Ports", ports, kw));
        }
        if !c.environment.is_empty() {
            let names = c.environment.iter().map(|e| e.name.clone()).collect::<Vec<_>>().join(", ");
            lines.push(widgets::kv("  Variables", Span::raw(format!("{names}  (values not shown)")), kw));
        }
        for s in &c.secrets {
            lines.push(widgets::kv("  Secret", Span::styled(format!("{} ← {}", s.name, s.value_from), theme::muted()), kw));
        }
        if let Some(h) = &c.health_check {
            lines.push(widgets::kv("  Health check", h.command.join(" "), kw));
        }
    }
    lines
}

fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

fn state_color(s: State) -> Color {
    match s {
        State::Failed => theme::ERR,
        State::Degraded => theme::WARN,
        State::Deploying => theme::RUNNING,
        State::Steady => theme::OK,
        State::Idle => theme::MUTED,
    }
}

fn task_symbol(t: &Task) -> (&'static str, Color) {
    if !t.stopped() {
        return match t.last_status.as_str() {
            "RUNNING" => ("●", theme::OK),
            _ => ("◐", theme::RUNNING),
        };
    }
    if t.failed_containers().any(|c| c.exit_code.is_some_and(|e| e != 0 && e != 143)) || t.stopped_reason.as_deref().is_some_and(|r| r.contains("Error")) {
        ("✗", theme::ERR)
    } else {
        ("○", theme::MUTED)
    }
}

fn breaker_span(s: &EcsService) -> Span<'static> {
    match s.circuit_breaker() {
        Some(c) if c.rollback => Span::styled("on · rolls back failed deployments", Style::new().fg(theme::OK)),
        Some(_) => Span::styled("on · stops failed deployments (no rollback)", Style::new().fg(theme::WARN)),
        None => Span::styled("off · a failing deployment keeps trying", theme::muted()),
    }
}

fn breaker_line(s: &EcsService) -> String {
    match s.circuit_breaker() {
        Some(c) if c.rollback => "If its tasks keep failing, the circuit breaker rolls it back.".into(),
        Some(_) => "If its tasks keep failing, the circuit breaker stops it (no rollback).".into(),
        None => "The circuit breaker is off: if its tasks fail, ECS keeps trying.".into(),
    }
}

/// ECS's refusal, said for the user.
fn refusal(e: &AwsError) -> String {
    match e {
        AwsError::Api { code, message, .. } if code == "InvalidParameterException" || code == "ClientException" => {
            let text = message.rsplit_once("Exception: ").map_or(message.as_str(), |(_, m)| m).trim_end_matches(['\'', ')', '"']);
            format!("ECS refused it: {}", sanitize(text))
        }
        AwsError::Api { code, .. } if code == "AccessDeniedException" => {
            format!("Not allowed: {} (ecs:UpdateService)", e.short())
        }
        _ => e.short(),
    }
}

impl Module for Ecs {
    fn title(&self) -> &'static str {
        "ECS"
    }

    fn description(&self) -> &'static str {
        "ECS and Fargate · services, deployments, why tasks stopped, logs, redeploy, scale, roll back"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        let env = ctx.env();
        *self = Ecs::new();
        self.env_name = env.name.clone();
        self.region = env.region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.clusters_load, LoadState::Idle) {
            self.load_clusters(ctx);
        }
        self.metrics.retain(|_, (at, _)| at.elapsed() < METRICS_FRESH);
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some((_, view)) = &mut self.log {
            if let LogKey::Copy = view.key(key) {
                let ok = crate::platform::copy_to_clipboard(&view.lines().join("\n"));
                ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Copied" } else { "Clipboard not available" });
            }
            if !view.zoomed {
                self.log = None;
            }
            return Handled::Yes;
        }
        if self.dialog.is_some() {
            self.dialog_key(key, ctx);
            return Handled::Yes;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        if self.filtering {
            match self.filter.handle(key) {
                InputEvent::Changed => {
                    self.rebuild();
                    self.select(0);
                }
                InputEvent::Submit => self.filtering = false,
                InputEvent::Cancel => {
                    self.filtering = false;
                    self.filter.clear();
                    self.rebuild();
                }
                InputEvent::Ignored => match key.code {
                    KeyCode::Up => self.select(selected.saturating_sub(1)),
                    KeyCode::Down => self.select(selected + 1),
                    _ => {}
                },
                InputEvent::Moved => {}
            }
            return Handled::Yes;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.rebuild();
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Backspace if self.level != Level3::Clusters => self.back(),
            KeyCode::Enter | KeyCode::Right => match self.level {
                Level3::Clusters => {
                    if let Some(name) = self.selected_cluster().map(|c| c.cluster_name.clone()) {
                        self.open_cluster(name, ctx);
                    }
                }
                Level3::Services => self.open_service(ctx),
                Level3::Tasks if key.code == KeyCode::Enter => self.open_task_log(ctx),
                Level3::Tasks => {}
            },
            KeyCode::Char('m') if self.level == Level3::Services => {
                self.range = self.range.next();
                self.fetch_metrics(ctx);
            }
            KeyCode::Char('D') if self.level != Level3::Clusters => self.open_redeploy(ctx),
            KeyCode::Char('s') if self.level != Level3::Clusters => self.open_desired(),
            KeyCode::Char('R') if self.level != Level3::Clusters => self.open_revisions(ctx),
            KeyCode::Char('c') if self.level != Level3::Clusters => self.copy_report(ctx),
            KeyCode::Char('l') if self.level != Level3::Clusters => self.jump_to_logs(ctx),
            KeyCode::Char('y') => {
                let arn = match self.level {
                    Level3::Clusters => self.selected_cluster().map(|c| c.cluster_arn.clone()),
                    Level3::Services => self.selected_service().map(|s| s.service_arn.clone()),
                    Level3::Tasks => self.selected_task().map(|t| t.task_arn.clone()),
                };
                if let Some(arn) = arn {
                    self.copy(&arn, "the ARN", ctx);
                }
            }
            KeyCode::Char('o') => self.open_console(ctx),
            KeyCode::Char('r') => self.refresh(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        let pos = (event.column, event.row).into();
        if self.dialog.is_some() || self.log.is_some() || !self.list_area.contains(pos) {
            return Handled::No;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = event.row.saturating_sub(self.list_area.y + 2) as usize;
                self.select(self.list_state.offset() + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    /// `cluster` or `cluster/service` (from the `:` search, say).
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        let (cluster, service) = match target.split_once('/') {
            Some((c, s)) => (c.to_string(), Some(s.to_string())),
            None => (target.to_string(), None),
        };
        self.log = None;
        self.dialog = None;
        self.open_target = Some((cluster, service));
        if matches!(self.clusters_load, LoadState::Idle) {
            self.load_clusters(ctx);
        }
        self.try_open(ctx);
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Clusters(result) => {
                match result {
                    Ok(list) => {
                        self.clusters = list;
                        self.clusters_load = LoadState::Loaded;
                    }
                    Err(e) => self.clusters_load = LoadState::Failed(e),
                }
                if self.level == Level3::Clusters {
                    self.rebuild();
                    if self.list_state.selected().is_none() {
                        self.select(0);
                    }
                }
                if !self.try_open(ctx) && self.open_target.is_none() && self.level == Level3::Clusters && self.clusters.len() == 1 && self.cluster.is_none() {
                    // One cluster: straight to its services.
                    let name = self.clusters[0].cluster_name.clone();
                    self.open_cluster(name, ctx);
                }
            }
            Msg::Services { cluster, result } => {
                if self.cluster.as_deref() != Some(cluster.as_str()) {
                    return;
                }
                match result {
                    Ok(list) => {
                        self.services = list;
                        self.services_load = LoadState::Loaded;
                    }
                    Err(e) => self.services_load = LoadState::Failed(e),
                }
                if self.level == Level3::Services {
                    self.rebuild();
                    if self.list_state.selected().is_none() {
                        self.select(0);
                    }
                    self.pending = Some(Instant::now());
                }
                self.try_open(ctx);
            }
            Msg::Tasks { cluster, service, result } => {
                if self.cluster.as_deref() != Some(cluster.as_str()) || self.service.as_deref() != Some(service.as_str()) {
                    return;
                }
                match result {
                    Ok(list) => {
                        self.tasks = list;
                        self.tasks_load = LoadState::Loaded;
                    }
                    Err(e) => self.tasks_load = LoadState::Failed(e),
                }
                if self.level == Level3::Tasks {
                    self.rebuild();
                    if self.list_state.selected().is_none() {
                        self.select(0);
                    }
                    self.pending = Some(Instant::now());
                }
            }
            Msg::Definition { arn, result } => {
                self.defs_in_flight.remove(&arn);
                self.defs.insert(arn, *result);
            }
            Msg::Metrics { key, result } => {
                self.metrics_in_flight.remove(&key);
                self.metrics.insert(key, (Instant::now(), *result));
            }
            Msg::Revisions { cluster, service, result } => self.on_revisions(cluster, service, result, ctx),
            Msg::Updated { cluster, change, result } => self.on_updated(cluster, change, *result, ctx),
            Msg::Followed { id, result } => self.on_followed(id, *result, ctx),
            Msg::Log { title, result } => {
                self.log_loading = false;
                match result {
                    Ok(lines) if lines.is_empty() => ctx.toast(Level::Info, "That log stream has no lines"),
                    Ok(lines) => {
                        let mut view = LogView::default();
                        view.extend(lines);
                        view.zoomed = true;
                        self.log = Some((title, view));
                    }
                    Err(e) => ctx.toast(Level::Error, format!("Could not read the log: {}", e.short())),
                }
            }
            Msg::Report { service, result } => self.on_report(service, result, ctx),
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if let Some(at) = self.pending
            && at.elapsed() >= DEBOUNCE
        {
            self.pending = None;
            if active {
                self.settle(ctx);
            }
        }
        if !self.follows.is_empty() {
            self.poll_follows(ctx);
        }
        // Lists shown stay current.
        if active && self.dialog.is_none() && self.loaded_at.elapsed() >= REFRESH {
            match self.level {
                Level3::Services if !matches!(self.services_load, LoadState::Loading) => self.load_services(ctx),
                Level3::Tasks if !matches!(self.tasks_load, LoadState::Loading) => self.load_tasks(ctx),
                _ => self.loaded_at = Instant::now(),
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some((title, log)) = &mut self.log {
            let title = title.clone();
            log.render_zoomed(f, area, &title);
            return;
        }
        let [left, right] = if area.width < 90 {
            area.layout(&Layout::vertical([Constraint::Percentage(40), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(36), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        self.render_detail(f, right, view);
        let color = if is_production(&self.env_name) { theme::ERR } else { theme::ACCENT };
        match &mut self.dialog {
            Some(Dialog::Revisions { picker, .. }) | Some(Dialog::Container { picker, .. }) => {
                let rect = widgets::popup_area(area, 90, picker.height(area.height.saturating_sub(4)));
                picker.render(f, rect, color);
            }
            Some(Dialog::Desired { form, .. }) => form.render(f, area, color),
            Some(Dialog::Confirm { title, lines, typed, busy, error, .. }) => {
                let mut text: Vec<Line> = vec![Line::raw("")];
                text.extend(lines.iter().map(|l| Line::raw(l.clone())));
                text.push(Line::raw(""));
                let mut cursor = None;
                if let Some((name, input)) = typed {
                    text.push(Line::styled(format!("Type '{name}' to confirm"), Style::new().fg(theme::WARN)));
                    let (line, col) = input.line("› ", "");
                    cursor = Some((text.len() as u16, col));
                    text.push(line);
                    text.push(Line::raw(""));
                }
                if let Some(e) = error {
                    text.push(Line::styled(format!("✗ {e}"), Style::new().fg(theme::ERR)));
                    text.push(Line::raw(""));
                }
                if *busy {
                    text.push(Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw("Asking ECS…")]));
                } else {
                    text.push(widgets::hints_line(&[("Enter", "confirm"), ("Esc", "cancel")]));
                }
                let rect = widgets::popup_area(area, 84, widgets::wrapped_height(&text, 80) + 2);
                widgets::popup(f, rect, title, color, Text::from(text));
                if let Some((row, col)) = cursor {
                    f.set_cursor_position((rect.x + 2 + col, rect.y + 1 + row));
                }
            }
            None => {}
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.log.is_some() {
            return vec![("/", "search"), ("y", "copy"), ("Esc", "back")];
        }
        match &self.dialog {
            Some(Dialog::Confirm { .. }) => return vec![("Enter", "confirm"), ("Esc", "cancel")],
            Some(Dialog::Desired { .. }) => return vec![("Tab", "next field"), ("Enter", "apply"), ("Esc", "cancel")],
            Some(_) => return vec![("type", "filter"), ("↑↓", "move"), ("Enter", "choose"), ("Esc", "cancel")],
            None => {}
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        match self.level {
            Level3::Clusters => vec![("↑↓", "move"), ("Enter", "services"), ("/", "search"), ("y", "copy ARN"), ("o", "console"), ("r", "refresh")],
            Level3::Services => vec![
                ("↑↓", "move"),
                ("Enter", "tasks"),
                ("Esc", "clusters"),
                ("/", "search"),
                ("m", "1h/24h/7d"),
                ("D", "redeploy"),
                ("s", "scale"),
                ("R", "revision/rollback"),
                ("c", "copy for AI"),
                ("l", "logs"),
                ("y", "copy ARN"),
                ("o", "console"),
                ("r", "refresh"),
                ("PgUp/PgDn", "scroll"),
            ],
            Level3::Tasks => vec![
                ("↑↓", "move"),
                ("Enter", "container log"),
                ("Esc", "services"),
                ("/", "search"),
                ("l", "log group"),
                ("D", "redeploy"),
                ("s", "scale"),
                ("R", "revision/rollback"),
                ("c", "copy for AI"),
                ("y", "copy ARN"),
                ("o", "console"),
                ("r", "refresh"),
                ("PgUp/PgDn", "scroll"),
            ],
        }
    }

    fn animating(&self) -> bool {
        matches!(self.clusters_load, LoadState::Loading)
            || matches!(self.services_load, LoadState::Loading)
            || matches!(self.tasks_load, LoadState::Loading)
            || self.pending.is_some()
            || !self.metrics_in_flight.is_empty()
            || !self.defs_in_flight.is_empty()
            || !self.follows.is_empty()
            || !self.reporting.is_empty()
            || self.log_loading
            || matches!(self.dialog, Some(Dialog::Confirm { busy: true, .. }) | Some(Dialog::Desired { busy: true, .. }))
    }

    fn capturing(&self) -> bool {
        self.filtering || self.dialog.is_some() || self.log.is_some()
    }

    fn wants_mouse(&self) -> bool {
        self.log.is_none()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.dialog {
            Some(Dialog::Desired { form, busy: false, .. }) => form.paste(text),
            Some(Dialog::Confirm { typed: Some((_, input)), busy: false, .. }) => input.insert_str(text, false),
            Some(Dialog::Revisions { picker, .. }) | Some(Dialog::Container { picker, .. }) => picker.paste(text),
            Some(_) => {}
            None if self.filtering => {
                self.filter.insert_str(text, false);
                self.rebuild();
            }
            None => {}
        }
    }
}

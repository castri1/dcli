//! Step Functions: state machines → their executions (filtered by status) → the steps of
//! an execution, where it failed and why. Opens the logs of a failed Lambda step or of the
//! machine, copies a failure report for an AI agent, and redrives, starts and stops
//! executions.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::sfn::{self, Execution, ExecutionDetail, HistoryEvent, MachineDetail, StateMachine, Step, StepStatus};
use crate::aws;
use crate::config::is_production;
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, fmt_duration, fmt_utc, pretty_json};

const DEBOUNCE: Duration = Duration::from_millis(200);
/// Executions listed per machine.
const MAX_EXECUTIONS: usize = 100;
/// How often a running execution (and a list with running ones) refreshes.
const DETAIL_REFRESH: Duration = Duration::from_secs(5);
const LIST_REFRESH: Duration = Duration::from_secs(15);

/// Status filters `f` cycles through: (API value, label).
const FILTERS: [(Option<&str>, &str); 6] = [
    (None, "all"),
    (Some("RUNNING"), "running"),
    (Some("FAILED"), "failed"),
    (Some("SUCCEEDED"), "succeeded"),
    (Some("TIMED_OUT"), "timed out"),
    (Some("ABORTED"), "aborted"),
];

/// An execution's description, history and the steps built from it.
struct Detail {
    detail: ExecutionDetail,
    events: Vec<HistoryEvent>,
    steps: Vec<Step>,
    loaded: Instant,
}

enum Msg {
    Machines(aws::Result<Vec<StateMachine>>),
    Machine { arn: String, result: aws::Result<MachineDetail> },
    Executions { seq: u64, result: aws::Result<Vec<Execution>> },
    Detail { arn: String, result: Box<aws::Result<(ExecutionDetail, Vec<HistoryEvent>)>> },
    /// An action finished: what to say, and the execution to select afterwards.
    Acted { result: aws::Result<(String, Option<String>)> },
}

#[derive(PartialEq)]
enum Screen {
    Machines,
    Executions,
}

#[derive(PartialEq)]
enum Focus {
    List,
    Steps,
}

#[derive(Clone)]
enum Action {
    Start { machine_arn: String },
    Stop { execution_arn: String },
    Redrive { execution_arn: String },
}

/// A form (start, stop) or a yes/no confirmation (redrive), with the action it runs.
enum Dialog {
    Form { form: Form, action: Action, busy: bool },
    Confirm { title: String, lines: Vec<String>, typed: Option<(String, TextInput)>, action: Action, busy: bool, error: Option<String> },
}

pub struct StepFunctions {
    machines: Vec<StateMachine>,
    load: LoadState,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    machine_state: ListState,
    machine_details: HashMap<String, aws::Result<MachineDetail>>,
    machine_in_flight: HashSet<String>,
    screen: Screen,
    /// The machine whose executions are shown.
    machine: Option<StateMachine>,
    executions: Vec<Execution>,
    exec_load: LoadState,
    exec_state: ListState,
    exec_seq: u64,
    status_filter: usize,
    listed_at: Instant,
    /// Execution to select once the list loads (after starting one).
    reselect: Option<String>,
    details: HashMap<String, aws::Result<Detail>>,
    detail_in_flight: HashSet<String>,
    pending_detail: Option<(String, Instant)>,
    focus: Focus,
    step_cursor: usize,
    detail_scroll: u16,
    /// Input / output of an execution or a step, full screen.
    zoom: Option<(String, LogView)>,
    dialog: Option<Dialog>,
    /// A jump arrived before the machines loaded.
    pending_open: Option<String>,
    env_name: String,
    region: String,
    list_area: Rect,
}

fn status_style(status: &str) -> (&'static str, Style, &'static str) {
    let (s, label) = sfn::status(status);
    (s.symbol(), theme::status_style(s), label)
}

impl StepFunctions {
    pub fn new() -> StepFunctions {
        StepFunctions {
            machines: Vec::new(),
            load: LoadState::Idle,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            machine_state: ListState::default(),
            machine_details: HashMap::new(),
            machine_in_flight: HashSet::new(),
            screen: Screen::Machines,
            machine: None,
            executions: Vec::new(),
            exec_load: LoadState::Idle,
            exec_state: ListState::default(),
            exec_seq: 0,
            status_filter: 0,
            listed_at: Instant::now(),
            reselect: None,
            details: HashMap::new(),
            detail_in_flight: HashSet::new(),
            pending_detail: None,
            focus: Focus::List,
            step_cursor: 0,
            detail_scroll: 0,
            zoom: None,
            dialog: None,
            pending_open: None,
            env_name: String::new(),
            region: String::new(),
            list_area: Rect::default(),
        }
    }

    // ---- state machines ---------------------------------------------------------------

    fn load_machines(&mut self, ctx: &mut Ctx) {
        self.load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Machines(sfn::all_state_machines(&aws)));
    }

    fn refilter(&mut self) {
        let keep = self.selected_machine().map(|m| m.state_machine_arn.clone());
        let query = self.filter.value.trim().to_lowercase();
        let mut visible: Vec<usize> = (0..self.machines.len()).filter(|&i| matches_filter(&self.machines[i].name, &query)).collect();
        visible.sort_by(|&a, &b| self.machines[a].name.cmp(&self.machines[b].name));
        self.visible = visible;
        let index = keep.and_then(|k| self.visible.iter().position(|&i| self.machines[i].state_machine_arn == k)).unwrap_or(0);
        self.machine_state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    fn selected_machine(&self) -> Option<&StateMachine> {
        self.machines.get(*self.visible.get(self.machine_state.selected()?)?)
    }

    fn select_machine(&mut self, index: usize, ctx: &mut Ctx) {
        if self.visible.is_empty() {
            return;
        }
        self.machine_state.select(Some(index.min(self.visible.len() - 1)));
        if let Some(arn) = self.selected_machine().map(|m| m.state_machine_arn.clone()) {
            self.fetch_machine(arn, ctx);
        }
    }

    fn fetch_machine(&mut self, arn: String, ctx: &mut Ctx) {
        if self.machine_details.contains_key(&arn) || !self.machine_in_flight.insert(arn.clone()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = sfn::describe_state_machine(&aws, &arn);
            Msg::Machine { arn, result }
        });
    }

    fn machine_detail(&self, arn: &str) -> Option<&MachineDetail> {
        self.machine_details.get(arn).and_then(|r| r.as_ref().ok())
    }

    /// Shows the executions of the selected machine.
    fn enter_machine(&mut self, ctx: &mut Ctx) {
        let Some(machine) = self.selected_machine().cloned() else { return };
        self.fetch_machine(machine.state_machine_arn.clone(), ctx);
        let same = self.machine.as_ref().is_some_and(|m| m.state_machine_arn == machine.state_machine_arn);
        self.machine = Some(machine);
        self.screen = Screen::Executions;
        self.focus = Focus::List;
        if !same {
            self.executions.clear();
            self.exec_state.select(None);
            self.status_filter = 0;
        }
        self.load_executions(ctx);
    }

    // ---- executions --------------------------------------------------------------------

    fn load_executions(&mut self, ctx: &mut Ctx) {
        let Some(machine) = &self.machine else { return };
        if machine.express() {
            self.exec_load = LoadState::Loaded;
            return;
        }
        self.exec_seq += 1;
        self.exec_load = LoadState::Loading;
        self.listed_at = Instant::now();
        let (aws, arn, seq) = (ctx.env().aws.clone(), machine.state_machine_arn.clone(), self.exec_seq);
        let filter = FILTERS[self.status_filter].0;
        ctx.spawn(move || Msg::Executions { seq, result: sfn::list_executions(&aws, &arn, filter, MAX_EXECUTIONS) });
    }

    fn selected_execution(&self) -> Option<&Execution> {
        self.executions.get(self.exec_state.selected()?)
    }

    fn select_execution(&mut self, index: usize) {
        if self.executions.is_empty() {
            return;
        }
        let index = index.min(self.executions.len() - 1);
        if self.exec_state.selected() != Some(index) {
            self.detail_scroll = 0;
            self.step_cursor = 0;
        }
        self.exec_state.select(Some(index));
        if let Some(e) = self.selected_execution()
            && !self.details.contains_key(&e.execution_arn)
        {
            self.pending_detail = Some((e.execution_arn.clone(), Instant::now()));
        }
    }

    fn fetch_detail(&mut self, arn: String, ctx: &mut Ctx) {
        if !self.detail_in_flight.insert(arn.clone()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = sfn::describe_execution(&aws, &arn).and_then(|d| Ok((d, sfn::execution_history(&aws, &arn)?)));
            Msg::Detail { arn, result: Box::new(result) }
        });
    }

    fn detail(&self) -> Option<&Detail> {
        self.details.get(&self.selected_execution()?.execution_arn)?.as_ref().ok()
    }

    // ---- actions -----------------------------------------------------------------------

    /// The log group for `l`: the selected step's Lambda (steps focus), else the failed
    /// Lambda step's, else the machine's logging group.
    fn log_group(&self) -> Option<String> {
        let detail = (self.screen == Screen::Executions).then(|| self.detail()).flatten();
        let step = detail.and_then(|d| {
            if self.focus == Focus::Steps {
                d.steps.get(self.step_cursor).and_then(Step::log_group)
            } else {
                let failed = d.steps.iter().rev().find(|s| s.status == StepStatus::Failed && s.lambda.is_some());
                let running = d.steps.iter().rev().find(|s| s.status == StepStatus::Running && s.lambda.is_some());
                failed.or(running).and_then(Step::log_group)
            }
        });
        let machine = match self.screen {
            Screen::Executions => self.machine.as_ref(),
            Screen::Machines => self.selected_machine(),
        };
        step.or_else(|| self.machine_detail(&machine?.state_machine_arn)?.log_group())
    }

    fn open_logs(&self, ctx: &mut Ctx) {
        match self.log_group() {
            Some(group) => ctx.jump("Logs", group),
            None => ctx.toast(Level::Info, "No Lambda step here and the state machine does not log to CloudWatch"),
        }
    }

    fn zoom_json(&mut self, title: String, json: Option<&str>, ctx: &mut Ctx) {
        let Some(json) = json.filter(|j| !j.is_empty()) else {
            return ctx.toast(Level::Info, "Nothing to show (empty, or the history has no execution data)");
        };
        let text = pretty_json(json).unwrap_or_else(|| json.to_string());
        let mut view = LogView::default();
        view.extend(text.lines().map(str::to_string));
        view.zoomed = true;
        // Start at the top, not following the end like a log.
        view.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        self.zoom = Some((title, view));
    }

    fn copy_report(&self, ctx: &mut Ctx) {
        let (Some(machine), Some(d)) = (&self.machine, self.detail()) else {
            return ctx.toast(Level::Info, "Select an execution (its details are still loading?)");
        };
        let fallback = MachineDetail {
            state_machine_arn: machine.state_machine_arn.clone(),
            name: machine.name.clone(),
            kind: machine.kind.clone(),
            ..Default::default()
        };
        let machine_detail = self.machine_detail(&machine.state_machine_arn).unwrap_or(&fallback);
        let md = sfn::report(&self.env_name, &self.region, machine_detail, &d.detail, &d.events);
        if crate::platform::copy_to_clipboard(&md) {
            return ctx.toast(Level::Success, format!("Copied the report of {}. Paste it into your AI agent.", d.detail.name));
        }
        match crate::platform::write_temp_file(&format!("{}-{}", machine.name, d.detail.name), &md) {
            Ok(path) => ctx.toast(Level::Success, format!("Saved the report to {}", path.display())),
            Err(e) => ctx.toast(Level::Error, format!("Could not save the report: {e}")),
        }
    }

    fn open_start(&mut self) {
        let machine = match self.screen {
            Screen::Executions => self.machine.clone(),
            Screen::Machines => self.selected_machine().cloned(),
        };
        let Some(machine) = machine else { return };
        let input = match self.screen {
            Screen::Executions => self.detail().and_then(|d| d.detail.input.clone()),
            Screen::Machines => None,
        };
        let mut value = Field::secret("Input", input.unwrap_or_else(|| "{}".into())).hint("JSON; paste works");
        value.masked = false;
        let mut form = Form::new(
            format!("Start execution · {}", machine.name),
            vec![Field::fixed("Machine", machine.name.clone()), Field::text("Name", "").optional("generated"), value],
        )
        .submit_label("start");
        form.note = Some(if self.screen == Screen::Executions {
            "Input copied from the selected execution; edit it before starting.".into()
        } else {
            "Starts a new execution with this input.".into()
        });
        let form = if is_production(&self.env_name) { form.confirm_with(&self.env_name) } else { form };
        self.dialog = Some(Dialog::Form { form, action: Action::Start { machine_arn: machine.state_machine_arn }, busy: false });
    }

    fn open_stop(&mut self, ctx: &mut Ctx) {
        let Some(e) = self.selected_execution().cloned() else { return };
        if e.status != "RUNNING" {
            return ctx.toast(Level::Info, format!("{} is not running", e.name));
        }
        let form = Form::new(
            format!("Stop execution · {}", e.name),
            vec![
                Field::fixed("Execution", e.name.clone()),
                Field::text("Error", "").optional("e.g. Manual.Stop"),
                Field::text("Cause", "").optional("why it was stopped"),
            ],
        )
        .submit_label("stop");
        let form = if is_production(&self.env_name) { form.confirm_with(&self.env_name) } else { form };
        self.dialog = Some(Dialog::Form { form, action: Action::Stop { execution_arn: e.execution_arn }, busy: false });
    }

    fn open_redrive(&mut self, ctx: &mut Ctx) {
        let Some(e) = self.selected_execution().cloned() else { return };
        if !matches!(e.status.as_str(), "FAILED" | "TIMED_OUT" | "ABORTED") {
            return ctx.toast(Level::Info, "Only failed, timed out or aborted executions can be redriven");
        }
        if let Some(d) = self.detail()
            && d.detail.redrive_status.as_deref().is_some_and(|s| s == "NOT_REDRIVABLE")
        {
            let why = d.detail.redrive_status_reason.clone().unwrap_or_else(|| "not redrivable".into());
            return ctx.toast(Level::Warn, format!("Cannot redrive {}: {why}", e.name));
        }
        let from = self
            .detail()
            .and_then(|d| sfn::failure(&d.steps, &d.events, Some(&d.detail)))
            .and_then(|f| f.state)
            .map(|s| format!(" from {s}"))
            .unwrap_or_default();
        let lines = vec![
            format!("Redrive {}?", e.name),
            format!("It restarts{from}, the step that failed; the steps that succeeded are not run again."),
        ];
        let typed = is_production(&self.env_name).then(|| (self.env_name.clone(), TextInput::default()));
        self.dialog = Some(Dialog::Confirm {
            title: "Redrive execution".into(),
            lines,
            typed,
            action: Action::Redrive { execution_arn: e.execution_arn },
            busy: false,
            error: None,
        });
    }

    fn run(&mut self, action: &Action, values: (String, String), ctx: &mut Ctx) {
        let aws = ctx.env().aws.clone();
        match action {
            Action::Start { machine_arn } => {
                let (name, input) = values;
                let arn = machine_arn.clone();
                ctx.spawn(move || {
                    let result = sfn::start_execution(&aws, &arn, &input, Some(&name)).map(|exec| {
                        let short = exec.rsplit(':').next().unwrap_or(&exec).to_string();
                        (format!("Started {short}"), Some(exec))
                    });
                    Msg::Acted { result }
                });
            }
            Action::Stop { execution_arn } => {
                let (error, cause) = values;
                let arn = execution_arn.clone();
                ctx.spawn(move || {
                    let result = sfn::stop_execution(&aws, &arn, &error, &cause).map(|_| ("Stop requested".into(), Some(arn)));
                    Msg::Acted { result }
                });
            }
            Action::Redrive { execution_arn } => {
                let arn = execution_arn.clone();
                ctx.spawn(move || {
                    let result = sfn::redrive_execution(&aws, &arn).map(|_| ("Redrive started".into(), Some(arn)));
                    Msg::Acted { result }
                });
            }
        }
    }

    fn dialog_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        enum Next {
            Stay,
            Close,
            Run(Action, (String, String)),
        }
        let Some(dialog) = &mut self.dialog else { return };
        let next = match dialog {
            Dialog::Form { busy: true, .. } | Dialog::Confirm { busy: true, .. } => Next::Stay,
            Dialog::Form { form, action, busy } => match form.handle(key) {
                FormEvent::Cancel => Next::Close,
                FormEvent::Pending => Next::Stay,
                FormEvent::Submit => {
                    let values = match action {
                        Action::Start { .. } => {
                            let input = match form.get("Input").trim() {
                                "" => "{}".to_string(),
                                text => text.to_string(),
                            };
                            match serde_json::from_str::<serde_json::Value>(&input) {
                                Ok(_) => Some((form.get("Name").trim().to_string(), input)),
                                Err(e) => {
                                    form.fail(format!("Input is not valid JSON: {e}"));
                                    None
                                }
                            }
                        }
                        _ => Some((form.get("Error").trim().to_string(), form.get("Cause").trim().to_string())),
                    };
                    match values {
                        Some(values) => {
                            *busy = true;
                            Next::Run(action.clone(), values)
                        }
                        None => Next::Stay,
                    }
                }
            },
            Dialog::Confirm { typed: Some((name, input)), action, busy, error, .. } => match input.handle(key) {
                InputEvent::Cancel => Next::Close,
                InputEvent::Submit if input.value.trim() == name.as_str() => {
                    *busy = true;
                    Next::Run(action.clone(), Default::default())
                }
                InputEvent::Submit => {
                    *error = Some(format!("Type '{name}' to confirm"));
                    Next::Stay
                }
                _ => Next::Stay,
            },
            Dialog::Confirm { action, busy, .. } => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    *busy = true;
                    Next::Run(action.clone(), Default::default())
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => Next::Close,
                _ => Next::Stay,
            },
        };
        match next {
            Next::Stay => {}
            Next::Close => self.dialog = None,
            Next::Run(action, values) => self.run(&action, values, ctx),
        }
    }

    fn on_acted(&mut self, result: aws::Result<(String, Option<String>)>, ctx: &mut Ctx) {
        match result {
            Ok((text, execution)) => {
                self.dialog = None;
                ctx.toast(Level::Success, text);
                if let Some(arn) = execution {
                    self.details.remove(&arn);
                    self.reselect = Some(arn);
                }
                if self.screen == Screen::Machines {
                    self.enter_machine(ctx);
                } else {
                    self.load_executions(ctx);
                }
            }
            Err(e) => match &mut self.dialog {
                Some(Dialog::Form { form, busy, .. }) => {
                    *busy = false;
                    form.fail(e.short());
                }
                Some(Dialog::Confirm { busy, error, .. }) => {
                    *busy = false;
                    *error = Some(e.short());
                }
                None => ctx.toast(Level::Error, e.short()),
            },
        }
    }

    // ---- rendering ---------------------------------------------------------------------

    fn render_machines(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search state machines");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = match &self.load {
            LoadState::Loading => format!(" {} State machines ", theme::spinner(view.tick)),
            _ => format!(" State machines · {} ", self.visible.len()),
        };
        if self.visible.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading state machines…", theme::muted()),
                _ => Line::styled("No state machines", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|&i| {
                let m = &self.machines[i];
                let badge = if m.express() { "express" } else { "" };
                let name = ellipsize(&m.name, width.saturating_sub(badge.len() + 2));
                let pad = width.saturating_sub(name.chars().count() + badge.len());
                ListItem::new(Line::from(vec![
                    Span::raw(name),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(badge, Style::new().fg(theme::APPROVAL)),
                ]))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.machine_state);
    }

    fn render_machine_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(m) = self.selected_machine() else {
            f.render_widget(
                widgets::placeholder(Line::styled("Select a state machine", theme::muted())).block(widgets::panel(" State machine ", false)),
                area,
            );
            return;
        };
        let kw = 11;
        let mut lines = vec![widgets::kv("Type", Span::styled(m.kind.clone(), theme::bold()), kw)];
        match self.machine_details.get(&m.state_machine_arn) {
            Some(Ok(d)) => {
                lines.push(widgets::kv("Status", d.status.clone().unwrap_or_default(), kw));
                if let Some(t) = d.creation_date {
                    lines.push(widgets::kv("Created", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
                }
                if let Some(r) = &d.role_arn {
                    lines.push(widgets::kv("Role", r.rsplit('/').next().unwrap_or(r).to_string(), kw));
                }
                let logging = match (d.log_group(), d.logging_configuration.as_ref().and_then(|l| l.level.clone())) {
                    (Some(g), level) => format!("{} · {g}", level.unwrap_or_default()),
                    _ => "off".into(),
                };
                lines.push(widgets::kv("Logging", logging, kw));
                if let Some(desc) = d.description.as_ref().filter(|d| !d.is_empty()) {
                    lines.push(widgets::kv("Description", desc.clone(), kw));
                }
            }
            Some(Err(e)) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            None => lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" loading…")])),
        }
        lines.push(Line::raw(""));
        if m.express() {
            lines.push(Line::styled("Express workflows do not keep a list of executions:", theme::muted()));
            lines.push(Line::styled("their history is in the logs (l), when logging is on.", theme::muted()));
        } else {
            lines.push(Line::styled("Enter shows its executions · n starts one · l logs · o console", theme::muted()));
        }
        let block = widgets::panel(Line::from(format!(" {} ", m.name)).bold(), false);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
    }

    fn render_executions(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let machine = self.machine.as_ref().map(|m| m.name.clone()).unwrap_or_default();
        let filter = FILTERS[self.status_filter].1;
        let title = match &self.exec_load {
            LoadState::Loading => format!(" {} {machine} · {filter} ", theme::spinner(view.tick)),
            _ => format!(" {machine} · {filter} · {} ", self.executions.len()),
        };
        let focused = self.focus == Focus::List;
        if self.executions.is_empty() {
            let text = match (&self.exec_load, self.machine.as_ref().is_some_and(StateMachine::express)) {
                (_, true) => Line::styled("Express: no execution list · l opens the logs", theme::muted()),
                (LoadState::Failed(e), _) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                (LoadState::Loading, _) => Line::styled("Loading executions…", theme::muted()),
                _ => Line::styled(format!("No {} executions · f changes the filter", if filter == "all" { "" } else { filter }), theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, focused)), area);
            return;
        }
        let width = area.width.saturating_sub(4) as usize;
        let now = view.now;
        let items: Vec<ListItem> = self
            .executions
            .iter()
            .map(|e| {
                let (symbol, style, _) = status_style(&e.status);
                let symbol = if e.status == "RUNNING" { theme::spinner(view.tick) } else { symbol };
                let duration = e.start_date.map(|s| fmt_duration(e.stop_date.unwrap_or(now) - s)).unwrap_or_default();
                let right = format!("{} · {duration}", e.start_date.map(fmt_ago).unwrap_or_default());
                let name = ellipsize(&e.name, width.saturating_sub(right.chars().count() + 4));
                let pad = width.saturating_sub(name.chars().count() + right.chars().count() + 2);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{symbol} "), style),
                    Span::raw(name),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(right, theme::muted()),
                ]))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, focused)).highlight_style(theme::selected());
        f.render_stateful_widget(list, area, &mut self.exec_state);
    }

    fn render_execution(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(e) = self.selected_execution() else {
            let text = if self.machine.as_ref().is_some_and(StateMachine::express) {
                match self.machine.as_ref().and_then(|m| self.machine_detail(&m.state_machine_arn)?.log_group()) {
                    Some(g) => format!("Express executions are only in the logs: l opens {g}"),
                    None => "Express executions are only in the logs, and this machine does not log to CloudWatch".into(),
                }
            } else {
                "Select an execution".into()
            };
            f.render_widget(
                widgets::placeholder(Line::styled(text, theme::muted())).block(widgets::panel(" Execution ", false)),
                area,
            );
            return;
        };
        let (symbol, style, label) = status_style(&e.status);
        let block = widgets::panel(Line::from(format!(" {} ", e.name)).bold(), self.focus == Focus::Steps)
            .title_top(Line::styled(format!(" {symbol} {label} "), style.bold()).right_aligned());
        let d = match self.details.get(&e.execution_arn) {
            Some(Ok(d)) => d,
            Some(Err(err)) => {
                f.render_widget(widgets::placeholder(Line::styled(err.short(), Style::new().fg(theme::ERR))).block(block), area);
                return;
            }
            None => {
                let loading = Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" loading…")]);
                f.render_widget(widgets::placeholder(loading).block(block), area);
                return;
            }
        };
        let kw = 10;
        let mut lines = Vec::new();
        if let Some(t) = d.detail.start_date {
            lines.push(widgets::kv("Started", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
        }
        if let Some(dur) = d.detail.duration(view.now) {
            let text = if d.detail.status == "RUNNING" { format!("running for {}", fmt_duration(dur)) } else { fmt_duration(dur) };
            lines.push(widgets::kv("Duration", text, kw));
        }
        if let Some(n) = d.detail.redrive_count.filter(|n| *n > 0) {
            lines.push(widgets::kv("Redriven", format!("{n}×"), kw));
        }
        if let Some(failure) = sfn::failure(&d.steps, &d.events, Some(&d.detail)) {
            let err = Style::new().fg(theme::ERR);
            if let Some(state) = &failure.state {
                lines.push(Line::from(vec![Span::styled(format!("{:<kw$} ", "Failed at"), theme::muted()), Span::styled(state.clone(), err.bold())]));
            }
            if let Some(error) = &failure.error {
                lines.push(Line::from(vec![Span::styled(format!("{:<kw$} ", "Error"), theme::muted()), Span::styled(error.clone(), err)]));
            }
            if let Some(cause) = &failure.cause {
                let pretty = sfn::pretty_cause(cause);
                let total = pretty.lines().count();
                for (i, l) in pretty.lines().take(8).enumerate() {
                    let key = if i == 0 { "Cause" } else { "" };
                    lines.push(Line::from(vec![Span::styled(format!("{key:<kw$} "), theme::muted()), Span::styled(l.to_string(), err)]));
                }
                if total > 8 {
                    let more = if total - 8 == 1 { "1 more line".to_string() } else { format!("{} more lines", total - 8) };
                    lines.push(Line::styled(format!("{:<kw$} … {more} · c copies the full report", ""), theme::muted()));
                }
            }
        }
        lines.push(Line::raw(""));
        let hint = if self.focus == Focus::Steps { "   ↑↓ step · i/O its input/output · l its logs · Esc back" } else { "   → / Enter to browse" };
        lines.push(Line::from(vec![widgets::section(&format!("Steps · {}", d.steps.len())).spans.remove(0), Span::styled(hint, theme::muted())]));
        let name_w = d.steps.iter().map(|s| s.name.chars().count()).max().unwrap_or(4).clamp(4, 28);
        for (i, s) in d.steps.iter().enumerate() {
            let selected = self.focus == Focus::Steps && i == self.step_cursor;
            let (symbol, style) = match s.status {
                StepStatus::Running => (theme::spinner(view.tick), Style::new().fg(theme::RUNNING)),
                StepStatus::Succeeded => ("✓", Style::new().fg(theme::OK)),
                StepStatus::Failed => ("✗", Style::new().fg(theme::ERR)),
                StepStatus::Caught => ("↷", Style::new().fg(theme::WARN)),
            };
            let duration = s.duration(view.now).map(fmt_duration).unwrap_or_default();
            let what = s.lambda.as_deref().map(|l| format!("λ {l}")).or_else(|| s.resource.clone()).unwrap_or_default();
            let mut spans = vec![
                Span::styled(if selected { " ▶ " } else { "   " }, theme::accent()),
                Span::styled(format!("{symbol} "), style),
                Span::styled(format!("{:<name_w$} ", ellipsize(&s.name, name_w)), if selected { theme::bold() } else { Style::new() }),
                Span::styled(format!("{:<9}", s.kind), theme::muted()),
                Span::raw(format!("{duration:>8}  ")),
                Span::styled(what, theme::muted()),
            ];
            if s.retries > 0 {
                spans.push(Span::styled(format!("  ↻{}", s.retries), Style::new().fg(theme::WARN)));
            }
            if s.status == StepStatus::Caught {
                spans.push(Span::styled("  caught", Style::new().fg(theme::WARN)));
            }
            let mut line = Line::from(spans);
            if selected {
                line = line.patch_style(theme::selected());
            }
            lines.push(line);
            if matches!(s.status, StepStatus::Failed | StepStatus::Caught)
                && let Some(err) = &s.error
            {
                lines.push(Line::styled(format!("       {err}"), Style::new().fg(theme::ERR)));
            }
        }
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
    }

    fn render_dialog(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(dialog) = &self.dialog else { return };
        let color = if is_production(&self.env_name) { theme::ERR } else { theme::ACCENT };
        match dialog {
            Dialog::Form { form, busy, .. } => {
                form.render(f, area, color);
                if *busy {
                    busy_popup(f, area, view);
                }
            }
            Dialog::Confirm { title, lines, typed, busy, error, .. } => {
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
                text.push(widgets::hints_line(&[("Enter", "confirm"), ("Esc", "cancel")]));
                let rect = widgets::popup_area(area, 76, widgets::wrapped_height(&text, 72) + 2);
                widgets::popup(f, rect, title, color, Text::from(text));
                if let Some((row, col)) = cursor {
                    f.set_cursor_position((rect.x + 2 + col, rect.y + 1 + row));
                }
                if *busy {
                    busy_popup(f, area, view);
                }
            }
        }
    }
}

fn busy_popup(f: &mut Frame, area: Rect, view: &View) {
    let body = Text::from(vec![
        Line::raw(""),
        Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw("Working…")]),
    ]);
    widgets::popup(f, widgets::popup_area(area, 30, 5), "Step Functions", theme::ACCENT, body);
}

impl Module for StepFunctions {
    fn title(&self) -> &'static str {
        "Step Functions"
    }

    fn description(&self) -> &'static str {
        "Step Functions · executions, failed steps, logs, redrive, start, stop"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = StepFunctions::new();
        self.env_name = ctx.env().name.clone();
        self.region = ctx.env().region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load_machines(ctx);
        }
    }

    /// Shows the executions of the state machine `target` (an ARN or a name).
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        if !matches!(self.load, LoadState::Loaded) {
            self.pending_open = Some(target.to_string());
            return;
        }
        self.filter.clear();
        self.refilter();
        let found = self.visible.iter().position(|&i| {
            let m = &self.machines[i];
            m.state_machine_arn == target || m.name == target
        });
        match found {
            Some(pos) => {
                self.zoom = None;
                self.dialog = None;
                self.select_machine(pos, ctx);
                self.enter_machine(ctx);
            }
            None => ctx.toast(Level::Warn, format!("State machine not found here: {target}")),
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some((_, view)) = &mut self.zoom {
            if let LogKey::Copy = view.key(key) {
                let ok = crate::platform::copy_to_clipboard(&view.lines().join("\n"));
                ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Copied" } else { "Clipboard not available" });
            }
            if !view.zoomed {
                self.zoom = None;
            }
            return Handled::Yes;
        }
        if self.dialog.is_some() {
            self.dialog_key(key, ctx);
            return Handled::Yes;
        }
        if self.filtering {
            let selected = self.machine_state.selected().unwrap_or(0);
            match self.filter.handle(key) {
                InputEvent::Changed => self.refilter(),
                InputEvent::Submit => self.filtering = false,
                InputEvent::Cancel => {
                    self.filtering = false;
                    self.filter.clear();
                    self.refilter();
                }
                InputEvent::Ignored => match key.code {
                    KeyCode::Up => self.select_machine(selected.saturating_sub(1), ctx),
                    KeyCode::Down => self.select_machine(selected + 1, ctx),
                    _ => {}
                },
                InputEvent::Moved => {}
            }
            return Handled::Yes;
        }
        match self.screen {
            Screen::Machines => {
                let selected = self.machine_state.selected().unwrap_or(0);
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => self.select_machine(selected.saturating_sub(1), ctx),
                    KeyCode::Down | KeyCode::Char('j') => self.select_machine(selected + 1, ctx),
                    KeyCode::Enter | KeyCode::Right => self.enter_machine(ctx),
                    KeyCode::Char('/') => self.filtering = true,
                    KeyCode::Esc if !self.filter.value.is_empty() => {
                        self.filter.clear();
                        self.refilter();
                    }
                    KeyCode::Char('l') => self.open_logs(ctx),
                    KeyCode::Char('n') => self.open_start(),
                    KeyCode::Char('o') => {
                        if let Some(m) = self.selected_machine() {
                            open_url(ctx, &sfn::console_url(&self.region, &m.state_machine_arn));
                        }
                    }
                    KeyCode::Char('r') => self.load_machines(ctx),
                    _ => return Handled::No,
                }
            }
            Screen::Executions if self.focus == Focus::Steps => {
                let steps = self.detail().map_or(0, |d| d.steps.len());
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => self.step_cursor = self.step_cursor.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => self.step_cursor = (self.step_cursor + 1).min(steps.saturating_sub(1)),
                    KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => self.focus = Focus::List,
                    KeyCode::Char('i') | KeyCode::Char('O') => {
                        let step = self.detail().and_then(|d| d.steps.get(self.step_cursor)).cloned();
                        if let Some(s) = step {
                            let (what, json) = if key.code == KeyCode::Char('i') { ("input", s.input) } else { ("output", s.output) };
                            self.zoom_json(format!("{} · {what}", s.name), json.as_deref(), ctx);
                        }
                    }
                    KeyCode::Char('l') => self.open_logs(ctx),
                    KeyCode::Char('c') => self.copy_report(ctx),
                    KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
                    KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
                    _ => return Handled::No,
                }
            }
            Screen::Executions => {
                let selected = self.exec_state.selected().unwrap_or(0);
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => self.select_execution(selected.saturating_sub(1)),
                    KeyCode::Down | KeyCode::Char('j') => self.select_execution(selected + 1),
                    KeyCode::Enter | KeyCode::Right => {
                        if self.detail().is_some_and(|d| !d.steps.is_empty()) {
                            self.focus = Focus::Steps;
                        }
                    }
                    KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => {
                        self.screen = Screen::Machines;
                        self.focus = Focus::List;
                    }
                    KeyCode::Char('f') => {
                        self.status_filter = (self.status_filter + 1) % FILTERS.len();
                        self.executions.clear();
                        self.exec_state.select(None);
                        self.load_executions(ctx);
                    }
                    KeyCode::Char('i') | KeyCode::Char('O') => {
                        let d = self.detail().map(|d| (d.detail.name.clone(), d.detail.input.clone(), d.detail.output.clone()));
                        if let Some((name, input, output)) = d {
                            let (what, json) = if key.code == KeyCode::Char('i') { ("input", input) } else { ("output", output) };
                            self.zoom_json(format!("{name} · {what}"), json.as_deref(), ctx);
                        }
                    }
                    KeyCode::Char('l') => self.open_logs(ctx),
                    KeyCode::Char('c') => self.copy_report(ctx),
                    KeyCode::Char('R') => self.open_redrive(ctx),
                    KeyCode::Char('n') => self.open_start(),
                    KeyCode::Char('x') => self.open_stop(ctx),
                    KeyCode::Char('r') => {
                        if let Some(arn) = self.selected_execution().map(|e| e.execution_arn.clone()) {
                            self.details.remove(&arn);
                            self.reselect = Some(arn);
                        }
                        self.load_executions(ctx);
                    }
                    KeyCode::Char('o') => {
                        if let Some(e) = self.selected_execution() {
                            open_url(ctx, &sfn::console_url(&self.region, &e.execution_arn));
                        }
                    }
                    KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
                    KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
                    _ => return Handled::No,
                }
            }
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() || self.zoom.is_some() || !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        // Rows start below the panel border (and the filter bar for machines).
        let top = self.list_area.y + if self.screen == Screen::Machines { 2 } else { 1 };
        let row = event.row.saturating_sub(top) as usize;
        match (&self.screen, event.kind) {
            (Screen::Machines, MouseEventKind::ScrollUp) => {
                let s = self.machine_state.selected().unwrap_or(0);
                self.select_machine(s.saturating_sub(1), ctx);
            }
            (Screen::Machines, MouseEventKind::ScrollDown) => {
                let s = self.machine_state.selected().unwrap_or(0);
                self.select_machine(s + 1, ctx);
            }
            (Screen::Machines, MouseEventKind::Down(MouseButton::Left)) => {
                let index = self.machine_state.offset() + row;
                if self.machine_state.selected() == Some(index) {
                    self.enter_machine(ctx);
                } else {
                    self.select_machine(index, ctx);
                }
            }
            (Screen::Executions, MouseEventKind::ScrollUp) => {
                let s = self.exec_state.selected().unwrap_or(0);
                self.select_execution(s.saturating_sub(1));
            }
            (Screen::Executions, MouseEventKind::ScrollDown) => {
                let s = self.exec_state.selected().unwrap_or(0);
                self.select_execution(s + 1);
            }
            (Screen::Executions, MouseEventKind::Down(MouseButton::Left)) => {
                self.focus = Focus::List;
                self.select_execution(self.exec_state.offset() + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Machines(result) => match result {
                Ok(machines) => {
                    self.machines = machines;
                    self.load = LoadState::Loaded;
                    self.refilter();
                    self.select_machine(self.machine_state.selected().unwrap_or(0), ctx);
                    if let Some(target) = self.pending_open.take() {
                        self.open(&target, ctx);
                    }
                }
                Err(e) => self.load = LoadState::Failed(e),
            },
            Msg::Machine { arn, result } => {
                self.machine_in_flight.remove(&arn);
                self.machine_details.insert(arn, result);
            }
            Msg::Executions { seq, result } => {
                if seq != self.exec_seq {
                    return;
                }
                match result {
                    Ok(list) => {
                        let keep = self.reselect.take().or_else(|| self.selected_execution().map(|e| e.execution_arn.clone()));
                        // Running ones are refreshed below; finished ones keep their cached detail.
                        for e in list.iter().filter(|e| e.status == "RUNNING") {
                            self.details.remove(&e.execution_arn);
                        }
                        self.executions = list;
                        self.exec_load = LoadState::Loaded;
                        let index = keep.and_then(|k| self.executions.iter().position(|e| e.execution_arn == k)).unwrap_or(0);
                        self.exec_state.select(None);
                        self.select_execution(index);
                    }
                    Err(e) => self.exec_load = LoadState::Failed(e),
                }
            }
            Msg::Detail { arn, result } => {
                self.detail_in_flight.remove(&arn);
                let result = result.map(|(detail, events)| Detail { steps: sfn::steps(&events), detail, events, loaded: Instant::now() });
                self.details.insert(arn, result);
            }
            Msg::Acted { result } => self.on_acted(result, ctx),
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if let Some((arn, at)) = &self.pending_detail
            && at.elapsed() >= DEBOUNCE
        {
            let arn = arn.clone();
            self.pending_detail = None;
            if self.selected_execution().is_some_and(|e| e.execution_arn == arn) && !self.details.contains_key(&arn) {
                self.fetch_detail(arn, ctx);
            }
        }
        if !active || self.screen != Screen::Executions {
            return;
        }
        // A running execution's detail keeps up; so does a list with running executions.
        let running = self.detail().filter(|d| d.detail.status == "RUNNING" && d.loaded.elapsed() >= DETAIL_REFRESH);
        if let Some(arn) = running.map(|d| d.detail.execution_arn.clone()) {
            self.fetch_detail(arn, ctx);
        }
        let any_running = self.executions.iter().any(|e| e.status == "RUNNING");
        if any_running && !matches!(self.exec_load, LoadState::Loading) && self.listed_at.elapsed() >= LIST_REFRESH {
            self.load_executions(ctx);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some((title, zoomed)) = &mut self.zoom {
            let title = title.clone();
            zoomed.render_zoomed(f, area, &title);
            return;
        }
        let [left, right] = if area.width < 100 {
            area.layout(&Layout::vertical([Constraint::Percentage(40), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(36), Constraint::Fill(1)]))
        };
        match self.screen {
            Screen::Machines => {
                self.render_machines(f, left, view);
                self.render_machine_detail(f, right, view);
            }
            Screen::Executions => {
                self.render_executions(f, left, view);
                self.render_execution(f, right, view);
            }
        }
        self.render_dialog(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.zoom.is_some() {
            return vec![("/", "search"), ("y", "copy"), ("Esc", "back")];
        }
        if let Some(dialog) = &self.dialog {
            return match dialog {
                Dialog::Form { .. } => vec![("Tab", "next field"), ("Enter", "confirm"), ("Esc", "cancel")],
                Dialog::Confirm { .. } => vec![("Enter", "confirm"), ("Esc", "cancel")],
            };
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        match (&self.screen, &self.focus) {
            (Screen::Machines, _) => {
                vec![("↑↓", "move"), ("Enter", "executions"), ("/", "search"), ("n", "start"), ("l", "logs"), ("o", "console"), ("r", "reload")]
            }
            (Screen::Executions, Focus::Steps) => {
                vec![("↑↓", "step"), ("i/O", "step input/output"), ("l", "its logs"), ("c", "copy report"), ("Esc", "back")]
            }
            (Screen::Executions, Focus::List) => vec![
                ("↑↓", "move"),
                ("→", "steps"),
                ("f", "filter"),
                ("i/O", "input/output"),
                ("l", "logs"),
                ("c", "copy for AI"),
                ("R", "redrive"),
                ("n", "start"),
                ("x", "stop"),
                ("Esc", "machines"),
            ],
        }
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || matches!(self.exec_load, LoadState::Loading)
            || self.pending_detail.is_some()
            || !self.detail_in_flight.is_empty()
            || self.executions.iter().any(|e| e.status == "RUNNING")
            || matches!(self.dialog, Some(Dialog::Form { busy: true, .. }) | Some(Dialog::Confirm { busy: true, .. }))
    }

    fn capturing(&self) -> bool {
        self.filtering || self.dialog.is_some() || self.zoom.is_some()
    }

    fn wants_mouse(&self) -> bool {
        self.zoom.is_none()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.dialog {
            Some(Dialog::Form { form, .. }) => form.paste(text),
            Some(Dialog::Confirm { typed: Some((_, input)), .. }) => input.insert_str(text, false),
            _ if self.filtering => {
                self.filter.insert_str(text, false);
                self.refilter();
            }
            _ => {}
        }
    }
}

fn open_url(ctx: &mut Ctx, url: &str) {
    if !crate::platform::open_url(url) {
        ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
    }
}

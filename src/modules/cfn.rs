//! CloudFormation: stacks (nested ones under their parent) with their status and drift; a
//! stack's overview, events (followed live while an operation runs), resources (Enter opens
//! one in its dcli module), outputs, drift (detected on demand) and change sets; its
//! template full screen; and Markdown for an AI agent: why the latest operation failed
//! (down the nested stacks), what drifted, what a change set would do.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::cloudformation::{self as cfn, Change, ChangeSet, DriftDetection, Health, Resource, ResourceDrift, Stack, StackEvent, Trail};
use crate::aws;
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, fmt_utc};

const DEBOUNCE: Duration = Duration::from_millis(200);
/// How often the selected stack refreshes (with its events) while an operation runs.
const LIVE_REFRESH: Duration = Duration::from_secs(5);
/// How often the list refreshes while some stack has an operation running.
const LIST_REFRESH: Duration = Duration::from_secs(15);
const DRIFT_POLL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Overview,
    Events,
    Resources,
    Outputs,
    Drift,
    Changes,
}

const TABS: [Tab; 6] = [Tab::Overview, Tab::Events, Tab::Resources, Tab::Outputs, Tab::Drift, Tab::Changes];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Events => "Events",
            Tab::Resources => "Resources",
            Tab::Outputs => "Outputs",
            Tab::Drift => "Drift",
            Tab::Changes => "Change sets",
        }
    }
}

enum Msg {
    Stacks(aws::Result<Vec<Stack>>),
    /// One stack read again (an operation running, a drift detection finished).
    Stack { name: String, result: aws::Result<Vec<Stack>> },
    Events { stack: String, result: aws::Result<Vec<StackEvent>> },
    Trail { stack: String, result: aws::Result<Option<Trail>> },
    Resources { stack: String, result: aws::Result<Vec<Resource>> },
    Template { stack: String, result: aws::Result<String> },
    DriftStarted { stack: String, result: aws::Result<String> },
    DriftStatus { stack: String, result: aws::Result<DriftDetection> },
    Drifts { stack: String, result: aws::Result<Vec<ResourceDrift>> },
    ChangeSets { stack: String, result: aws::Result<Vec<ChangeSet>> },
    Changes { stack: String, set: String, result: aws::Result<Vec<Change>> },
    /// A report for an AI agent: what it is about, and the Markdown (`None`: nothing to say).
    Report { what: String, result: aws::Result<Option<String>> },
}

/// What is known about a stack beyond its description, read as the views need it.
#[derive(Default)]
struct Cache {
    events: Option<aws::Result<Vec<StackEvent>>>,
    events_at: Option<Instant>,
    trail: Option<aws::Result<Option<Trail>>>,
    resources: Option<aws::Result<Vec<Resource>>>,
    template: Option<aws::Result<String>>,
    drifts: Option<aws::Result<Vec<ResourceDrift>>>,
    /// A drift detection running: its ID and when it was last asked about.
    detecting: Option<(String, Instant)>,
    change_sets: Option<aws::Result<Vec<ChangeSet>>>,
    /// Changes of the change sets opened (by ID).
    changes: HashMap<String, aws::Result<Vec<Change>>>,
    /// Change sets shown open.
    expanded: HashSet<String>,
}

pub struct CloudFormation {
    stacks: Vec<Stack>,
    load: LoadState,
    listed_at: Instant,
    filter: TextInput,
    filtering: bool,
    /// Rows of the list: index in `stacks` and nesting depth, in tree order.
    rows: Vec<(usize, usize)>,
    list_state: ListState,
    tab: Tab,
    /// The keys move through the rows of the tab instead of the stacks.
    focused: bool,
    cursor: usize,
    scroll: u16,
    cache: HashMap<String, Cache>,
    in_flight: HashSet<(String, &'static str)>,
    pending: Option<(String, Instant)>,
    /// Stack waiting for the user to confirm a drift detection.
    confirm_drift: Option<String>,
    /// The template, full screen.
    zoom: Option<(String, LogView)>,
    /// A stack another module asked to show, selected once the list loads.
    pending_open: Option<String>,
    env_name: String,
    region: String,
    list_area: Rect,
}

/// Symbol and style of a stack or resource status.
fn look(status: &str, tick: usize) -> (&'static str, Style) {
    match cfn::health(status) {
        Health::InProgress => (theme::spinner(tick), Style::new().fg(theme::RUNNING)),
        Health::RollingBack => (theme::spinner(tick), Style::new().fg(theme::WARN)),
        Health::Ok if status.starts_with("DELETE") => ("·", theme::muted()),
        Health::Ok => ("✓", Style::new().fg(theme::OK)),
        Health::RolledBack => ("↶", Style::new().fg(theme::WARN)),
        Health::Failed => ("✗", Style::new().fg(theme::ERR)),
    }
}

/// The dcli module (and what to open there) for a resource, when there is one.
fn jump_target(r: &Resource) -> Option<(&'static str, String)> {
    let p = r.physical_id.clone()?;
    Some(match r.resource_type.as_str() {
        "AWS::Lambda::Function" => ("Lambda", p),
        "AWS::SQS::Queue" => ("SQS", p),
        "AWS::DynamoDB::Table" => ("DynamoDB", p),
        "AWS::StepFunctions::StateMachine" => ("Step Functions", p),
        "AWS::Logs::LogGroup" => ("Logs", p),
        "AWS::S3::Bucket" => ("S3", format!("s3://{p}/")),
        "AWS::SSM::Parameter" => ("Parameters", p),
        _ => return None,
    })
}

fn copy(ctx: &mut Ctx, label: &str, text: &str, what: &str) {
    if crate::platform::copy_to_clipboard(text) {
        return ctx.toast(Level::Success, format!("Copied {what}"));
    }
    match crate::platform::write_temp_file(label, text) {
        Ok(path) => ctx.toast(Level::Success, format!("No clipboard: saved {what} to {}", path.display())),
        Err(e) => ctx.toast(Level::Error, format!("Could not save {what}: {e}")),
    }
}

impl CloudFormation {
    pub fn new() -> CloudFormation {
        CloudFormation {
            stacks: Vec::new(),
            load: LoadState::Idle,
            listed_at: Instant::now(),
            filter: TextInput::default(),
            filtering: false,
            rows: Vec::new(),
            list_state: ListState::default(),
            tab: Tab::Overview,
            focused: false,
            cursor: 0,
            scroll: 0,
            cache: HashMap::new(),
            in_flight: HashSet::new(),
            pending: None,
            confirm_drift: None,
            zoom: None,
            pending_open: None,
            env_name: String::new(),
            region: String::new(),
            list_area: Rect::default(),
        }
    }

    // ---- the list ----------------------------------------------------------------------

    fn load_stacks(&mut self, ctx: &mut Ctx) {
        self.load = LoadState::Loading;
        self.listed_at = Instant::now();
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Stacks(cfn::describe_stacks(&aws, None)));
    }

    fn refilter(&mut self) {
        let keep = self.selected().map(|s| s.id.clone());
        let query = self.filter.value.trim().to_lowercase();
        self.rows = cfn::tree(&self.stacks).into_iter().filter(|(i, _)| matches_filter(&self.stacks[*i].name, &query)).collect();
        let index = keep.and_then(|k| self.rows.iter().position(|(i, _)| self.stacks[*i].id == k)).unwrap_or(0);
        self.list_state.select(if self.rows.is_empty() { None } else { Some(index) });
    }

    fn selected(&self) -> Option<&Stack> {
        self.stacks.get(self.rows.get(self.list_state.selected()?)?.0)
    }

    fn select(&mut self, index: usize) {
        if self.rows.is_empty() {
            return;
        }
        let index = index.min(self.rows.len() - 1);
        if self.list_state.selected() != Some(index) {
            self.cursor = 0;
            self.scroll = 0;
            self.focused = false;
        }
        self.list_state.select(Some(index));
        if let Some(s) = self.selected() {
            self.pending = Some((s.name.clone(), Instant::now()));
        }
    }

    /// Selects the stack named `target` (a name or ID), clearing a filter that hides it.
    fn try_open(&mut self) -> bool {
        let Some(target) = self.pending_open.clone() else { return false };
        let Some(i) = self.stacks.iter().position(|s| s.name == target || s.id == target) else { return false };
        if !self.rows.iter().any(|(r, _)| *r == i) {
            self.filter.clear();
            self.filtering = false;
            self.refilter();
        }
        if let Some(pos) = self.rows.iter().position(|(r, _)| *r == i) {
            self.select(pos);
        }
        self.pending_open = None;
        true
    }

    fn cache(&mut self, stack: &str) -> &mut Cache {
        self.cache.entry(stack.to_string()).or_default()
    }

    fn cached(&self, stack: &str) -> Option<&Cache> {
        self.cache.get(stack)
    }

    /// Starts `job` for `stack` unless the same kind of read is already running.
    fn fetch<F>(&mut self, stack: &Stack, what: &'static str, ctx: &mut Ctx, job: F)
    where
        F: FnOnce(&aws::Aws, String, String) -> Msg + Send + 'static,
    {
        if !self.in_flight.insert((stack.name.clone(), what)) {
            return;
        }
        let (aws, name, id) = (ctx.env().aws.clone(), stack.name.clone(), stack.id_or_name());
        ctx.spawn(move || job(&aws, name, id));
    }

    fn load_events(&mut self, stack: &Stack, ctx: &mut Ctx) {
        self.fetch(stack, "events", ctx, |aws, stack, id| Msg::Events { result: cfn::stack_events(aws, &id, 0.0, 3), stack });
        if stack.failed() && self.cached(&stack.name).is_none_or(|c| c.trail.is_none()) {
            let s = stack.clone();
            self.fetch(stack, "trail", ctx, move |aws, stack, _| Msg::Trail { result: cfn::trail(aws, &s), stack });
        }
    }

    /// Reads what the current tab shows about the selected stack, if it is not known yet.
    fn load_tab(&mut self, ctx: &mut Ctx) {
        let Some(stack) = self.selected().cloned() else { return };
        let cache = self.cached(&stack.name);
        match self.tab {
            Tab::Overview | Tab::Events if cache.is_none_or(|c| c.events.is_none()) => self.load_events(&stack, ctx),
            Tab::Resources if cache.is_none_or(|c| c.resources.is_none()) => {
                self.fetch(&stack, "resources", ctx, |aws, stack, id| Msg::Resources { result: cfn::list_resources(aws, &id), stack });
            }
            // The results of the last detection, when there was one.
            Tab::Drift
                if cache.is_none_or(|c| c.drifts.is_none() && c.detecting.is_none())
                    && stack.drift.as_deref().is_some_and(|d| d != "NOT_CHECKED") =>
            {
                self.fetch(&stack, "drifts", ctx, |aws, stack, id| Msg::Drifts { result: cfn::resource_drifts(aws, &id), stack });
            }
            Tab::Changes if cache.is_none_or(|c| c.change_sets.is_none()) => {
                self.fetch(&stack, "change sets", ctx, |aws, stack, id| Msg::ChangeSets { result: cfn::list_change_sets(aws, &id), stack });
            }
            _ => {}
        }
    }

    fn set_tab(&mut self, tab: Tab, ctx: &mut Ctx) {
        if self.tab != tab {
            self.tab = tab;
            self.cursor = 0;
            self.scroll = 0;
        }
        self.load_tab(ctx);
    }

    fn step_tab(&mut self, forward: bool, ctx: &mut Ctx) {
        let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
        let next = if forward { (i + 1) % TABS.len() } else { (i + TABS.len() - 1) % TABS.len() };
        self.set_tab(TABS[next], ctx);
    }

    // ---- rows of the tabs --------------------------------------------------------------

    /// How many rows the current tab has to move through.
    fn row_count(&self) -> usize {
        let Some(s) = self.selected() else { return 0 };
        let cache = self.cached(&s.name);
        match self.tab {
            Tab::Overview => 0,
            Tab::Events => cache.and_then(|c| c.events.as_ref()?.as_ref().ok()).map_or(0, Vec::len),
            Tab::Resources => cache.and_then(|c| c.resources.as_ref()?.as_ref().ok()).map_or(0, Vec::len),
            Tab::Outputs => s.outputs.len(),
            Tab::Drift => cache.and_then(|c| c.drifts.as_ref()?.as_ref().ok()).map_or(0, Vec::len),
            Tab::Changes => cache.and_then(|c| c.change_sets.as_ref()?.as_ref().ok()).map_or(0, Vec::len),
        }
    }

    fn selected_resource(&self) -> Option<&Resource> {
        let s = self.selected()?;
        self.cached(&s.name)?.resources.as_ref()?.as_ref().ok()?.get(self.cursor)
    }

    fn selected_change_set(&self) -> Option<&ChangeSet> {
        let s = self.selected()?;
        self.cached(&s.name)?.change_sets.as_ref()?.as_ref().ok()?.get(self.cursor)
    }

    fn selected_event(&self) -> Option<&StackEvent> {
        let s = self.selected()?;
        self.cached(&s.name)?.events.as_ref()?.as_ref().ok()?.get(self.cursor)
    }

    /// Enter on a row: open the resource (or nested stack), open a change set.
    fn activate_row(&mut self, ctx: &mut Ctx) {
        match self.tab {
            Tab::Resources => {
                let Some(r) = self.selected_resource().cloned() else { return };
                if r.resource_type == "AWS::CloudFormation::Stack" {
                    return self.open_stack(r.physical_id.as_deref().unwrap_or_default(), ctx);
                }
                match jump_target(&r) {
                    Some((module, target)) => ctx.jump(module, target),
                    None => ctx.toast(Level::Info, format!("dcli has no module for {}; o opens the stack in the console", r.resource_type)),
                }
            }
            Tab::Events => {
                let Some(e) = self.selected_event().cloned() else { return };
                if e.resource_type == "AWS::CloudFormation::Stack" && !e.of_stack() {
                    self.open_stack(e.physical_id.as_deref().unwrap_or_default(), ctx);
                }
            }
            Tab::Changes => {
                let (Some(stack), Some(set)) = (self.selected().cloned(), self.selected_change_set().cloned()) else { return };
                let cache = self.cache(&stack.name);
                if !cache.expanded.remove(&set.id) {
                    cache.expanded.insert(set.id.clone());
                    if !cache.changes.contains_key(&set.id) {
                        let id = set.id.clone();
                        self.fetch(&stack, "changes", ctx, move |aws, stack, stack_id| Msg::Changes {
                            result: cfn::describe_change_set(aws, &stack_id, &id),
                            set: id,
                            stack,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// Selects a nested stack (by its ID) in the list.
    fn open_stack(&mut self, id: &str, ctx: &mut Ctx) {
        self.pending_open = Some(id.to_string());
        if self.try_open() {
            self.tab = Tab::Overview;
            self.load_tab(ctx);
        } else {
            self.pending_open = None;
            ctx.toast(Level::Info, format!("{} is not in the list (r reloads it)", cfn::stack_name_from_id(id)));
        }
    }

    // ---- actions -----------------------------------------------------------------------

    /// `y`: the value of the selected row, or the stack's name.
    fn copy_row(&self, ctx: &mut Ctx) {
        let Some(s) = self.selected() else { return };
        let (text, what) = match (self.tab, self.focused) {
            (Tab::Outputs, _) => match s.outputs.get(self.cursor) {
                Some(o) => (o.value.clone(), format!("output {}", o.key)),
                None => return,
            },
            (Tab::Resources, true) => match self.selected_resource().and_then(|r| r.physical_id.clone().map(|p| (p, r.logical_id.clone()))) {
                Some((p, l)) => (p, format!("the physical ID of {l}")),
                None => return,
            },
            (Tab::Events, true) => match self.selected_event().and_then(|e| e.reason.clone().map(|r| (r, e.logical_id.clone()))) {
                Some((r, l)) => (r, format!("the reason of {l}")),
                None => return,
            },
            _ => (s.name.clone(), format!("the stack name {}", s.name)),
        };
        if text.chars().any(crate::util::is_unsafe) {
            return ctx.toast(Level::Warn, "It contains control characters; not copied");
        }
        copy(ctx, &s.name, &text, &what);
    }

    /// `c`: Markdown for an AI agent — the drift on the Drift tab, the selected change set on
    /// the Change sets tab, else why the latest operation failed.
    fn copy_report(&mut self, ctx: &mut Ctx) {
        let Some(stack) = self.selected().cloned() else { return };
        let (env, region) = (self.env_name.clone(), self.region.clone());
        match self.tab {
            Tab::Drift => {
                let drifts = self.cached(&stack.name).and_then(|c| c.drifts.as_ref()?.as_ref().ok());
                match drifts {
                    Some(d) => copy(ctx, &format!("{}-drift", stack.name), &cfn::drift_report(&env, &region, &stack, d), "the drift report"),
                    None => ctx.toast(Level::Info, "No drift results yet: d detects drift"),
                }
            }
            Tab::Changes => {
                let Some(set) = self.selected_change_set().cloned() else {
                    return ctx.toast(Level::Info, "Select a change set (Enter on the tab, ↑↓)");
                };
                let what = format!("the change set {}", set.name);
                let s = stack.clone();
                self.fetch(&stack, "report", ctx, move |aws, _, id| {
                    let result = cfn::describe_change_set(aws, &id, &set.id)
                        .map(|changes| Some(cfn::change_set_report(&env, &region, &s, &set, &changes)));
                    Msg::Report { what, result }
                });
            }
            _ => {
                if !stack.failed() {
                    return ctx.toast(Level::Info, format!("{} is {}: nothing failed to explain", stack.name, stack.status));
                }
                let what = format!("why {} failed", stack.name);
                self.fetch(&stack.clone(), "report", ctx, move |aws, _, _| Msg::Report {
                    what,
                    result: cfn::failure(aws, &env, &region, &stack).map(|f| f.map(|f| cfn::failure_report(&f))),
                });
            }
        }
    }

    fn start_drift(&mut self, ctx: &mut Ctx) {
        let Some(name) = self.confirm_drift.take() else { return };
        let Some(stack) = self.stacks.iter().find(|s| s.name == name).cloned() else { return };
        self.cache(&name).drifts = None;
        self.tab = Tab::Drift;
        self.cursor = 0;
        self.fetch(&stack, "detect", ctx, |aws, stack, id| Msg::DriftStarted { result: cfn::detect_drift(aws, &id), stack });
    }

    fn show_template(&mut self, ctx: &mut Ctx) {
        let Some(stack) = self.selected().cloned() else { return };
        match self.cached(&stack.name).and_then(|c| c.template.as_ref()) {
            Some(Ok(t)) => {
                let mut view = LogView::default();
                view.extend(t.lines().map(str::to_string));
                view.zoomed = true;
                // From the top, not following the end like a log.
                view.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
                self.zoom = Some((format!("{} · template", stack.name), view));
            }
            Some(Err(e)) => ctx.toast(Level::Error, e.short()),
            None => {
                self.fetch(&stack, "template", ctx, |aws, stack, id| Msg::Template { result: cfn::template(aws, &id), stack });
                ctx.toast(Level::Info, "Reading the template…");
            }
        }
    }

    // ---- rendering ---------------------------------------------------------------------

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search stacks");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = match &self.load {
            LoadState::Loading if self.stacks.is_empty() => format!(" {} Stacks ", theme::spinner(view.tick)),
            _ => format!(" Stacks · {} ", self.rows.len()),
        };
        if self.rows.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading stacks…", theme::muted()),
                _ => Line::styled("No stacks", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, !self.focused)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|&(i, depth)| {
                let s = &self.stacks[i];
                let (symbol, style) = look(&s.status, view.tick);
                let drifted = if s.drift.as_deref() == Some("DRIFTED") { " ≠" } else { "" };
                let when = s.updated.or(s.created).map(fmt_ago).unwrap_or_default();
                let indent = "  ".repeat(depth);
                let name_w = width.saturating_sub(indent.len() + 3 + when.chars().count() + drifted.chars().count() + 1);
                let name = ellipsize(&s.name, name_w);
                let pad = width.saturating_sub(indent.len() + 2 + name.chars().count() + drifted.chars().count() + when.chars().count());
                ListItem::new(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(format!("{symbol} "), style),
                    Span::raw(name),
                    Span::styled(drifted, Style::new().fg(theme::WARN)),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(when, theme::muted()),
                ]))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, !self.focused)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_detail(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let Some(stack) = self.selected().cloned() else {
            f.render_widget(widgets::placeholder(Line::styled("Select a stack", theme::muted())).block(widgets::panel(" Stack ", false)), area);
            return;
        };
        let (symbol, style) = look(&stack.status, view.tick);
        let block = widgets::panel(Line::from(format!(" {} ", stack.name)).bold(), self.focused)
            .title_top(Line::styled(format!(" {symbol} {} ", stack.status), style.bold()).right_aligned());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [tabs_area, body] = inner.layout(&Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(self.tabs_line(&stack)), tabs_area);

        let (lines, rows) = self.tab_lines(&stack, body.width as usize, view);
        // Keep the selected row in view.
        if self.focused
            && let Some(&at) = rows.get(self.cursor)
        {
            let at = at as u16;
            let height = body.height.max(1);
            if at < self.scroll {
                self.scroll = at;
            } else if at >= self.scroll + height {
                self.scroll = at + 1 - height;
            }
        }
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.scroll, 0)), body);
    }

    fn tabs_line(&self, stack: &Stack) -> Line<'static> {
        let cache = self.cached(&stack.name);
        let count = |n: Option<usize>| n.map(|n| format!(" {n}")).unwrap_or_default();
        let mut spans = Vec::new();
        for tab in TABS {
            let extra = match tab {
                Tab::Events => count(cache.and_then(|c| c.events.as_ref()?.as_ref().ok()).map(Vec::len)),
                Tab::Resources => count(cache.and_then(|c| c.resources.as_ref()?.as_ref().ok()).map(Vec::len)),
                Tab::Outputs => count(Some(stack.outputs.len())),
                Tab::Drift if stack.drift.as_deref() == Some("DRIFTED") => " ≠".into(),
                _ => String::new(),
            };
            let style = if tab == self.tab { Style::new().fg(ratatui::style::Color::Black).bg(theme::ACCENT).bold() } else { theme::muted() };
            spans.push(Span::styled(format!(" {}{extra} ", tab.label()), style));
            spans.push(Span::raw(" "));
        }
        Line::from(spans)
    }

    /// The lines of the current tab, and the line where each of its rows starts.
    fn tab_lines(&self, stack: &Stack, width: usize, view: &View) -> (Vec<Line<'static>>, Vec<usize>) {
        let cache = self.cached(&stack.name);
        let loading = |what: &str| Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(format!(" loading {what}…"))]);
        let failed = |e: &aws::AwsError| Line::styled(e.short(), Style::new().fg(theme::ERR));
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut rows = Vec::new();
        let selected = |i: usize| self.focused && i == self.cursor;
        let marker = |i: usize| Span::styled(if selected(i) { "▶ " } else { "  " }, theme::accent());
        let patch = |line: Line<'static>, i: usize| if selected(i) { line.patch_style(theme::selected()) } else { line };
        match self.tab {
            Tab::Overview => lines = self.overview(stack, view),
            Tab::Events => match cache.and_then(|c| c.events.as_ref()) {
                None => lines.push(loading("events")),
                Some(Err(e)) => lines.push(failed(e)),
                Some(Ok(events)) => {
                    let cause = cache.and_then(|c| c.trail.as_ref()?.as_ref().ok()?.as_ref()).map(|t| {
                        // In this stack, the cause is the event of the resource (or nested stack) that failed first.
                        let top = t.operation.iter().filter(|e| e.failed() && !e.of_stack());
                        let first = top.clone().find(|e| e.reason.as_deref().is_none_or(|r| !r.to_lowercase().contains("cancel"))).or(top.clone().next());
                        first.map(|e| (e.logical_id.clone(), e.status.clone(), e.timestamp))
                    });
                    let cause = cause.flatten();
                    let id_w = events.iter().map(|e| e.logical_id.chars().count()).max().unwrap_or(4).clamp(4, 30);
                    for (i, e) in events.iter().enumerate() {
                        rows.push(lines.len());
                        let (symbol, style) = look(&e.status, view.tick);
                        let symbol = if e.status.ends_with("_IN_PROGRESS") && !stack.status.ends_with("_IN_PROGRESS") { "·" } else { symbol };
                        let when = cfn::time(e.timestamp).get(5..).unwrap_or_default().to_string();
                        let is_cause = cause.as_ref().is_some_and(|(l, s, t)| *l == e.logical_id && *s == e.status && *t == e.timestamp);
                        let mut spans = vec![
                            marker(i),
                            Span::styled(format!("{when}  "), theme::muted()),
                            Span::styled(format!("{symbol} "), style),
                            Span::styled(format!("{:<id_w$} ", ellipsize(&e.logical_id, id_w)), if e.of_stack() { theme::bold() } else { Style::new() }),
                            Span::styled(e.status.clone(), style),
                        ];
                        if is_cause {
                            spans.push(Span::styled("  ◆ root cause", Style::new().fg(theme::ERR).bold()));
                        }
                        lines.push(patch(Line::from(spans), i));
                        if let Some(r) = &e.reason {
                            let reason_style = if e.failed() { Style::new().fg(theme::ERR) } else { theme::muted() };
                            let indent = 2 + when.chars().count() + 2 + 2;
                            lines.push(Line::styled(format!("{}{}", " ".repeat(indent), r), reason_style));
                        }
                    }
                    if events.is_empty() {
                        lines.push(Line::styled("No events", theme::muted()));
                    }
                }
            },
            Tab::Resources => match cache.and_then(|c| c.resources.as_ref()) {
                None => lines.push(loading("resources")),
                Some(Err(e)) => lines.push(failed(e)),
                Some(Ok(resources)) => {
                    let id_w = resources.iter().map(|r| r.logical_id.chars().count()).max().unwrap_or(4).clamp(4, 28);
                    let type_w = resources.iter().map(|r| r.resource_type.trim_start_matches("AWS::").chars().count()).max().unwrap_or(4).clamp(4, 26);
                    for (i, r) in resources.iter().enumerate() {
                        rows.push(lines.len());
                        let (symbol, style) = look(&r.status, view.tick);
                        let drift = match r.drift.as_deref() {
                            Some("MODIFIED") => Span::styled(" ≠ modified", Style::new().fg(theme::WARN)),
                            Some("DELETED") => Span::styled(" ≠ deleted", Style::new().fg(theme::ERR)),
                            _ => Span::raw(""),
                        };
                        let opens = jump_target(r).is_some() || r.resource_type == "AWS::CloudFormation::Stack";
                        let used = 2 + 2 + id_w + 1 + type_w + 1;
                        let physical = ellipsize(r.physical_id.as_deref().unwrap_or("—"), width.saturating_sub(used + 12).max(8));
                        lines.push(patch(
                            Line::from(vec![
                                marker(i),
                                Span::styled(format!("{symbol} "), style),
                                Span::styled(format!("{:<id_w$} ", ellipsize(&r.logical_id, id_w)), theme::bold()),
                                Span::styled(format!("{:<type_w$} ", ellipsize(r.resource_type.trim_start_matches("AWS::"), type_w)), theme::muted()),
                                Span::styled(physical, if opens { theme::accent() } else { Style::new() }),
                                drift,
                            ]),
                            i,
                        ));
                        if r.status.ends_with("_FAILED")
                            && let Some(reason) = &r.reason
                        {
                            lines.push(Line::styled(format!("    {reason}"), Style::new().fg(theme::ERR)));
                        }
                    }
                    if resources.is_empty() {
                        lines.push(Line::styled("No resources", theme::muted()));
                    }
                }
            },
            Tab::Outputs => {
                for (i, o) in stack.outputs.iter().enumerate() {
                    rows.push(lines.len());
                    lines.push(patch(Line::from(vec![marker(i), Span::styled(o.key.clone(), theme::bold())]), i));
                    lines.push(Line::raw(format!("    {}", o.value)));
                    let mut extra = Vec::new();
                    if let Some(d) = &o.description {
                        extra.push(d.clone());
                    }
                    if let Some(e) = &o.export {
                        extra.push(format!("exported as {e}"));
                    }
                    if !extra.is_empty() {
                        lines.push(Line::styled(format!("    {}", extra.join(" · ")), theme::muted()));
                    }
                }
                if stack.outputs.is_empty() {
                    lines.push(Line::styled("No outputs", theme::muted()));
                }
            }
            Tab::Drift => self.drift_lines(stack, view, &mut lines, &mut rows),
            Tab::Changes => match cache.and_then(|c| c.change_sets.as_ref()) {
                None => lines.push(loading("change sets")),
                Some(Err(e)) => lines.push(failed(e)),
                Some(Ok(sets)) => {
                    for (i, set) in sets.iter().enumerate() {
                        rows.push(lines.len());
                        let open = cache.is_some_and(|c| c.expanded.contains(&set.id));
                        let style = match set.execution_status.as_str() {
                            "AVAILABLE" => Style::new().fg(theme::ACCENT),
                            "EXECUTE_FAILED" => Style::new().fg(theme::ERR),
                            _ => theme::muted(),
                        };
                        lines.push(patch(
                            Line::from(vec![
                                marker(i),
                                Span::styled(if open { "▾ " } else { "▸ " }, theme::muted()),
                                Span::styled(set.name.clone(), theme::bold()),
                                Span::styled(format!("  {}", set.execution_status), style),
                                Span::styled(format!("  {}", set.created.map(fmt_ago).unwrap_or_default()), theme::muted()),
                            ]),
                            i,
                        ));
                        if let Some(d) = &set.description {
                            lines.push(Line::styled(format!("      {d}"), theme::muted()));
                        }
                        if let Some(r) = &set.reason {
                            lines.push(Line::styled(format!("      {r}"), Style::new().fg(theme::WARN)));
                        }
                        if open {
                            match cache.and_then(|c| c.changes.get(&set.id)) {
                                None => lines.push(loading("changes")),
                                Some(Err(e)) => lines.push(failed(e)),
                                Some(Ok(changes)) => {
                                    for c in changes {
                                        let (symbol, style) = match c.action.as_str() {
                                            "Add" => ("+", Style::new().fg(theme::OK)),
                                            "Remove" => ("-", Style::new().fg(theme::ERR)),
                                            _ => ("~", Style::new().fg(theme::WARN)),
                                        };
                                        let mut spans = vec![
                                            Span::styled(format!("      {symbol} "), style),
                                            Span::styled(c.logical_id.clone(), theme::bold()),
                                            Span::styled(format!("  {}", c.resource_type.trim_start_matches("AWS::")), theme::muted()),
                                        ];
                                        match c.replacement.as_deref() {
                                            Some("True") => spans.push(Span::styled("  replaces it", Style::new().fg(theme::ERR).bold())),
                                            Some("Conditional") => spans.push(Span::styled("  may replace it", Style::new().fg(theme::WARN))),
                                            _ => {}
                                        }
                                        lines.push(Line::from(spans));
                                        if !c.details.is_empty() {
                                            lines.push(Line::styled(format!("          {}", c.details.join(", ")), theme::muted()));
                                        }
                                    }
                                    if changes.is_empty() {
                                        lines.push(Line::styled("      no changes", theme::muted()));
                                    }
                                }
                            }
                        }
                    }
                    if sets.is_empty() {
                        lines.push(Line::styled("No change sets: create one (aws cloudformation create-change-set) to review an update here", theme::muted()));
                    }
                }
            },
        }
        (lines, rows)
    }

    fn overview(&self, s: &Stack, view: &View) -> Vec<Line<'static>> {
        let kw = 12;
        let mut lines = Vec::new();
        let (_, style) = look(&s.status, view.tick);
        lines.push(widgets::kv("Status", Span::styled(s.status.clone(), style.bold()), kw));
        if let Some(r) = &s.status_reason {
            lines.push(widgets::kv("Reason", r.clone(), kw));
        }
        if s.failed() {
            let trail = self.cached(&s.name).and_then(|c| c.trail.as_ref());
            match trail {
                Some(Ok(Some(t))) => {
                    let c = &t.cause;
                    let err = Style::new().fg(theme::ERR);
                    let mut spans = vec![
                        Span::styled(format!("{:<kw$} ", "Root cause"), theme::muted()),
                        Span::styled(c.logical_id.clone(), err.bold()),
                        Span::styled(format!(" {}", c.status), err),
                    ];
                    if t.chain.len() > 1 {
                        spans.push(Span::styled(format!(" in nested stack {}", c.stack_name), theme::muted()));
                    }
                    lines.push(Line::from(spans));
                    if let Some(r) = &c.reason {
                        lines.push(Line::styled(format!("{:<kw$} {r}", ""), err));
                    }
                    lines.push(Line::styled(format!("{:<kw$} c copies a report for an AI agent", ""), theme::muted()));
                }
                Some(Ok(None)) => {}
                Some(Err(e)) => lines.push(widgets::kv("Root cause", Span::styled(e.short(), Style::new().fg(theme::ERR)), kw)),
                None => lines.push(widgets::kv("Root cause", Span::styled(format!("{} looking…", theme::spinner(view.tick)), theme::muted()), kw)),
            }
        }
        if let Some(d) = &s.description {
            lines.push(widgets::kv("Description", d.clone(), kw));
        }
        if let Some(t) = s.created {
            lines.push(widgets::kv("Created", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
        }
        if let Some(t) = s.updated {
            lines.push(widgets::kv("Updated", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
        }
        let drift = s.drift.clone().unwrap_or_else(|| "NOT_CHECKED".into());
        let checked = s.drift_checked.map(|t| format!(" · checked {}", fmt_ago(t))).unwrap_or_default();
        let drift_style = if drift == "DRIFTED" { Style::new().fg(theme::WARN) } else { Style::new() };
        lines.push(Line::from(vec![
            Span::styled(format!("{:<kw$} ", "Drift"), theme::muted()),
            Span::styled(drift, drift_style),
            Span::styled(format!("{checked} · d detects it"), theme::muted()),
        ]));
        if let Some(p) = &s.parent_id {
            lines.push(widgets::kv("Nested in", cfn::stack_name_from_id(p).to_string(), kw));
        }
        if s.termination_protection {
            lines.push(widgets::kv("Protection", "termination protection on", kw));
        }
        if let Some(r) = &s.role_arn {
            lines.push(widgets::kv("Role", r.rsplit('/').next().unwrap_or(r).to_string(), kw));
        }
        if !s.parameters.is_empty() {
            lines.push(Line::raw(""));
            lines.push(widgets::section(&format!("Parameters · {}", s.parameters.len())));
            let w = s.parameters.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(4).min(30);
            for (k, v) in &s.parameters {
                lines.push(Line::from(vec![Span::styled(format!("  {:<w$} ", ellipsize(k, w)), theme::muted()), Span::raw(v.clone())]));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("←→ events, resources, outputs, drift, change sets · T template · o console", theme::muted()));
        lines
    }

    fn drift_lines(&self, stack: &Stack, view: &View, lines: &mut Vec<Line<'static>>, rows: &mut Vec<usize>) {
        let cache = self.cached(&stack.name);
        if cache.is_some_and(|c| c.detecting.is_some()) || self.in_flight.contains(&(stack.name.clone(), "detect")) {
            lines.push(Line::from(vec![
                Span::styled(theme::spinner(view.tick), theme::accent()),
                Span::raw(" Detecting drift: comparing each resource with the template…"),
            ]));
            return;
        }
        match cache.and_then(|c| c.drifts.as_ref()) {
            None if stack.drift.as_deref().is_none_or(|d| d == "NOT_CHECKED") => {
                lines.push(Line::raw("Drift has not been checked for this stack."));
                lines.push(Line::styled("d detects it: CloudFormation compares each resource with the template (a read).", theme::muted()));
            }
            None => lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" loading drift…")])),
            Some(Err(e)) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            Some(Ok(drifts)) => {
                let drifted = drifts.iter().filter(|d| !matches!(d.status.as_str(), "IN_SYNC" | "NOT_CHECKED")).count();
                let checked = stack.drift_checked.map(|t| format!(", checked {}", fmt_ago(t))).unwrap_or_default();
                let summary = format!("{drifted} of {} resources changed outside CloudFormation{checked} · c copies it for an AI agent · d checks again", drifts.len());
                lines.push(Line::styled(summary, if drifted > 0 { Style::new().fg(theme::WARN) } else { Style::new().fg(theme::OK) }));
                lines.push(Line::raw(""));
                for (i, d) in drifts.iter().enumerate() {
                    rows.push(lines.len());
                    let (symbol, style) = match d.status.as_str() {
                        "MODIFIED" => ("≠", Style::new().fg(theme::WARN)),
                        "DELETED" => ("✗", Style::new().fg(theme::ERR)),
                        "IN_SYNC" => ("✓", Style::new().fg(theme::OK)),
                        _ => ("·", theme::muted()),
                    };
                    let mut line = Line::from(vec![
                        Span::styled(if self.focused && i == self.cursor { "▶ " } else { "  " }, theme::accent()),
                        Span::styled(format!("{symbol} "), style),
                        Span::styled(d.logical_id.clone(), theme::bold()),
                        Span::styled(format!("  {}  {}", d.resource_type.trim_start_matches("AWS::"), d.status), theme::muted()),
                    ]);
                    if self.focused && i == self.cursor {
                        line = line.patch_style(theme::selected());
                    }
                    lines.push(line);
                    for diff in &d.differences {
                        lines.push(Line::from(vec![
                            Span::styled(format!("      {} {}  ", diff.kind, diff.path), theme::muted()),
                            Span::styled(diff.expected.clone().unwrap_or_else(|| "—".into()), theme::muted()),
                            Span::raw(" → "),
                            Span::styled(diff.actual.clone().unwrap_or_else(|| "—".into()), Style::new().fg(theme::WARN)),
                        ]));
                    }
                }
            }
        }
    }

    fn render_confirm(&self, f: &mut Frame, area: Rect) {
        let Some(name) = &self.confirm_drift else { return };
        let text = Text::from(vec![
            Line::raw(""),
            Line::raw(format!("Detect drift of {name}?")),
            Line::styled("CloudFormation compares each resource with the template. It changes nothing", theme::muted()),
            Line::styled("and can take a minute on big stacks.", theme::muted()),
            Line::raw(""),
            widgets::hints_line(&[("Enter", "detect"), ("Esc", "cancel")]),
        ]);
        let rect = widgets::popup_area(area, 82, 8);
        widgets::popup(f, rect, "Drift detection", theme::ACCENT, text);
    }
}

impl Module for CloudFormation {
    fn title(&self) -> &'static str {
        "CloudFormation"
    }

    fn description(&self) -> &'static str {
        "CloudFormation stacks · events, resources, outputs, drift, change sets, failure reports"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = CloudFormation::new();
        self.env_name = ctx.env().name.clone();
        self.region = ctx.env().region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load_stacks(ctx);
        }
    }

    /// Shows the stack `target` (a name or ID).
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        self.zoom = None;
        self.confirm_drift = None;
        self.pending_open = Some(target.to_string());
        if matches!(self.load, LoadState::Loaded) && !self.try_open() {
            self.pending_open = None;
            ctx.toast(Level::Warn, format!("Stack not found here: {target}"));
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some((_, view)) = &mut self.zoom {
            if let LogKey::Copy = view.key(key) {
                let ok = crate::platform::copy_to_clipboard(&view.lines().join("\n"));
                ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Copied the template" } else { "Clipboard not available" });
            }
            if !view.zoomed {
                self.zoom = None;
            }
            return Handled::Yes;
        }
        if self.confirm_drift.is_some() {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => self.start_drift(ctx),
                KeyCode::Esc | KeyCode::Char('n') => self.confirm_drift = None,
                _ => {}
            }
            return Handled::Yes;
        }
        if self.filtering {
            let selected = self.list_state.selected().unwrap_or(0);
            match self.filter.handle(key) {
                InputEvent::Changed => self.refilter(),
                InputEvent::Submit => self.filtering = false,
                InputEvent::Cancel => {
                    self.filtering = false;
                    self.filter.clear();
                    self.refilter();
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
        // Keys that work the same with or without the rows focused.
        match key.code {
            KeyCode::Left => {
                self.step_tab(false, ctx);
                return Handled::Yes;
            }
            KeyCode::Right => {
                self.step_tab(true, ctx);
                return Handled::Yes;
            }
            KeyCode::Char('c') => {
                self.copy_report(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('y') => {
                self.copy_row(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('d') => {
                self.confirm_drift = self.selected().map(|s| s.name.clone());
                return Handled::Yes;
            }
            KeyCode::Char('T') => {
                self.show_template(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('o') => {
                if let Some(s) = self.selected() {
                    let url = cfn::console_url(&self.region, &s.id_or_name());
                    if !crate::platform::open_url(&url) {
                        ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                    }
                }
                return Handled::Yes;
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                return Handled::Yes;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                return Handled::Yes;
            }
            _ => {}
        }
        if self.focused {
            let count = self.row_count();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(count.saturating_sub(1)),
                KeyCode::Enter => self.activate_row(ctx),
                KeyCode::Esc | KeyCode::Backspace => self.focused = false,
                _ => return Handled::No,
            }
            return Handled::Yes;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::Enter => {
                if self.row_count() > 0 {
                    self.focused = true;
                    self.cursor = self.cursor.min(self.row_count() - 1);
                } else if self.tab == Tab::Overview {
                    self.set_tab(Tab::Events, ctx);
                    self.focused = self.row_count() > 0;
                }
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Char('r') => {
                if let Some(s) = self.selected() {
                    let name = s.name.clone();
                    let detecting = self.cache.remove(&name).and_then(|c| c.detecting);
                    self.cache(&name).detecting = detecting;
                }
                self.load_stacks(ctx);
                self.load_tab(ctx);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if self.zoom.is_some() || self.confirm_drift.is_some() || !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                // Rows start below the filter bar and the panel border.
                let row = event.row.saturating_sub(self.list_area.y + 2) as usize;
                self.focused = false;
                self.select(self.list_state.offset() + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Stacks(result) => match result {
                Ok(stacks) => {
                    self.stacks = stacks;
                    self.load = LoadState::Loaded;
                    self.refilter();
                    if !self.try_open() {
                        self.select(self.list_state.selected().unwrap_or(0));
                    }
                }
                Err(e) => self.load = LoadState::Failed(e),
            },
            Msg::Stack { name, result } => {
                self.in_flight.remove(&(name.clone(), "stack"));
                if let Ok(Some(fresh)) = result.map(|v| v.into_iter().next()) {
                    let was_running = self.stacks.iter().find(|s| s.name == name).is_some_and(|s| s.status.ends_with("_IN_PROGRESS"));
                    let failed_now = fresh.failed() && !fresh.status.ends_with("_IN_PROGRESS");
                    if let Some(s) = self.stacks.iter_mut().find(|s| s.name == name) {
                        *s = fresh;
                    }
                    if was_running && failed_now {
                        self.cache(&name).trail = None;
                    }
                }
            }
            Msg::Events { stack, result } => {
                self.in_flight.remove(&(stack.clone(), "events"));
                let cache = self.cache(&stack);
                cache.events = Some(result);
                cache.events_at = Some(Instant::now());
                // A failure that just ended an operation: explain it.
                if let Some(s) = self.stacks.iter().find(|s| s.name == stack).cloned()
                    && s.failed()
                    && self.cached(&stack).is_none_or(|c| c.trail.is_none())
                {
                    self.fetch(&s.clone(), "trail", ctx, move |aws, stack, _| Msg::Trail { result: cfn::trail(aws, &s), stack });
                }
            }
            Msg::Trail { stack, result } => {
                self.in_flight.remove(&(stack.clone(), "trail"));
                self.cache(&stack).trail = Some(result);
            }
            Msg::Resources { stack, result } => {
                self.in_flight.remove(&(stack.clone(), "resources"));
                self.cache(&stack).resources = Some(result);
            }
            Msg::Template { stack, result } => {
                self.in_flight.remove(&(stack.clone(), "template"));
                let ok = result.is_ok();
                self.cache(&stack).template = Some(result);
                if ok && self.selected().is_some_and(|s| s.name == stack) {
                    self.show_template(ctx);
                }
            }
            Msg::DriftStarted { stack, result } => {
                self.in_flight.remove(&(stack.clone(), "detect"));
                match result {
                    Ok(id) => self.cache(&stack).detecting = Some((id, Instant::now())),
                    Err(e) => ctx.toast(Level::Error, format!("Drift detection did not start: {}", e.short())),
                }
            }
            Msg::DriftStatus { stack, result } => {
                self.in_flight.remove(&(stack.clone(), "drift status"));
                match result {
                    Ok(d) if d.status == "DETECTION_IN_PROGRESS" => {}
                    Ok(d) if d.status == "DETECTION_FAILED" => {
                        self.cache(&stack).detecting = None;
                        ctx.toast(Level::Error, format!("Drift detection failed: {}", d.reason.unwrap_or_default()));
                    }
                    Ok(d) => {
                        self.cache(&stack).detecting = None;
                        let drifted = d.drifted.unwrap_or(0);
                        let level = if d.stack_drift.as_deref() == Some("DRIFTED") { Level::Warn } else { Level::Success };
                        let what = match drifted {
                            0 => "no resource drifted".to_string(),
                            1 => "1 resource drifted".to_string(),
                            n => format!("{n} resources drifted"),
                        };
                        ctx.toast(level, format!("{stack}: {what}"));
                        if let Some(s) = self.stacks.iter().find(|s| s.name == stack).cloned() {
                            self.fetch(&s, "drifts", ctx, |aws, stack, id| Msg::Drifts { result: cfn::resource_drifts(aws, &id), stack });
                            self.fetch(&s, "stack", ctx, |aws, name, id| Msg::Stack { result: cfn::describe_stacks(aws, Some(&id)), name });
                            self.cache(&stack).resources = None;
                        }
                    }
                    Err(e) => {
                        self.cache(&stack).detecting = None;
                        ctx.toast(Level::Error, e.short());
                    }
                }
            }
            Msg::Drifts { stack, result } => {
                self.in_flight.remove(&(stack.clone(), "drifts"));
                self.cache(&stack).drifts = Some(result);
            }
            Msg::ChangeSets { stack, result } => {
                self.in_flight.remove(&(stack.clone(), "change sets"));
                self.cache(&stack).change_sets = Some(result);
            }
            Msg::Changes { stack, set, result } => {
                self.in_flight.remove(&(stack.clone(), "changes"));
                self.cache(&stack).changes.insert(set, result);
            }
            Msg::Report { what, result } => {
                if let Some(s) = self.selected().map(|s| s.name.clone()) {
                    self.in_flight.remove(&(s, "report"));
                }
                self.in_flight.retain(|(_, w)| *w != "report");
                match result {
                    Ok(Some(md)) => {
                        let label = self.selected().map(|s| s.name.clone()).unwrap_or_else(|| "cloudformation".into());
                        copy(ctx, &label, &md, &format!("{what}. Paste it into your AI agent."));
                    }
                    Ok(None) => ctx.toast(Level::Info, "Nothing failed in the latest operation"),
                    Err(e) => ctx.toast(Level::Error, e.short()),
                }
            }
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if let Some((name, at)) = &self.pending
            && at.elapsed() >= DEBOUNCE
        {
            let name = name.clone();
            self.pending = None;
            if self.selected().is_some_and(|s| s.name == name) {
                self.load_tab(ctx);
                // The events tell why a failed stack failed; the overview shows it.
                if self.tab != Tab::Events
                    && self.cached(&name).is_none_or(|c| c.events.is_none())
                    && let Some(s) = self.selected().cloned()
                {
                    self.load_events(&s, ctx);
                }
            }
        }
        // Drift detections keep going even when another stack (or module) is shown.
        let polls: Vec<(String, String)> = self
            .cache
            .iter()
            .filter_map(|(name, c)| c.detecting.as_ref().filter(|(_, at)| at.elapsed() >= DRIFT_POLL).map(|(id, _)| (name.clone(), id.clone())))
            .collect();
        for (name, id) in polls {
            if let Some((_, at)) = self.cache(&name).detecting.as_mut() {
                *at = Instant::now();
            }
            if self.in_flight.insert((name.clone(), "drift status")) {
                let aws = ctx.env().aws.clone();
                ctx.spawn(move || Msg::DriftStatus { result: cfn::drift_detection(&aws, &id), stack: name });
            }
        }
        if !active {
            return;
        }
        // An operation running: follow the selected stack and its events.
        if let Some(s) = self.selected().cloned()
            && s.status.ends_with("_IN_PROGRESS")
            && self.cached(&s.name).and_then(|c| c.events_at).is_none_or(|at| at.elapsed() >= LIVE_REFRESH)
        {
            self.cache(&s.name).events_at = Some(Instant::now());
            self.fetch(&s, "stack", ctx, |aws, name, id| Msg::Stack { result: cfn::describe_stacks(aws, Some(&id)), name });
            self.fetch(&s, "events", ctx, |aws, stack, id| Msg::Events { result: cfn::stack_events(aws, &id, 0.0, 3), stack });
        }
        let running = self.stacks.iter().any(|s| s.status.ends_with("_IN_PROGRESS"));
        if running && !matches!(self.load, LoadState::Loading) && self.listed_at.elapsed() >= LIST_REFRESH {
            self.load_stacks(ctx);
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
            area.layout(&Layout::horizontal([Constraint::Percentage(34), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        self.render_detail(f, right, view);
        self.render_confirm(f, area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.zoom.is_some() {
            return vec![("/", "search"), ("y", "copy"), ("Esc", "back")];
        }
        if self.confirm_drift.is_some() {
            return vec![("Enter", "detect drift"), ("Esc", "cancel")];
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        let copy_for_ai = match self.tab {
            Tab::Drift => ("c", "copy drift for AI"),
            Tab::Changes => ("c", "copy change set for AI"),
            _ => ("c", "copy for AI"),
        };
        if self.focused {
            let enter = match self.tab {
                Tab::Resources => ("Enter", "open"),
                Tab::Changes => ("Enter", "show changes"),
                Tab::Events => ("Enter", "open nested stack"),
                _ => ("Enter", ""),
            };
            let mut hints = vec![("↑↓", "row"), ("←→", "view")];
            if !enter.1.is_empty() {
                hints.push(enter);
            }
            hints.extend([("y", "copy"), copy_for_ai, ("Esc", "stacks")]);
            return hints;
        }
        vec![
            ("↑↓", "stack"),
            ("←→", "view"),
            ("Enter", "rows"),
            copy_for_ai,
            ("d", "drift"),
            ("T", "template"),
            ("/", "search"),
            ("o", "console"),
            ("r", "reload"),
        ]
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || !self.in_flight.is_empty()
            || self.pending.is_some()
            || self.stacks.iter().any(|s| s.status.ends_with("_IN_PROGRESS"))
            || self.cache.values().any(|c| c.detecting.is_some())
    }

    fn capturing(&self) -> bool {
        self.filtering || self.zoom.is_some() || self.confirm_drift.is_some()
    }

    fn wants_mouse(&self) -> bool {
        self.zoom.is_none()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if self.filtering {
            self.filter.insert_str(text, false);
            self.refilter();
        }
    }
}

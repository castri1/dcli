//! DMS: replication tasks with their tables and CDC latency (and where it comes from),
//! replication instances and endpoints. Tasks can be resumed, stopped and reloaded (every
//! table, or chosen ones), instances scaled, endpoint connections tested, and `c` copies a
//! report for an AI agent. The header counts tasks that failed or have tables in error,
//! refreshed every minute while other modules are open.
//!
//! Everything works on [`Replication`]s, so DMS Serverless replications can join the task
//! list once `aws::dms` lists them.

mod views;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::ListState;

use super::common::{LoadState, matches_filter};
use crate::aws::dms::{self, Connection, Endpoint, Instance, Metrics, PrepMode, Replication, StartType, TableState, TableStat};
use crate::aws::lambda::Range;
use crate::aws;
use crate::config::is_production;
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{InputEvent, TextInput};
use crate::util::now_secs;

const DEBOUNCE: Duration = Duration::from_millis(200);
/// How often the task list refreshes while the module is open…
const LIST_REFRESH: Duration = Duration::from_secs(30);
/// …and in the background, for the header, while there are tasks.
const BADGE_REFRESH: Duration = Duration::from_secs(60);
/// In the background when the account has no tasks or they cannot be read.
const QUIET_REFRESH: Duration = Duration::from_secs(600);
/// Right after a change (a task starting or stopping), until this long after it.
const FAST_REFRESH: Duration = Duration::from_secs(3);
const FAST_FOR: Duration = Duration::from_secs(45);
/// An open task's tables and the metrics shown are read again after this long.
const DETAIL_REFRESH: Duration = Duration::from_secs(60);
/// Connection tests are checked this often, for this long.
const TEST_POLL: Duration = Duration::from_secs(3);
const TEST_WAIT: Duration = Duration::from_secs(120);
const MAX_NOTIFICATIONS: usize = 3;
/// A stopped CDC task older than this gets a warning before resuming.
const LONG_STOP: f64 = 86_400.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Tasks,
    Instances,
    Endpoints,
}

const PAGES: [Page; 3] = [Page::Tasks, Page::Instances, Page::Endpoints];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Tables,
    Settings,
}

const TABS: [Tab; 3] = [Tab::Overview, Tab::Tables, Tab::Settings];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Tables => "Tables",
            Tab::Settings => "Settings",
        }
    }
}

/// Which tables the Tables tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableFilter {
    All,
    Errors,
    Loading,
    Validation,
}

impl TableFilter {
    fn label(self) -> &'static str {
        match self {
            TableFilter::All => "all",
            TableFilter::Errors => "in error",
            TableFilter::Loading => "loading",
            TableFilter::Validation => "validation problems",
        }
    }

    fn next(self) -> TableFilter {
        match self {
            TableFilter::All => TableFilter::Errors,
            TableFilter::Errors => TableFilter::Loading,
            TableFilter::Loading => TableFilter::Validation,
            TableFilter::Validation => TableFilter::All,
        }
    }

    fn keeps(self, t: &TableStat) -> bool {
        match self {
            TableFilter::All => true,
            TableFilter::Errors => t.state() == TableState::Error,
            TableFilter::Loading => matches!(t.state(), TableState::Loading | TableState::Queued),
            TableFilter::Validation => t.validation_problem(),
        }
    }
}

enum Msg {
    Listed(aws::Result<(Vec<Instance>, Vec<Replication>)>),
    Inventory(aws::Result<(Vec<Endpoint>, Vec<Connection>)>),
    Full { arn: String, result: Box<aws::Result<Replication>> },
    Tables { arn: String, result: aws::Result<Vec<TableStat>> },
    LogErrors { arn: String, result: aws::Result<Vec<String>> },
    Metrics { key: String, range: Range, result: Box<aws::Result<Metrics>> },
    Classes { instance: String, result: aws::Result<Vec<String>> },
    Acted { result: aws::Result<String> },
    TestStarted { instance: String, endpoint: String, result: aws::Result<Connection> },
    Connections(aws::Result<Vec<Connection>>),
    Report { id: String, result: aws::Result<String> },
}

/// A task opened full screen.
struct TaskView {
    arn: String,
    tab: Tab,
    /// The task with its settings and table mappings.
    full: Option<aws::Result<Replication>>,
    tables: Option<aws::Result<Vec<TableStat>>>,
    tables_at: Instant,
    tables_in_flight: bool,
    table_state: ListState,
    table_filter: TableFilter,
    search: TextInput,
    searching: bool,
    /// Tables marked with Space: (schema, table).
    marked: BTreeSet<(String, String)>,
    /// Recent error lines of its log.
    errors: Option<aws::Result<Vec<String>>>,
    scroll: u16,
}

impl TaskView {
    fn new(arn: String) -> TaskView {
        TaskView {
            arn,
            tab: Tab::Overview,
            full: None,
            tables: None,
            tables_at: Instant::now(),
            tables_in_flight: false,
            table_state: ListState::default(),
            table_filter: TableFilter::All,
            search: TextInput::default(),
            searching: false,
            marked: BTreeSet::new(),
            errors: None,
            scroll: 0,
        }
    }

    /// The tables the Tables tab lists, in order.
    fn shown(&self) -> Vec<&TableStat> {
        let query = self.search.value.trim().to_lowercase();
        match &self.tables {
            Some(Ok(list)) => list.iter().filter(|t| self.table_filter.keeps(t) && matches_filter(&t.name(), &query)).collect(),
            _ => Vec::new(),
        }
    }

    fn selected_table(&self) -> Option<&TableStat> {
        self.shown().get(self.table_state.selected()?).copied()
    }

    fn errored(&self) -> Vec<(String, String)> {
        match &self.tables {
            Some(Ok(list)) => list.iter().filter(|t| t.state() == TableState::Error).map(|t| (t.schema_name.clone(), t.table_name.clone())).collect(),
            _ => Vec::new(),
        }
    }
}

/// What reloading reloads.
#[derive(Debug, Clone, PartialEq)]
enum ReloadChoice {
    Tables(Vec<(String, String)>),
    Everything,
}

#[derive(Clone)]
enum Action {
    Start { arn: String, how: StartType },
    Stop { arn: String },
    Tables { arn: String, tables: Vec<(String, String)>, validate_only: bool },
    Scale { instance: String, class: String, now: bool },
}

enum Dialog {
    ReloadWhat(Picker, String, Vec<ReloadChoice>),
    ReloadTables { form: Form, arn: String, tables: Vec<(String, String)>, busy: bool },
    Classes(Picker, String, Vec<String>),
    Scale { form: Form, instance: String, class: String, busy: bool },
    TestFrom(Picker, String, Vec<String>),
    Confirm { title: String, lines: Vec<String>, typed: Option<(String, TextInput)>, action: Action, busy: bool, error: Option<String> },
}

/// `The table in error`, `The 3 marked tables`.
fn tables_label(what: &str, n: usize) -> String {
    if n == 1 { format!("The table {what}") } else { format!("The {n} tables {what}") }
}

const RELOAD_KINDS: [&str; 2] = ["the rows (data)", "validation only"];
const APPLY_WHEN: [&str; 2] = ["now (the instance restarts)", "in the maintenance window"];

pub struct Dms {
    env_name: String,
    region: String,
    page: Page,
    instances: Vec<Instance>,
    tasks: Vec<Replication>,
    load: LoadState,
    listed_at: Option<Instant>,
    in_flight: bool,
    /// Faster refreshes until then, after a change.
    fast_until: Option<Instant>,
    /// Each task's trouble at the last refresh (none before the first), to notify new ones.
    seen: Option<HashMap<String, bool>>,
    endpoints: Vec<Endpoint>,
    connections: Vec<Connection>,
    inventory: LoadState,
    filter: TextInput,
    filtering: bool,
    /// The rows of the current page (indexes into its list), and the cursor of each page.
    visible: Vec<usize>,
    states: [ListState; 3],
    open: Option<TaskView>,
    range: Range,
    metrics: HashMap<(String, Range), (Instant, aws::Result<Metrics>)>,
    metrics_in_flight: HashSet<(String, Range)>,
    pending_metrics: Option<(String, Instant)>,
    dialog: Option<Dialog>,
    /// Connection tests running: (instance ARN, endpoint ARN, started).
    testing: Vec<(String, String, Instant)>,
    tests_polled: Instant,
    reporting: HashSet<String>,
    pending_open: Option<String>,
    detail_scroll: u16,
    list_area: Rect,
}

impl Dms {
    pub fn new() -> Dms {
        Dms {
            env_name: String::new(),
            region: String::new(),
            page: Page::Tasks,
            instances: Vec::new(),
            tasks: Vec::new(),
            load: LoadState::Idle,
            listed_at: None,
            in_flight: false,
            fast_until: None,
            seen: None,
            endpoints: Vec::new(),
            connections: Vec::new(),
            inventory: LoadState::Idle,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            states: [ListState::default(), ListState::default(), ListState::default()],
            open: None,
            range: Range::Hour,
            metrics: HashMap::new(),
            metrics_in_flight: HashSet::new(),
            pending_metrics: None,
            dialog: None,
            testing: Vec::new(),
            tests_polled: Instant::now(),
            reporting: HashSet::new(),
            pending_open: None,
            detail_scroll: 0,
            list_area: Rect::default(),
        }
    }

    // ---- loading -----------------------------------------------------------------------------

    fn load_list(&mut self, ctx: &mut Ctx) {
        if self.in_flight {
            return;
        }
        self.in_flight = true;
        self.listed_at = Some(Instant::now());
        if matches!(self.load, LoadState::Idle) {
            self.load = LoadState::Loading;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            Msg::Listed(dms::list_instances(&aws).and_then(|instances| {
                let tasks = dms::list_replications(&aws, &instances, false)?;
                Ok((instances, tasks))
            }))
        });
    }

    fn load_inventory(&mut self, ctx: &mut Ctx) {
        self.inventory = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Inventory(dms::list_endpoints(&aws).and_then(|e| Ok((e, dms::connections(&aws)?)))));
    }

    fn refresh_due(&self, active: bool) -> bool {
        let Some(at) = self.listed_at else { return true };
        if self.in_flight {
            return false;
        }
        let every = if self.fast_until.is_some_and(|u| Instant::now() < u) {
            FAST_REFRESH
        } else if active {
            LIST_REFRESH
        } else if matches!(self.load, LoadState::Loaded) && !self.tasks.is_empty() {
            BADGE_REFRESH
        } else {
            QUIET_REFRESH
        };
        at.elapsed() >= every
    }

    fn on_listed(&mut self, result: aws::Result<(Vec<Instance>, Vec<Replication>)>, ctx: &mut Ctx) {
        self.in_flight = false;
        match result {
            Ok((instances, tasks)) => {
                self.notify_new_trouble(&tasks, ctx);
                let keep = self.selected_key();
                self.instances = instances;
                self.tasks = tasks;
                self.load = LoadState::Loaded;
                self.reselect(keep);
            }
            // A failed refresh keeps what was read before.
            Err(_) if matches!(self.load, LoadState::Loaded) => self.refilter(),
            Err(e) => {
                self.load = LoadState::Failed(e);
                self.refilter();
            }
        }
        if let Some(target) = self.pending_open.take() {
            self.open(&target, ctx);
        }
    }

    /// Tells about tasks that failed or got tables in error since the last refresh.
    fn notify_new_trouble(&mut self, tasks: &[Replication], ctx: &mut Ctx) {
        let started: Vec<&Replication> = match &self.seen {
            Some(seen) => tasks.iter().filter(|t| t.in_trouble() && seen.get(&t.arn) != Some(&true)).collect(),
            None => Vec::new(),
        };
        self.seen = Some(tasks.iter().map(|t| (t.arn.clone(), t.in_trouble())).collect());
        for t in started.iter().take(MAX_NOTIFICATIONS) {
            let title = match t.health() {
                dms::Health::Failed => format!("DMS task failed in {}", self.env_name),
                _ => format!("DMS task has tables in error in {}", self.env_name),
            };
            ctx.notify(Level::Error, &title, &t.id);
        }
        if started.len() > MAX_NOTIFICATIONS {
            ctx.notify(Level::Error, &format!("DMS tasks in trouble in {}", self.env_name), &format!("and {} more", started.len() - MAX_NOTIFICATIONS));
        }
    }

    fn load_full(&mut self, arn: String, ctx: &mut Ctx) {
        let aws = ctx.env().aws.clone();
        let instances = self.instances.clone();
        ctx.spawn(move || {
            let result = Box::new(dms::find(&aws, &arn, &instances));
            Msg::Full { arn, result }
        });
    }

    fn load_tables(&mut self, ctx: &mut Ctx) {
        let Some(task) = self.open_task().cloned() else { return };
        let Some(view) = &mut self.open else { return };
        if view.tables_in_flight {
            return;
        }
        view.tables_in_flight = true;
        view.tables_at = Instant::now();
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = dms::table_statistics(&aws, &task);
            Msg::Tables { arn: task.arn, result }
        });
    }

    fn load_log_errors(&mut self, ctx: &mut Ctx) {
        let Some(view) = &self.open else { return };
        let task = match &view.full {
            Some(Ok(t)) => t.clone(),
            _ => return,
        };
        let Some((group, stream)) = task.log_location() else { return };
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = dms::log_errors(&aws, &group, &stream, 86_400.0, now_secs(), 6);
            Msg::LogErrors { arn: task.arn, result }
        });
    }

    /// Metrics of a task (by its ARN) or an instance (`instance:<id>`).
    fn fetch_metrics(&mut self, key: String, ctx: &mut Ctx) {
        let range = self.range;
        if !self.metrics_in_flight.insert((key.clone(), range)) {
            return;
        }
        let aws = ctx.env().aws.clone();
        let task = self.tasks.iter().find(|t| t.arn == key).cloned();
        ctx.spawn(move || {
            let result = match (&task, key.strip_prefix("instance:")) {
                (Some(t), _) => dms::metrics(&aws, t, range, now_secs()),
                (None, Some(id)) => dms::instance_metrics(&aws, id, range, now_secs()),
                _ => Err(aws::AwsError::Config("unknown metrics".into())),
            };
            Msg::Metrics { key, range, result: Box::new(result) }
        });
    }

    /// The metrics key of what is shown: the open or selected task, or the selected instance.
    fn metrics_key(&self) -> Option<String> {
        if let Some(view) = &self.open {
            return Some(view.arn.clone());
        }
        match self.page {
            Page::Tasks => self.selected_task().map(|t| t.arn.clone()),
            Page::Instances => self.selected_instance().map(|i| format!("instance:{}", i.id)),
            Page::Endpoints => None,
        }
    }

    fn metrics_of(&self, key: &str) -> Option<&aws::Result<Metrics>> {
        self.metrics.get(&(key.to_string(), self.range)).map(|(_, m)| m)
    }

    /// Reads the metrics shown after a short pause (arrow-key scrolling), or again when old.
    fn queue_metrics(&mut self) {
        let Some(key) = self.metrics_key() else { return };
        let stale = self.metrics.get(&(key.clone(), self.range)).is_none_or(|(at, _)| at.elapsed() >= DETAIL_REFRESH);
        if stale && !self.metrics_in_flight.contains(&(key.clone(), self.range)) {
            self.pending_metrics = Some((key, Instant::now()));
        }
    }

    // ---- what is shown -----------------------------------------------------------------------

    fn page_index(&self) -> usize {
        PAGES.iter().position(|p| *p == self.page).unwrap_or(0)
    }

    fn state(&mut self) -> &mut ListState {
        let i = self.page_index();
        &mut self.states[i]
    }

    fn page_len(&self) -> usize {
        match self.page {
            Page::Tasks => self.tasks.len(),
            Page::Instances => self.instances.len(),
            Page::Endpoints => self.endpoints.len(),
        }
    }

    fn row_matches(&self, i: usize, query: &str) -> bool {
        match self.page {
            Page::Tasks => {
                let t = &self.tasks[i];
                matches_filter(&t.id, query) || t.instance.as_deref().is_some_and(|n| matches_filter(n, query)) || t.status.contains(query)
            }
            Page::Instances => {
                let i = &self.instances[i];
                matches_filter(&i.id, query) || matches_filter(&i.class, query)
            }
            Page::Endpoints => {
                let e = &self.endpoints[i];
                matches_filter(&e.id, query) || matches_filter(&e.engine_label, query) || matches_filter(&e.address(), query)
            }
        }
    }

    /// The rows again after the filter or the page changed (the data did not): the selected
    /// row stays selected while it shows, else the cursor stays where it was.
    fn refilter(&mut self) {
        let keep = self.selected_key();
        self.rebuild(keep, false);
    }

    /// The rows again after new data replaced the list: `keep` (the key of the row selected
    /// before) stays selected wherever it moved; when it is gone nothing is selected, so that
    /// a key cannot act on whatever took its place.
    fn reselect(&mut self, keep: Option<String>) {
        self.rebuild(keep, true);
    }

    /// Tasks in trouble first, then running ones, then by name.
    fn rebuild(&mut self, keep: Option<String>, new_data: bool) {
        let query = self.filter.value.trim().to_lowercase();
        let mut visible: Vec<usize> = (0..self.page_len()).filter(|&i| self.row_matches(i, &query)).collect();
        if self.page == Page::Tasks {
            visible.sort_by_key(|&i| (self.tasks[i].health(), self.tasks[i].id.to_lowercase()));
        }
        self.visible = visible;
        let last = self.visible.len().saturating_sub(1);
        let index = match keep.map(|k| self.visible.iter().position(|&i| self.key_of(i) == k)) {
            Some(found) if new_data => found,
            Some(Some(found)) => Some(found),
            _ => Some(self.state().selected().unwrap_or(0).min(last)),
        };
        let empty = self.visible.is_empty();
        self.state().select(if empty { None } else { index });
        self.queue_metrics();
    }

    fn key_of(&self, i: usize) -> String {
        match self.page {
            Page::Tasks => self.tasks[i].arn.clone(),
            Page::Instances => self.instances[i].arn.clone(),
            Page::Endpoints => self.endpoints[i].arn.clone(),
        }
    }

    fn selected_key(&self) -> Option<String> {
        let i = *self.visible.get(self.states[self.page_index()].selected()?)?;
        (i < self.page_len()).then(|| self.key_of(i))
    }

    fn selected_index(&self) -> Option<usize> {
        let i = *self.visible.get(self.states[self.page_index()].selected()?)?;
        (i < self.page_len()).then_some(i)
    }

    fn selected_task(&self) -> Option<&Replication> {
        (self.page == Page::Tasks).then(|| self.selected_index().and_then(|i| self.tasks.get(i))).flatten()
    }

    fn selected_instance(&self) -> Option<&Instance> {
        (self.page == Page::Instances).then(|| self.selected_index().and_then(|i| self.instances.get(i))).flatten()
    }

    fn selected_endpoint(&self) -> Option<&Endpoint> {
        (self.page == Page::Endpoints).then(|| self.selected_index().and_then(|i| self.endpoints.get(i))).flatten()
    }

    /// The task acted on: the open one (with its settings once read), else the selected one.
    fn current_task(&self) -> Option<&Replication> {
        match &self.open {
            Some(view) => match &view.full {
                Some(Ok(t)) => Some(t),
                _ => self.tasks.iter().find(|t| t.arn == view.arn),
            },
            None => self.selected_task(),
        }
    }

    fn open_task(&self) -> Option<&Replication> {
        let view = self.open.as_ref()?;
        match &view.full {
            Some(Ok(t)) => Some(t),
            _ => self.tasks.iter().find(|t| t.arn == view.arn),
        }
    }

    fn instance_of(&self, t: &Replication) -> Option<&Instance> {
        self.instances.iter().find(|i| Some(&i.arn) == t.instance_arn.as_ref())
    }

    fn endpoint(&self, arn: &str) -> Option<&Endpoint> {
        self.endpoints.iter().find(|e| e.arn == arn)
    }

    fn select(&mut self, index: usize) {
        if self.visible.is_empty() {
            return;
        }
        let index = index.min(self.visible.len() - 1);
        if self.state().selected() != Some(index) {
            self.detail_scroll = 0;
        }
        self.state().select(Some(index));
        self.queue_metrics();
    }

    fn switch_page(&mut self, forward: bool) {
        let i = self.page_index();
        self.page = PAGES[if forward { (i + 1) % PAGES.len() } else { (i + PAGES.len() - 1) % PAGES.len() }];
        self.filter.clear();
        self.filtering = false;
        self.detail_scroll = 0;
        // The visible rows are still the other page's: no key to keep from them.
        self.rebuild(None, false);
    }

    fn show_page(&mut self, page: Page) {
        self.page = page;
        self.filter.clear();
        self.filtering = false;
        self.detail_scroll = 0;
        self.rebuild(None, false);
    }

    fn open_view(&mut self, arn: String, ctx: &mut Ctx) {
        self.open = Some(TaskView::new(arn.clone()));
        self.load_full(arn.clone(), ctx);
        self.load_tables(ctx);
        self.queue_metrics();
    }

    // ---- actions -----------------------------------------------------------------------------

    fn confirm(&self, title: String, lines: Vec<String>, action: Action) -> Dialog {
        let typed = is_production(&self.env_name).then(|| (self.env_name.clone(), TextInput::default()));
        Dialog::Confirm { title, lines, typed, action, busy: false, error: None }
    }

    fn open_start(&mut self, ctx: &mut Ctx) {
        let Some(t) = self.current_task().cloned() else { return };
        if t.running() {
            return ctx.toast(Level::Info, format!("{} is {}", t.id, t.status));
        }
        if !matches!(t.status.as_str(), "stopped" | "failed" | "ready") {
            return ctx.toast(Level::Info, format!("{} is {}: wait until it settles", t.id, t.status));
        }
        if t.finished_full_load() {
            return ctx.toast(Level::Info, format!("{} is a full load that finished: R loads it again", t.id));
        }
        let how = t.start_type();
        let mut lines = vec![match how {
            StartType::Start => format!("Start {} for the first time ({})?", t.id, t.migration.label()),
            _ => format!("Resume {} where it stopped ({})?", t.id, t.migration.label()),
        }];
        if let Some(stopped) = t.stopped_at().filter(|s| t.migration.has_cdc() && now_secs() - s >= LONG_STOP) {
            lines.push(format!(
                "It stopped {} ago: resuming needs the source to still keep its change log (binlog, WAL, redo) from then.",
                dms::fmt_since(now_secs() - stopped)
            ));
            if let Some(cp) = &t.recovery_checkpoint {
                lines.push(format!("It resumes from checkpoint {cp}."));
            }
        }
        if let Some(f) = &t.last_failure {
            lines.push(format!("It stopped with: {}", crate::util::ellipsize(f, 200)));
        }
        let title = format!("{} {}", how.verb(), t.id);
        let action = Action::Start { arn: t.arn.clone(), how };
        let prep = PrepMode::of(t.settings.as_deref());
        if how == StartType::Start && prep.rewrites_target() {
            // A first start loads the tables like a full reload: the same name to type.
            lines.push(format!("Loading, the task {}.", prep.effect()));
            let typed = Some((t.id.clone(), TextInput::default()));
            self.dialog = Some(Dialog::Confirm { title, lines, typed, action, busy: false, error: None });
        } else {
            self.dialog = Some(self.confirm(title, lines, action));
        }
    }

    fn open_stop(&mut self, ctx: &mut Ctx) {
        let Some(t) = self.current_task().cloned() else { return };
        if !t.running() {
            return ctx.toast(Level::Info, format!("{} is {}, not running", t.id, t.status));
        }
        let mut lines = vec![format!("Stop {}?", t.id)];
        if t.migration.has_cdc() {
            lines.push("Changes on the source wait until it resumes (as long as the source keeps its change log).".into());
        }
        if t.loading() {
            lines.push("Tables still loading start their load again when it resumes.".into());
        }
        self.dialog = Some(self.confirm(format!("Stop {}", t.id), lines, Action::Stop { arn: t.arn.clone() }));
    }

    /// `R`: chooses what to reload.
    fn open_reload(&mut self, ctx: &mut Ctx) {
        let Some(t) = self.current_task().cloned() else { return };
        if !matches!(t.status.as_str(), "running" | "stopped" | "failed" | "ready") {
            return ctx.toast(Level::Info, format!("{} is {}: wait until it settles", t.id, t.status));
        }
        let mut items = Vec::new();
        let mut choices: Vec<ReloadChoice> = Vec::new();
        let mut offer = |item: Item, tables: Vec<(String, String)>| {
            // The same tables offered twice (the marked ones are those in error) once.
            let mut sorted = tables.clone();
            sorted.sort();
            let same = |c: &ReloadChoice| matches!(c, ReloadChoice::Tables(t) if { let mut o = t.clone(); o.sort(); o == sorted });
            if tables.is_empty() || !choices.iter().any(same) {
                items.push(item);
                choices.push(ReloadChoice::Tables(tables));
            }
        };
        if let Some(view) = &self.open {
            if !view.marked.is_empty() {
                offer(Item::new(tables_label("marked", view.marked.len()), "ReloadTables: only those"), view.marked.iter().cloned().collect());
            }
            let errored = view.errored();
            if !errored.is_empty() {
                offer(Item::new(tables_label("in error", errored.len()), "ReloadTables: only those").color(theme::ERR), errored);
            }
            if view.tab == Tab::Tables
                && let Some(table) = view.selected_table()
            {
                offer(Item::new(format!("{} (selected)", table.name()), "ReloadTables: only this one"), vec![(table.schema_name.clone(), table.table_name.clone())]);
            }
        } else if t.stats.tables_errored > 0 {
            offer(Item::new(tables_label("in error", t.stats.tables_errored as usize), "open the task (Enter) to pick them").color(theme::ERR), Vec::new());
        }
        let prep = PrepMode::of(t.settings.as_deref());
        let detail = match (t.running(), t.settings.is_some()) {
            (true, _) => "stop the task first (x)".to_string(),
            (false, true) => format!("reload-target · {}", prep.label()),
            (false, false) => "reload-target".into(),
        };
        items.push(Item::new("Every table of the task", detail).color(theme::WARN));
        choices.push(ReloadChoice::Everything);
        self.dialog = Some(Dialog::ReloadWhat(Picker::new(format!("Reload {}", t.id), "choose", items, false), t.arn.clone(), choices));
    }

    fn chose_reload(&mut self, arn: String, choice: ReloadChoice, ctx: &mut Ctx) {
        let Some(t) = self.current_task().filter(|t| t.arn == arn).cloned() else { return };
        match choice {
            ReloadChoice::Tables(tables) if tables.is_empty() => {
                // From the list: open the task on its tables in error.
                self.dialog = None;
                self.open_view(arn, ctx);
                if let Some(view) = &mut self.open {
                    view.tab = Tab::Tables;
                    view.table_filter = TableFilter::Errors;
                }
                ctx.toast(Level::Info, "Space marks tables, E marks those in error, R reloads them");
            }
            ReloadChoice::Tables(tables) => {
                let names: Vec<String> = tables.iter().map(|(s, n)| format!("{s}.{n}")).collect();
                let mut form = Form::new(
                    format!("Reload tables · {}", t.id),
                    vec![
                        Field::fixed("Tables", crate::util::ellipsize(&names.join(", "), 300)),
                        Field::choice("Reload", &RELOAD_KINDS, RELOAD_KINDS[0]),
                    ],
                )
                .submit_label("reload");
                let prep = PrepMode::of(t.settings.as_deref());
                form.note = Some(format!(
                    "Reloading the rows {}. Validation only checks the rows again without loading them.",
                    prep.effect()
                ));
                if is_production(&self.env_name) {
                    form = form.confirm_with(&self.env_name);
                }
                self.dialog = Some(Dialog::ReloadTables { form, arn, tables, busy: false });
            }
            ReloadChoice::Everything if t.running() => {
                self.dialog = None;
                ctx.toast(Level::Info, format!("{} is running: stop it first (x), then reload every table", t.id));
            }
            ReloadChoice::Everything => {
                let prep = PrepMode::of(t.settings.as_deref());
                let lines = vec![
                    format!("Reload every table of {} from the source ({}).", t.id, t.migration.label()),
                    format!("The task {}.", prep.effect()),
                    "Then it carries on with the changes, if it replicates them.".into(),
                ];
                // Always the task's name, whatever the environment: this rewrites the target.
                let typed = Some((t.id.clone(), TextInput::default()));
                self.dialog = Some(Dialog::Confirm {
                    title: format!("Reload {}", t.id),
                    lines,
                    typed,
                    action: Action::Start { arn, how: StartType::Reload },
                    busy: false,
                    error: None,
                });
            }
        }
    }

    /// `S`: the instance of the task, or the selected instance, to another class.
    fn open_scale(&mut self, ctx: &mut Ctx) {
        let instance = match self.page {
            Page::Instances if self.open.is_none() => self.selected_instance().cloned(),
            _ => self.current_task().and_then(|t| self.instance_of(t)).cloned(),
        };
        let Some(instance) = instance else { return ctx.toast(Level::Info, "Select a task or an instance") };
        if instance.status != "available" {
            return ctx.toast(Level::Info, format!("{} is {}: wait until it is available", instance.id, instance.status));
        }
        ctx.toast(Level::Info, format!("Reading the classes {} can move to…", instance.id));
        let aws = ctx.env().aws.clone();
        let (arn, version) = (instance.arn.clone(), instance.engine_version.clone());
        ctx.spawn(move || {
            let result = dms::orderable_classes(&aws, &version);
            Msg::Classes { instance: arn, result }
        });
    }

    fn on_classes(&mut self, instance: String, result: aws::Result<Vec<String>>, ctx: &mut Ctx) {
        let Some(i) = self.instances.iter().find(|i| i.arn == instance).cloned() else { return };
        let classes = match result {
            Ok(c) if !c.is_empty() => c,
            Ok(_) => return ctx.toast(Level::Warn, format!("No class is offered for engine {}", i.engine_version)),
            Err(e) => return ctx.toast(Level::Error, format!("Could not read the classes: {}", e.short())),
        };
        let key = format!("instance:{}", i.id);
        let usage = self.metrics_of(&key).or_else(|| self.tasks.iter().find(|t| t.instance_arn.as_ref() == Some(&i.arn)).and_then(|t| self.metrics_of(&t.arn)));
        let now = match usage {
            Some(Ok(m)) => {
                let free = m.last("i_freeable").map(|f| crate::util::fmt_bytes(f as i64)).unwrap_or_else(|| "?".into());
                let cpu = m.max("i_cpu").map(|c| format!("{c:.0}%")).unwrap_or_else(|| "?".into());
                format!(" · now {free} free, CPU up to {cpu}")
            }
            _ => String::new(),
        };
        let items: Vec<Item> = classes
            .iter()
            .map(|c| {
                let detail = match dms::class_specs(c) {
                    Some((v, g)) => format!("{v} vCPU · {g:.0} GiB"),
                    None => String::new(),
                };
                Item::new(c.clone(), detail).current(*c == i.class)
            })
            .collect();
        let cursor = classes.iter().position(|c| *c == i.class).unwrap_or(0);
        let picker = Picker::new(format!("{} ({}){now}", i.id, i.class), "choose", items, false).with_cursor(cursor);
        self.dialog = Some(Dialog::Classes(picker, i.arn.clone(), classes));
    }

    fn chose_class(&mut self, instance: String, class: String, ctx: &mut Ctx) {
        let Some(i) = self.instances.iter().find(|i| i.arn == instance).cloned() else { return };
        if class == i.class {
            self.dialog = None;
            return ctx.toast(Level::Info, format!("{} is already {class}", i.id));
        }
        let running: Vec<String> = self.tasks.iter().filter(|t| t.instance_arn.as_ref() == Some(&i.arn) && t.running()).map(|t| t.id.clone()).collect();
        let mut form = Form::new(
            format!("Scale {}", i.id),
            vec![
                Field::fixed("From", dms::class_label(&i.class)),
                Field::fixed("To", dms::class_label(&class)),
                Field::choice("Apply", &APPLY_WHEN, APPLY_WHEN[0]),
            ],
        )
        .submit_label("scale");
        let window = i.maintenance_window.as_deref().unwrap_or("?");
        form.note = Some(match running.as_slice() {
            [] => format!("Changing the class restarts the instance. Its maintenance window: {window}."),
            [one] => format!("Changing the class restarts the instance: {one} stops while it does and resumes after. Its maintenance window: {window}."),
            many => format!(
                "Changing the class restarts the instance: {} stop while it does and resume after. Its maintenance window: {window}.",
                many.join(", ")
            ),
        });
        if is_production(&self.env_name) {
            form = form.confirm_with(&self.env_name);
        }
        self.dialog = Some(Dialog::Scale { form, instance, class, busy: false });
    }

    /// `t` on an endpoint: tests it from an instance (the one its tasks use, or a choice).
    fn open_test(&mut self, ctx: &mut Ctx) {
        let Some(e) = self.selected_endpoint().cloned() else { return };
        if self.testing.iter().any(|(_, ep, _)| *ep == e.arn) {
            return ctx.toast(Level::Info, format!("Still testing {}", e.id));
        }
        let mut arns: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.source_arn == e.arn || t.target_arn == e.arn)
            .filter_map(|t| t.instance_arn.clone())
            .collect();
        arns.extend(self.instances.iter().map(|i| i.arn.clone()));
        let mut seen = HashSet::new();
        arns.retain(|a| seen.insert(a.clone()));
        if arns.is_empty() {
            return ctx.toast(Level::Info, "No replication instance to test from");
        }
        let items: Vec<Item> = arns
            .iter()
            .map(|a| {
                let i = self.instances.iter().find(|i| i.arn == *a);
                let used = self.tasks.iter().any(|t| t.instance_arn.as_ref() == Some(a) && (t.source_arn == e.arn || t.target_arn == e.arn));
                Item::new(i.map_or_else(|| dms::resource_id(a), |i| i.id.clone()), if used { "its tasks run here" } else { "" })
            })
            .collect();
        self.dialog = Some(Dialog::TestFrom(Picker::new(format!("Test {} from", e.id), "test", items, false), e.arn.clone(), arns));
    }

    fn start_test(&mut self, instance: String, endpoint: String, ctx: &mut Ctx) {
        self.dialog = None;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = dms::test_connection(&aws, &instance, &endpoint);
            Msg::TestStarted { instance, endpoint, result }
        });
    }

    fn run(&mut self, action: Action, ctx: &mut Ctx) {
        let aws = ctx.env().aws.clone();
        let tasks = self.tasks.clone();
        let full = self.open_task().cloned();
        let instances = self.instances.clone();
        // The task as last read, with its settings when open (DMS needs only its ARN).
        let task = move |arn: &str| full.clone().filter(|t| t.arn == arn).or_else(|| tasks.iter().find(|t| t.arn == arn).cloned());
        match action {
            Action::Start { arn, how } => {
                let Some(t) = task(&arn) else { return };
                ctx.spawn(move || {
                    let result = dms::start(&aws, &t, how).map(|fresh| match how {
                        StartType::Reload => format!("{} is reloading every table ({})", fresh.id, fresh.status),
                        _ => format!("{} is {}", fresh.id, fresh.status),
                    });
                    Msg::Acted { result }
                });
            }
            Action::Stop { arn } => {
                let Some(t) = task(&arn) else { return };
                ctx.spawn(move || Msg::Acted { result: dms::stop(&aws, &t).map(|fresh| format!("{} is {}", fresh.id, fresh.status)) });
            }
            Action::Tables { arn, tables, validate_only } => {
                let Some(t) = task(&arn) else { return };
                ctx.spawn(move || {
                    let n = tables.len();
                    let result = dms::reload_tables(&aws, &t, &tables, validate_only).map(|()| {
                        let what = if validate_only { "validating again" } else { "reloading" };
                        format!("{} of {} {what}", if n == 1 { format!("{}.{}", tables[0].0, tables[0].1) } else { format!("{n} tables") }, t.id)
                    });
                    Msg::Acted { result }
                });
            }
            Action::Scale { instance, class, now } => {
                let Some(i) = instances.iter().find(|i| i.arn == instance).cloned() else { return };
                let usage = [&i.class, &class].iter().flat_map(|c| crate::costs::ledger::usage_of_instance("DMS", c)).collect();
                let note = crate::costs::ledger::Note::new(&self.env_name, &self.region, "DMS", &i.id, usage, format!("{} from {} to {class}", i.id, i.class));
                ctx.spawn(move || {
                    let result = dms::modify_class(&aws, &i, &class, now).inspect(|_| note.record()).map(|fresh| match (now, &fresh.pending_class) {
                        (false, _) => format!("{} moves to {class} in its next maintenance window", fresh.id),
                        (true, _) => format!("{} is moving to {class} ({})", fresh.id, fresh.status),
                    });
                    Msg::Acted { result }
                });
            }
        }
    }

    fn on_acted(&mut self, result: aws::Result<String>, ctx: &mut Ctx) {
        match result {
            Ok(text) => {
                self.dialog = None;
                ctx.toast(Level::Success, text);
                self.fast_until = Some(Instant::now() + FAST_FOR);
                self.listed_at = None;
                if let Some(view) = &mut self.open {
                    view.marked.clear();
                    let arn = view.arn.clone();
                    self.load_full(arn, ctx);
                    self.load_tables(ctx);
                }
            }
            Err(e) => match &mut self.dialog {
                Some(Dialog::Confirm { busy, error, .. }) => {
                    *busy = false;
                    *error = Some(e.short());
                }
                Some(Dialog::ReloadTables { form, busy, .. } | Dialog::Scale { form, busy, .. }) => {
                    *busy = false;
                    form.fail(e.short());
                }
                _ => ctx.toast(Level::Error, e.short()),
            },
        }
    }

    fn on_test_started(&mut self, instance: String, endpoint: String, result: aws::Result<Connection>, ctx: &mut Ctx) {
        match result {
            Ok(_) => {
                let name = self.endpoints.iter().find(|e| e.arn == endpoint).map_or_else(|| dms::resource_id(&endpoint), |e| e.id.clone());
                ctx.toast(Level::Info, format!("Testing {name}…"));
                self.testing.push((instance, endpoint, Instant::now()));
                self.tests_polled = Instant::now();
            }
            Err(e) => ctx.toast(Level::Error, format!("Could not start the test: {}", e.short())),
        }
    }

    fn on_connections(&mut self, result: aws::Result<Vec<Connection>>, ctx: &mut Ctx) {
        let Ok(list) = result else { return };
        let mut done = Vec::new();
        for (k, (instance, endpoint, started)) in self.testing.iter().enumerate() {
            let found = list.iter().find(|c| c.replication_instance_arn == *instance && c.endpoint_arn == *endpoint);
            match found {
                Some(c) if c.status != "testing" => done.push((k, Some(c.clone()))),
                _ if started.elapsed() >= TEST_WAIT => done.push((k, None)),
                _ => {}
            }
        }
        for (k, c) in done.into_iter().rev() {
            let (_, endpoint, _) = self.testing.remove(k);
            let name = self.endpoints.iter().find(|e| e.arn == endpoint).map_or_else(|| dms::resource_id(&endpoint), |e| e.id.clone());
            match c {
                Some(c) if c.status == "successful" => ctx.toast(Level::Success, format!("{} can reach {name}", c.replication_instance_identifier)),
                Some(c) => ctx.toast(Level::Error, format!("{name}: {} · {}", c.status, c.last_failure_message.unwrap_or_default())),
                None => ctx.toast(Level::Warn, format!("The test of {name} is still running: r reads it again later")),
            }
        }
        self.connections = list;
    }

    fn copy_report(&mut self, ctx: &mut Ctx) {
        let Some(t) = self.current_task() else { return };
        let id = t.id.clone();
        if !self.reporting.insert(id.clone()) {
            return ctx.toast(Level::Info, format!("Still writing the report of {id}"));
        }
        ctx.toast(Level::Info, format!("Writing the report of {id}: tables, metrics and 24 h of its log…"));
        let env = ctx.env();
        let (aws, env_name, region) = (env.aws.clone(), env.name.clone(), env.region.clone());
        ctx.spawn(move || {
            let result = dms::gather_report(&aws, &env_name, &region, &id, now_secs());
            Msg::Report { id, result }
        });
    }

    fn on_report(&mut self, id: String, result: aws::Result<String>, ctx: &mut Ctx) {
        self.reporting.remove(&id);
        let md = match result {
            Ok(md) => md,
            Err(e) => return ctx.toast(Level::Error, format!("Could not write the report: {}", e.short())),
        };
        if crate::platform::copy_to_clipboard(&md) {
            return ctx.toast(Level::Success, format!("Copied the report of {id}. Paste it into your AI agent."));
        }
        match crate::platform::write_temp_file(&format!("dms-{id}"), &md) {
            Ok(path) => ctx.toast(Level::Success, format!("Saved the report to {}", path.display())),
            Err(e) => ctx.toast(Level::Error, format!("Could not save the report: {e}")),
        }
    }

    fn open_logs(&mut self, ctx: &mut Ctx) {
        if self.open.is_none() && self.page == Page::Instances {
            if let Some(i) = self.selected_instance() {
                ctx.jump("Logs", format!("dms-tasks-{}", i.id));
            }
            return;
        }
        let Some(t) = self.current_task() else { return };
        let logging_off = t
            .settings
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v.pointer("/Logging/EnableLogging").and_then(serde_json::Value::as_bool))
            == Some(false);
        if logging_off {
            return ctx.toast(Level::Info, format!("{} has logging off: there is no task log", t.id));
        }
        match t.log_location() {
            Some((group, _)) => ctx.jump("Logs", group),
            None => ctx.toast(Level::Info, "The task's log group is not known"),
        }
    }

    fn copy(&self, text: &str, what: &str, ctx: &mut Ctx) {
        let ok = crate::platform::copy_to_clipboard(text);
        ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { format!("Copied {what}") } else { "Clipboard not available".into() });
    }

    fn open_console(&self, ctx: &mut Ctx) {
        let url = match (&self.open, self.page) {
            (Some(_), _) | (None, Page::Tasks) => self.current_task().map(|t| dms::console_task(&self.region, &t.id)),
            (None, Page::Instances) => self.selected_instance().map(|i| dms::console_instance(&self.region, &i.id)),
            (None, Page::Endpoints) => self.selected_endpoint().map(|e| dms::console_endpoint(&self.region, &e.id)),
        };
        if let Some(url) = url
            && !crate::platform::open_url(&url)
        {
            ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
        }
    }

    fn copy_arn(&self, ctx: &mut Ctx) {
        let arn = match (&self.open, self.page) {
            (Some(_), _) | (None, Page::Tasks) => self.current_task().map(|t| t.arn.clone()),
            (None, Page::Instances) => self.selected_instance().map(|i| i.arn.clone()),
            (None, Page::Endpoints) => self.selected_endpoint().map(|e| e.arn.clone()),
        };
        if let Some(arn) = arn {
            self.copy(&arn, "the ARN", ctx);
        }
    }

    // ---- keys --------------------------------------------------------------------------------

    fn dialog_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(dialog) = &mut self.dialog else { return };
        match dialog {
            Dialog::ReloadWhat(picker, arn, choices) => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.dialog = None,
                Outcome::Done(chosen) => {
                    let (arn, choice) = (arn.clone(), chosen.first().and_then(|&i| choices.get(i).cloned()));
                    if let Some(choice) = choice {
                        self.chose_reload(arn, choice, ctx);
                    }
                }
            },
            Dialog::Classes(picker, instance, classes) => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.dialog = None,
                Outcome::Done(chosen) => {
                    let (instance, class) = (instance.clone(), chosen.first().and_then(|&i| classes.get(i).cloned()));
                    if let Some(class) = class {
                        self.chose_class(instance, class, ctx);
                    }
                }
            },
            Dialog::TestFrom(picker, endpoint, instances) => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.dialog = None,
                Outcome::Done(chosen) => {
                    let (endpoint, instance) = (endpoint.clone(), chosen.first().and_then(|&i| instances.get(i).cloned()));
                    if let Some(instance) = instance {
                        self.start_test(instance, endpoint, ctx);
                    }
                }
            },
            Dialog::ReloadTables { busy: true, .. } | Dialog::Scale { busy: true, .. } | Dialog::Confirm { busy: true, .. } => {}
            Dialog::ReloadTables { form, arn, tables, busy } => match form.handle(key) {
                FormEvent::Pending => {}
                FormEvent::Cancel => self.dialog = None,
                FormEvent::Submit => {
                    *busy = true;
                    let action = Action::Tables { arn: arn.clone(), tables: tables.clone(), validate_only: form.get("Reload") == RELOAD_KINDS[1] };
                    self.run(action, ctx);
                }
            },
            Dialog::Scale { form, instance, class, busy } => match form.handle(key) {
                FormEvent::Pending => {}
                FormEvent::Cancel => self.dialog = None,
                FormEvent::Submit => {
                    *busy = true;
                    let action = Action::Scale { instance: instance.clone(), class: class.clone(), now: form.get("Apply") == APPLY_WHEN[0] };
                    self.run(action, ctx);
                }
            },
            Dialog::Confirm { typed: Some((name, input)), action, busy, error, .. } => match input.handle(key) {
                InputEvent::Cancel => self.dialog = None,
                InputEvent::Submit if input.value.trim() == name.as_str() => {
                    *busy = true;
                    let action = action.clone();
                    self.run(action, ctx);
                }
                InputEvent::Submit => *error = Some(format!("Type '{name}' to confirm")),
                _ => {}
            },
            Dialog::Confirm { action, busy, .. } => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    *busy = true;
                    let action = action.clone();
                    self.run(action, ctx);
                }
                KeyCode::Esc | KeyCode::Char('n') => self.dialog = None,
                _ => {}
            },
        }
    }

    /// Keys shared by the list and the open task.
    fn action_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> bool {
        match key.code {
            KeyCode::Char('s') => self.open_start(ctx),
            KeyCode::Char('x') => self.open_stop(ctx),
            KeyCode::Char('R') => self.open_reload(ctx),
            KeyCode::Char('S') => self.open_scale(ctx),
            KeyCode::Char('c') => self.copy_report(ctx),
            KeyCode::Char('l') => self.open_logs(ctx),
            KeyCode::Char('m') => {
                self.range = self.range.next();
                self.queue_metrics();
            }
            KeyCode::Char('y') => self.copy_arn(ctx),
            KeyCode::Char('o') => self.open_console(ctx),
            _ => return false,
        }
        true
    }

    fn view_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let Some(view) = &mut self.open else { return Handled::No };
        if view.searching {
            match view.search.handle(key) {
                InputEvent::Changed => view.table_state.select(Some(0)),
                InputEvent::Submit => view.searching = false,
                InputEvent::Cancel => {
                    view.searching = false;
                    view.search.clear();
                }
                _ => {}
            }
            return Handled::Yes;
        }
        let tab = view.tab;
        match key.code {
            KeyCode::Esc if tab == Tab::Tables && !view.search.value.is_empty() => view.search.clear(),
            KeyCode::Esc | KeyCode::Backspace => {
                self.open = None;
                self.queue_metrics();
            }
            KeyCode::Left | KeyCode::Right => {
                let i = TABS.iter().position(|t| *t == tab).unwrap_or(0);
                let next = if key.code == KeyCode::Right { (i + 1) % TABS.len() } else { (i + TABS.len() - 1) % TABS.len() };
                view.tab = TABS[next];
                view.scroll = 0;
            }
            KeyCode::PageUp => view.scroll = view.scroll.saturating_sub(10),
            KeyCode::PageDown => view.scroll = view.scroll.saturating_add(10),
            KeyCode::Up | KeyCode::Char('k') if tab == Tab::Tables => {
                let i = view.table_state.selected().unwrap_or(0);
                view.table_state.select(Some(i.saturating_sub(1)));
            }
            KeyCode::Down | KeyCode::Char('j') if tab == Tab::Tables => {
                let n = view.shown().len();
                let i = view.table_state.selected().unwrap_or(0);
                view.table_state.select(Some((i + 1).min(n.saturating_sub(1))));
            }
            KeyCode::Up | KeyCode::Char('k') => view.scroll = view.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => view.scroll = view.scroll.saturating_add(1),
            KeyCode::Char(' ') if tab == Tab::Tables => {
                if let Some(t) = view.selected_table() {
                    let key = (t.schema_name.clone(), t.table_name.clone());
                    if !view.marked.remove(&key) {
                        view.marked.insert(key);
                    }
                    let n = view.shown().len();
                    let i = view.table_state.selected().unwrap_or(0);
                    view.table_state.select(Some((i + 1).min(n.saturating_sub(1))));
                }
            }
            KeyCode::Char('E') if tab == Tab::Tables => {
                let errored = view.errored();
                if errored.is_empty() {
                    ctx.toast(Level::Info, "No table is in error");
                } else {
                    let n = errored.len();
                    view.marked.extend(errored);
                    ctx.toast(Level::Info, format!("Marked {n} tables in error · R reloads them"));
                }
            }
            KeyCode::Char('f') if tab == Tab::Tables => {
                view.table_filter = view.table_filter.next();
                view.table_state.select(Some(0));
            }
            KeyCode::Char('/') if tab == Tab::Tables => view.searching = true,
            KeyCode::Char('r') => {
                let arn = view.arn.clone();
                self.metrics.retain(|(k, _), _| *k != arn);
                self.load_full(arn, ctx);
                self.load_tables(ctx);
                self.queue_metrics();
                self.listed_at = None;
            }
            _ => {
                if !self.action_key(key, ctx) {
                    return Handled::No;
                }
            }
        }
        Handled::Yes
    }

    fn list_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let selected = self.states[self.page_index()].selected().unwrap_or(0);
        if self.filtering {
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
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::Left => self.switch_page(false),
            KeyCode::Right => self.switch_page(true),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Enter => match self.page {
                Page::Tasks => {
                    if let Some(arn) = self.selected_task().map(|t| t.arn.clone()) {
                        self.open_view(arn, ctx);
                    }
                }
                Page::Instances => {
                    if let Some(id) = self.selected_instance().map(|i| i.id.clone()) {
                        self.show_page(Page::Tasks);
                        self.filter = TextInput::with_value(id);
                        self.refilter();
                    }
                }
                Page::Endpoints => self.open_test(ctx),
            },
            KeyCode::Char('t') if self.page == Page::Endpoints => self.open_test(ctx),
            KeyCode::Char('r') => {
                self.metrics.clear();
                self.listed_at = None;
                self.load_list(ctx);
                self.load_inventory(ctx);
            }
            KeyCode::Char('s' | 'x' | 'R' | 'c') if self.page != Page::Tasks => return Handled::No,
            _ => {
                if !self.action_key(key, ctx) {
                    return Handled::No;
                }
            }
        }
        Handled::Yes
    }
}

impl Module for Dms {
    fn title(&self) -> &'static str {
        "DMS"
    }

    fn description(&self) -> &'static str {
        "Database Migration Service · tasks, tables, CDC latency, instances"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Dms::new();
        self.env_name = ctx.env().name.clone();
        self.region = ctx.env().region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if !matches!(self.load, LoadState::Loaded) && !self.in_flight {
            self.load_list(ctx);
        }
        if matches!(self.inventory, LoadState::Idle) {
            self.load_inventory(ctx);
        }
        self.queue_metrics();
    }

    /// Opens a task (identifier or ARN), or selects an instance or endpoint by identifier.
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        if !matches!(self.load, LoadState::Loaded) {
            self.pending_open = Some(target.to_string());
            self.load_list(ctx);
            if matches!(self.inventory, LoadState::Idle) {
                self.load_inventory(ctx);
            }
            return;
        }
        self.dialog = None;
        self.filtering = false;
        if let Some(arn) = self.tasks.iter().find(|t| t.id == target || t.arn == target).map(|t| t.arn.clone()) {
            self.show_page(Page::Tasks);
            if let Some(pos) = self.visible.iter().position(|&i| self.tasks[i].arn == arn) {
                self.select(pos);
            }
            self.open_view(arn, ctx);
            return;
        }
        self.open = None;
        if let Some(index) = self.instances.iter().position(|i| i.id == target || i.arn == target) {
            self.show_page(Page::Instances);
            if let Some(pos) = self.visible.iter().position(|&i| i == index) {
                self.select(pos);
            }
            return;
        }
        if let Some(index) = self.endpoints.iter().position(|e| e.id == target || e.arn == target) {
            self.show_page(Page::Endpoints);
            if let Some(pos) = self.visible.iter().position(|&i| i == index) {
                self.select(pos);
            }
            return;
        }
        if !matches!(self.inventory, LoadState::Loaded | LoadState::Failed(_)) {
            // Maybe an endpoint: they come with the inventory.
            self.pending_open = Some(target.to_string());
            if matches!(self.inventory, LoadState::Idle) {
                self.load_inventory(ctx);
            }
            return;
        }
        ctx.toast(Level::Warn, format!("Not found in DMS here: {target}"));
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() {
            self.dialog_key(key, ctx);
            return Handled::Yes;
        }
        if self.open.is_some() {
            return self.view_key(key, ctx);
        }
        self.list_key(key, ctx)
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() || !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        if let Some(view) = &mut self.open {
            if view.tab != Tab::Tables {
                return Handled::No;
            }
            let n = view.shown().len();
            let i = view.table_state.selected().unwrap_or(0);
            let index = match event.kind {
                MouseEventKind::ScrollUp => i.saturating_sub(1),
                MouseEventKind::ScrollDown => i + 1,
                MouseEventKind::Down(MouseButton::Left) => view.table_state.offset() + event.row.saturating_sub(self.list_area.y + 2) as usize,
                _ => return Handled::No,
            };
            view.table_state.select(Some(index.min(n.saturating_sub(1))));
            return Handled::Yes;
        }
        let selected = self.states[self.page_index()].selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = event.row.saturating_sub(self.list_area.y + 1) as usize;
                let offset = self.states[self.page_index()].offset();
                self.select(offset + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Listed(result) => self.on_listed(result, ctx),
            Msg::Inventory(result) => {
                let keep = (self.page == Page::Endpoints).then(|| self.selected_key()).flatten();
                match result {
                    Ok((endpoints, connections)) => {
                        self.endpoints = endpoints;
                        self.connections = connections;
                        self.inventory = LoadState::Loaded;
                    }
                    Err(e) => self.inventory = LoadState::Failed(e),
                }
                if self.page == Page::Endpoints {
                    self.reselect(keep);
                }
                if let Some(target) = self.pending_open.take() {
                    self.open(&target, ctx);
                }
            }
            Msg::Full { arn, result } => {
                let fetch_errors = result.is_ok();
                if let Some(view) = self.open.as_mut().filter(|v| v.arn == arn) {
                    // A failed refresh keeps what was read before.
                    if result.is_ok() || !matches!(view.full, Some(Ok(_))) {
                        view.full = Some(*result);
                    }
                    if fetch_errors && view.errors.is_none() {
                        self.load_log_errors(ctx);
                    }
                }
            }
            Msg::Tables { arn, result } => {
                if let Some(view) = self.open.as_mut().filter(|v| v.arn == arn) {
                    view.tables_in_flight = false;
                    if result.is_ok() || !matches!(view.tables, Some(Ok(_))) {
                        // The selected table stays selected wherever the new numbers put it;
                        // when it is gone nothing is, so Space or a reload cannot take another.
                        let keep = view.selected_table().map(|t| (t.schema_name.clone(), t.table_name.clone()));
                        view.tables = Some(result);
                        let at = keep.map(|(schema, table)| {
                            view.shown().iter().position(|t| t.schema_name == schema && t.table_name == table)
                        });
                        match at {
                            Some(index) => view.table_state.select(index),
                            None if view.table_state.selected().is_none() => view.table_state.select(Some(0)),
                            None => {}
                        }
                    }
                }
            }
            Msg::LogErrors { arn, result } => {
                if let Some(view) = self.open.as_mut().filter(|v| v.arn == arn) {
                    view.errors = Some(result);
                }
            }
            Msg::Metrics { key, range, result } => {
                self.metrics_in_flight.remove(&(key.clone(), range));
                self.metrics.insert((key, range), (Instant::now(), *result));
            }
            Msg::Classes { instance, result } => self.on_classes(instance, result, ctx),
            Msg::Acted { result } => self.on_acted(result, ctx),
            Msg::TestStarted { instance, endpoint, result } => self.on_test_started(instance, endpoint, result, ctx),
            Msg::Connections(result) => self.on_connections(result, ctx),
            Msg::Report { id, result } => self.on_report(id, result, ctx),
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if self.refresh_due(active) {
            self.load_list(ctx);
        }
        if !active {
            return;
        }
        if let Some((key, at)) = &self.pending_metrics
            && at.elapsed() >= DEBOUNCE
        {
            let key = key.clone();
            self.pending_metrics = None;
            if self.metrics_key().as_ref() == Some(&key) {
                self.fetch_metrics(key, ctx);
            }
        }
        if let Some(key) = self.metrics_key()
            && self.metrics.get(&(key.clone(), self.range)).is_some_and(|(at, _)| at.elapsed() >= DETAIL_REFRESH)
            && !self.metrics_in_flight.contains(&(key.clone(), self.range))
            && self.dialog.is_none()
        {
            self.fetch_metrics(key, ctx);
        }
        if self.open.as_ref().is_some_and(|v| !v.tables_in_flight && v.tables_at.elapsed() >= DETAIL_REFRESH) {
            self.load_tables(ctx);
        }
        if !self.testing.is_empty() && self.tests_polled.elapsed() >= TEST_POLL {
            self.tests_polled = Instant::now();
            let aws = ctx.env().aws.clone();
            ctx.spawn(move || Msg::Connections(dms::connections(&aws)));
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if self.open.is_some() {
            self.render_task(f, area, view);
        } else {
            self.render_pages(f, area, view);
        }
        self.render_dialog(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.dialog {
            Some(Dialog::ReloadWhat(..) | Dialog::Classes(..) | Dialog::TestFrom(..)) => return vec![("↑↓", "move"), ("Enter", "choose"), ("Esc", "cancel")],
            Some(Dialog::ReloadTables { .. } | Dialog::Scale { .. }) => return vec![("Tab", "next field"), ("←→", "choose"), ("Enter", "confirm"), ("Esc", "cancel")],
            Some(Dialog::Confirm { .. }) => return vec![("Enter", "confirm"), ("Esc", "cancel")],
            None => {}
        }
        if let Some(v) = &self.open {
            if v.searching {
                return vec![("Enter", "keep search"), ("Esc", "clear")];
            }
            let mut hints = vec![("←→", "tab"), ("Esc", "tasks")];
            if v.tab == Tab::Tables {
                hints.extend([("Space", "mark"), ("E", "mark errors"), ("f", "filter"), ("/", "search")]);
            } else {
                hints.push(("PgUp/PgDn", "scroll"));
            }
            hints.extend([("s", "resume"), ("x", "stop"), ("R", "reload"), ("S", "scale"), ("c", "copy for AI"), ("l", "logs"), ("m", "range"), ("r", "refresh")]);
            return hints;
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        let mut hints = vec![("↑↓", "move"), ("←→", "page")];
        match self.page {
            Page::Tasks => hints.extend([("Enter", "open"), ("s", "resume"), ("x", "stop"), ("R", "reload"), ("S", "scale"), ("c", "copy for AI"), ("l", "logs")]),
            Page::Instances => hints.extend([("Enter", "its tasks"), ("S", "scale"), ("l", "logs")]),
            Page::Endpoints => hints.push(("t", "test connection")),
        }
        hints.extend([("/", "search"), ("m", "range"), ("y", "copy ARN"), ("o", "console"), ("r", "refresh")]);
        hints
    }

    fn badge(&self) -> Option<(String, Color)> {
        let n = self.tasks.iter().filter(|t| t.in_trouble()).count();
        (n > 0).then(|| (format!("✗ {n}"), theme::ERR))
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || !self.metrics_in_flight.is_empty()
            || self.pending_metrics.is_some()
            || !self.testing.is_empty()
            || self.open.as_ref().is_some_and(|v| v.tables_in_flight || v.full.is_none())
            || self.tasks.iter().any(|t| matches!(t.health(), dms::Health::Changing))
            || matches!(
                self.dialog,
                Some(Dialog::Confirm { busy: true, .. } | Dialog::ReloadTables { busy: true, .. } | Dialog::Scale { busy: true, .. })
            )
    }

    fn capturing(&self) -> bool {
        self.filtering || self.dialog.is_some() || self.open.as_ref().is_some_and(|v| v.searching)
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.dialog {
            Some(Dialog::ReloadWhat(p, ..) | Dialog::Classes(p, ..) | Dialog::TestFrom(p, ..)) => p.paste(text),
            Some(Dialog::ReloadTables { form, .. } | Dialog::Scale { form, .. }) => form.paste(text),
            Some(Dialog::Confirm { typed: Some((_, input)), .. }) => input.insert_str(text, false),
            Some(_) => {}
            None => {
                if let Some(v) = self.open.as_mut().filter(|v| v.searching) {
                    v.search.insert_str(text, false);
                } else if self.filtering {
                    self.filter.insert_str(text, false);
                    self.refilter();
                }
            }
        }
    }
}

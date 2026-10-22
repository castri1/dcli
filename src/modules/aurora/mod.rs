//! Aurora: clusters with their health; a cluster's topology (its endpoints and the nodes each
//! one serves, the nodes by zone with live CPU, connections and replica lag), each node's
//! metrics, its database load (top SQL and wait events, from Performance Insights), the
//! cluster's events and pending maintenance, and its configuration. Copies endpoints and
//! ready connection commands, and writes a performance diagnosis for an AI agent.
//!
//! Changes, each explained and confirmed first (the environment's name typed in
//! production): fail over (to the selected reader), reboot a node, stop and start the
//! cluster, change a node's class (or a Serverless v2 cluster's ACU range), take a manual
//! snapshot. The cluster is then read again until it settles, so the change shows as it
//! happens.

mod topology;
mod views;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::lambda::Range;
use crate::aws::pi;
use crate::aws::rds::{self, Action, Cluster, Endpoint, Event, Instance, Inventory, Maintenance, Metrics};
use crate::aws;
use crate::config::is_production;
use crate::tui::chart::{Agg, columns, fmt_ms, sparkline_to};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, now_secs};
use topology::{Topology, health_color};

/// How often the live figures (CPU, connections, lag) are read again while shown.
const PULSE_EVERY: Duration = Duration::from_secs(60);
/// Clusters are listed again after this when the module is opened again.
const LIST_FRESH: Duration = Duration::from_secs(300);
/// …and this often while a cluster or instance is changing (modifying, rebooting…).
const CHANGING_EVERY: Duration = Duration::from_secs(10);
/// After a change, how long the list is read again even before AWS shows it under way.
const WATCH_AFTER_CHANGE: Duration = Duration::from_secs(90);
/// Metrics and Performance Insights older than this are read again when shown.
const DATA_FRESH: Duration = Duration::from_secs(120);
const EVENTS_FRESH: Duration = Duration::from_secs(300);
/// Width of the details beside the topology diagram.
const SIDE_W: u16 = 48;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Topology,
    Performance,
    Sql,
    Events,
    Config,
}

const TABS: [Tab; 5] = [Tab::Topology, Tab::Performance, Tab::Sql, Tab::Events, Tab::Config];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Topology => "Topology",
            Tab::Performance => "Performance",
            Tab::Sql => "Top SQL",
            Tab::Events => "Events",
            Tab::Config => "Configuration",
        }
    }
}

/// A change being prepared or confirmed.
enum Dialog {
    /// Reading the classes AWS offers for the instance.
    Classes { instance: String },
    /// Choosing a class: the picker, the instance, the classes by item.
    Class(Picker, String, Vec<String>),
    /// What a change does, with its confirmation; the form holds what can still be edited
    /// (a snapshot's name, an ACU range, when a class change applies).
    Confirm { action: Action, form: Form, busy: bool },
}

const APPLY: [&str; 2] = ["now", "in the maintenance window"];

enum Msg {
    Inventory(aws::Result<Inventory>),
    Pulse(aws::Result<Metrics>),
    Metrics { cluster: String, range: Range, result: aws::Result<Metrics> },
    Insights { instance: String, range: Range, data: Box<views::Insights> },
    Events(aws::Result<Vec<Event>>),
    Maintenance(aws::Result<Vec<Maintenance>>),
    Report { cluster: String, result: aws::Result<String> },
    Classes { instance: String, result: aws::Result<Vec<String>> },
    Applied { action: Action, result: aws::Result<()> },
}

pub struct Aurora {
    inventory: Inventory,
    load: LoadState,
    loaded_at: Instant,
    filter: TextInput,
    filtering: bool,
    /// Indexes in `inventory.clusters` shown, after the filter.
    rows: Vec<usize>,
    list_state: ListState,
    /// The cluster shown (Enter on the list); `None` shows the list.
    open: Option<String>,
    tab: Tab,
    /// The topology's selection: endpoints first, then nodes.
    pick: usize,
    /// The node the Performance and Top SQL tabs show (index in the cluster's instances).
    node: usize,
    range: Range,
    pulse: Option<(Instant, aws::Result<Metrics>)>,
    metrics: HashMap<(String, Range), (Instant, aws::Result<Metrics>)>,
    insights: HashMap<(String, Range), (Instant, views::Insights)>,
    events: Option<(Instant, aws::Result<Vec<Event>>)>,
    maintenance: Option<aws::Result<Vec<Maintenance>>>,
    in_flight: HashSet<String>,
    /// Top SQL: the statement selected, while the statements have the keys.
    sql_cursor: Option<usize>,
    /// A statement, full screen.
    zoom: Option<(String, LogView)>,
    scroll: u16,
    /// A cluster another module asked to show, opened once the list loads.
    pending_open: Option<String>,
    env_name: String,
    region: String,
    profile: String,
    list_area: Rect,
    /// Where the topology's boxes are on screen, for clicks.
    hits: Vec<(Rect, usize)>,
    dialog: Option<Dialog>,
    /// Until when the list is read again after a change, as if something were changing.
    watch_until: Option<Instant>,
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

impl Aurora {
    pub fn new() -> Aurora {
        Aurora {
            inventory: Inventory::default(),
            load: LoadState::Idle,
            loaded_at: Instant::now(),
            filter: TextInput::default(),
            filtering: false,
            rows: Vec::new(),
            list_state: ListState::default(),
            open: None,
            tab: Tab::Topology,
            pick: 0,
            node: 0,
            range: Range::Day,
            pulse: None,
            metrics: HashMap::new(),
            insights: HashMap::new(),
            events: None,
            maintenance: None,
            in_flight: HashSet::new(),
            sql_cursor: None,
            zoom: None,
            scroll: 0,
            pending_open: None,
            env_name: String::new(),
            region: String::new(),
            profile: String::new(),
            list_area: Rect::default(),
            hits: Vec::new(),
            dialog: None,
            watch_until: None,
        }
    }

    // ---- data ------------------------------------------------------------------------------

    fn load_inventory(&mut self, ctx: &mut Ctx) {
        if !self.in_flight.insert("inventory".into()) {
            return;
        }
        if self.inventory.clusters.is_empty() {
            self.load = LoadState::Loading;
        }
        self.loaded_at = Instant::now();
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Inventory(rds::inventory(&aws, None)));
    }

    fn fetch_pulse(&mut self, ctx: &mut Ctx) {
        if self.inventory.instances.is_empty() || !self.in_flight.insert("pulse".into()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        let instances = self.inventory.instances.clone();
        let clusters: Vec<String> = self.inventory.clusters.iter().map(|c| c.id.clone()).collect();
        ctx.spawn(move || {
            let instances: Vec<&Instance> = instances.iter().collect();
            let clusters: Vec<&str> = clusters.iter().map(String::as_str).collect();
            Msg::Pulse(rds::pulse(&aws, &instances, &clusters, now_secs()))
        });
    }

    /// Reads what the current tab shows, unless it is here and fresh (or `force`).
    fn fetch_tab(&mut self, ctx: &mut Ctx, force: bool) {
        let Some(c) = self.cluster().cloned() else { return };
        let aws = ctx.env().aws.clone();
        let range = self.range;
        match self.tab {
            Tab::Topology | Tab::Config => {}
            Tab::Performance => {
                let key = (c.id.clone(), range);
                let fresh = self.metrics.get(&key).is_some_and(|(at, r)| r.is_ok() && at.elapsed() < DATA_FRESH);
                if (fresh && !force) || !self.in_flight.insert(format!("metrics {} {}", c.id, range.label())) {
                    return;
                }
                let instances: Vec<Instance> = self.instances().into_iter().cloned().collect();
                ctx.spawn(move || {
                    let refs: Vec<&Instance> = instances.iter().collect();
                    Msg::Metrics { result: rds::cluster_metrics(&aws, &c.id, &refs, range, now_secs()), cluster: c.id, range }
                });
            }
            Tab::Sql => {
                let Some(node) = self.node().cloned().filter(|n| n.pi_enabled && !n.resource_id.is_empty()) else { return };
                let key = (node.id.clone(), range);
                let fresh = self.insights.get(&key).is_some_and(|(at, _)| at.elapsed() < DATA_FRESH);
                if (fresh && !force) || !self.in_flight.insert(format!("insights {} {}", node.id, range.label())) {
                    return;
                }
                ctx.spawn(move || {
                    let end = now_secs();
                    let start = end - range.secs();
                    let id = &node.resource_id;
                    let (load, sql, waits) = std::thread::scope(|s| {
                        let load = s.spawn(|| pi::load(&aws, id, start, end, range.period()));
                        let sql = s.spawn(|| pi::top_sql(&aws, id, start, end, 10));
                        let waits = s.spawn(|| pi::top_waits(&aws, id, start, end, 10));
                        (rds::joined(load), rds::joined(sql), rds::joined(waits))
                    });
                    Msg::Insights { instance: node.id, range, data: Box::new(views::Insights { window: (start, end), load, sql, waits }) }
                });
            }
            Tab::Events => {
                let fresh = self.events.as_ref().is_some_and(|(at, r)| r.is_ok() && at.elapsed() < EVENTS_FRESH);
                if (fresh && !force) || !self.in_flight.insert("events".into()) {
                    return;
                }
                let aws2 = aws.clone();
                ctx.spawn(move || Msg::Events(rds::describe_events(&aws, 24 * 7)));
                ctx.spawn(move || Msg::Maintenance(rds::pending_maintenance(&aws2)));
            }
        }
    }

    fn refilter(&mut self) {
        let keep = self.list_cluster().map(|c| c.id.clone());
        let query = self.filter.value.trim().to_lowercase();
        self.rows = (0..self.inventory.clusters.len()).filter(|i| matches_filter(&self.inventory.clusters[*i].id, &query)).collect();
        let index = keep.and_then(|k| self.rows.iter().position(|i| self.inventory.clusters[*i].id == k)).unwrap_or(0);
        self.list_state.select(if self.rows.is_empty() { None } else { Some(index) });
    }

    /// Opens the cluster asked for by another module, once the list has it.
    fn try_open(&mut self, ctx: &mut Ctx) -> bool {
        let Some(target) = self.pending_open.clone() else { return false };
        let Some(i) = self.inventory.clusters.iter().position(|c| c.id == target || c.arn == target) else { return false };
        if !self.rows.contains(&i) {
            self.filter.clear();
            self.filtering = false;
            self.refilter();
        }
        if let Some(pos) = self.rows.iter().position(|r| *r == i) {
            self.list_state.select(Some(pos));
        }
        self.pending_open = None;
        self.open_cluster(ctx);
        true
    }

    // ---- selection ---------------------------------------------------------------------------

    /// The cluster under the list's cursor.
    fn list_cluster(&self) -> Option<&Cluster> {
        self.inventory.clusters.get(*self.rows.get(self.list_state.selected()?)?)
    }

    /// The cluster open.
    fn cluster(&self) -> Option<&Cluster> {
        self.inventory.cluster(self.open.as_deref()?)
    }

    fn instances(&self) -> Vec<&Instance> {
        self.open.as_deref().map(|c| self.inventory.instances_of(c)).unwrap_or_default()
    }

    fn endpoints(&self) -> Vec<&Endpoint> {
        self.open.as_deref().map(|c| self.inventory.endpoints_of(c)).unwrap_or_default()
    }

    fn node(&self) -> Option<&Instance> {
        let instances = self.instances();
        instances.get(self.node.min(instances.len().saturating_sub(1))).copied()
    }

    fn pulse(&self) -> Option<&Metrics> {
        self.pulse.as_ref().and_then(|(_, r)| r.as_ref().ok())
    }

    fn open_cluster(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.list_cluster().map(|c| c.id.clone()) else { return };
        self.open = Some(c);
        self.tab = Tab::Topology;
        self.node = 0;
        // The writer, below the endpoints.
        self.pick = self.endpoints().len();
        self.scroll = 0;
        self.sql_cursor = None;
        self.fetch_tab(ctx, false);
    }

    fn set_tab(&mut self, tab: Tab, ctx: &mut Ctx) {
        self.tab = tab;
        self.scroll = 0;
        self.sql_cursor = None;
        self.fetch_tab(ctx, false);
    }

    fn step_tab(&mut self, forward: bool, ctx: &mut Ctx) {
        let at = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
        let next = if forward { (at + 1) % TABS.len() } else { (at + TABS.len() - 1) % TABS.len() };
        self.set_tab(TABS[next], ctx);
    }

    /// Moves the topology's selection; a node selected is also the other tabs' node.
    fn move_pick(&mut self, delta: isize) {
        let items = self.endpoints().len() + self.instances().len();
        if items == 0 {
            return;
        }
        self.pick = (self.pick as isize + delta).clamp(0, items as isize - 1) as usize;
        if let Some(n) = self.pick.checked_sub(self.endpoints().len()) {
            self.node = n;
        }
    }

    fn move_node(&mut self, delta: isize, ctx: &mut Ctx) {
        let count = self.instances().len();
        if count == 0 {
            return;
        }
        self.node = (self.node as isize + delta).rem_euclid(count as isize) as usize;
        self.pick = self.endpoints().len() + self.node;
        self.sql_cursor = None;
        self.scroll = 0;
        self.fetch_tab(ctx, false);
    }

    fn statements(&self) -> usize {
        let Some(node) = self.node() else { return 0 };
        match self.insights.get(&(node.id.clone(), self.range)) {
            Some((_, d)) => d.sql.as_ref().map_or(0, Vec::len),
            None => 0,
        }
    }

    fn statement(&self, n: usize) -> Option<String> {
        let node = self.node()?;
        let (_, d) = self.insights.get(&(node.id.clone(), self.range))?;
        Some(d.sql.as_ref().ok()?.get(n)?.label.clone())
    }

    /// The host `y` and `Y` are about: the selected endpoint or node in the topology, the
    /// node on the other tabs, the writer endpoint on the list.
    fn selected_host(&self) -> Option<(String, String)> {
        if self.open.is_none() {
            let c = self.list_cluster()?;
            return Some((c.endpoint.clone()?, "the writer endpoint".into()));
        }
        let endpoints = self.endpoints();
        if self.tab == Tab::Topology
            && let Some(e) = endpoints.get(self.pick)
        {
            return Some((e.address.clone(), format!("the {} endpoint", e.name)));
        }
        let node = self.node()?;
        Some((node.address.clone()?, format!("the endpoint of {}", node.id)))
    }

    // ---- actions -----------------------------------------------------------------------------

    fn copy_host(&mut self, ctx: &mut Ctx) {
        if self.tab == Tab::Sql
            && self.open.is_some()
            && let Some(sql) = self.sql_cursor.and_then(|n| self.statement(n))
        {
            return copy(ctx, "statement", &sql, "the statement");
        }
        match self.selected_host() {
            Some((host, what)) => copy(ctx, "endpoint", &host, &what),
            None => ctx.toast(Level::Info, "No endpoint to copy"),
        }
    }

    fn copy_connect(&mut self, ctx: &mut Ctx) {
        let cluster = if self.open.is_some() { self.cluster() } else { self.list_cluster() };
        let (Some(c), Some((host, _))) = (cluster.cloned(), self.selected_host()) else {
            return ctx.toast(Level::Info, "No endpoint to connect to");
        };
        let command = rds::connect_command(&c, &host, &self.region, &self.profile);
        let how = if c.iam_auth { "the connection command (IAM token at run time)" } else { "the connection command (asks for the password)" };
        copy(ctx, "connect", &command, how);
    }

    fn copy_report(&mut self, ctx: &mut Ctx) {
        let cluster = if self.open.is_some() { self.cluster() } else { self.list_cluster() };
        let Some(id) = cluster.map(|c| c.id.clone()) else { return };
        if !self.in_flight.insert(format!("report {id}")) {
            return;
        }
        ctx.toast(Level::Info, format!("Writing the performance diagnosis of {id} (last {})…", self.range.label()));
        let (aws, env, region, range) = (ctx.env().aws.clone(), self.env_name.clone(), self.region.clone(), self.range);
        ctx.spawn(move || Msg::Report { result: rds::gather_report(&aws, &env, &region, &id, range, now_secs()), cluster: id });
    }

    fn open_console(&self, ctx: &mut Ctx) {
        let cluster = if self.open.is_some() { self.cluster() } else { self.list_cluster() };
        if let Some(c) = cluster {
            let url = rds::console_url(&self.region, &c.id);
            if !crate::platform::open_url(&url) {
                ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
            }
        }
    }

    fn zoom_statement(&mut self) {
        let (Some(n), Some(node)) = (self.sql_cursor, self.node().map(|n| n.id.clone())) else { return };
        let Some(sql) = self.statement(n) else { return };
        let mut view = LogView::default();
        view.extend(sql.lines().map(str::to_string));
        view.zoomed = true;
        view.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        self.zoom = Some((format!("{node} · statement {}", n + 1), view));
    }

    // ---- changes -----------------------------------------------------------------------------

    /// The cluster a cluster change is about: the one open, or the one under the list's cursor.
    fn target_cluster(&self) -> Option<Cluster> {
        if self.open.is_some() { self.cluster() } else { self.list_cluster() }.cloned()
    }

    /// The node a node change is about: the node selected in the topology (not an endpoint),
    /// or the node the other tabs show.
    fn target_node(&self, ctx: &mut Ctx) -> Option<Instance> {
        if self.open.is_none() {
            ctx.toast(Level::Info, "Open the cluster and select a node first");
            return None;
        }
        if self.tab == Tab::Topology && self.pick < self.endpoints().len() {
            ctx.toast(Level::Info, "Select a node (↑↓) first");
            return None;
        }
        self.node().cloned()
    }

    fn start_failover(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.target_cluster() else { return };
        // The reader selected in the topology becomes the writer; otherwise AWS picks one.
        let to = (self.open.is_some() && self.tab == Tab::Topology && self.pick >= self.endpoints().len())
            .then(|| self.node().filter(|n| !n.writer).map(|n| n.id.clone()))
            .flatten();
        self.ask(Action::Failover { cluster: c.id, to }, ctx);
    }

    fn start_reboot(&mut self, ctx: &mut Ctx) {
        if let Some(node) = self.target_node(ctx) {
            self.ask(Action::Reboot { instance: node.id }, ctx);
        }
    }

    fn start_stop(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.target_cluster() else { return };
        let action = if c.status == "stopped" { Action::Start { cluster: c.id } } else { Action::Stop { cluster: c.id } };
        self.ask(action, ctx);
    }

    fn start_snapshot(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.target_cluster() else { return };
        let id = rds::snapshot_name(&c.id, now_secs());
        self.ask(Action::Snapshot { cluster: c.id, id }, ctx);
    }

    /// A Serverless v2 node changes its cluster's ACU range; any other, its class (picked
    /// among those AWS offers for its engine version).
    fn start_resize(&mut self, ctx: &mut Ctx) {
        let Some(node) = self.target_node(ctx) else { return };
        if node.serverless() {
            let Some(c) = self.cluster().cloned() else { return };
            let range = c.serverless.clone().unwrap_or(rds::Capacity { min: 0.5, max: 16.0 });
            return self.ask(Action::SetCapacity { cluster: c.id, min: range.min, max: range.max }, ctx);
        }
        if !self.in_flight.insert(format!("classes {}", node.id)) {
            return;
        }
        self.dialog = Some(Dialog::Classes { instance: node.id.clone() });
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Classes { result: rds::orderable_classes(&aws, &node.engine, &node.engine_version), instance: node.id });
    }

    fn on_classes(&mut self, instance: String, result: aws::Result<Vec<String>>, ctx: &mut Ctx) {
        self.in_flight.remove(&format!("classes {instance}"));
        if !matches!(&self.dialog, Some(Dialog::Classes { instance: i }) if *i == instance) {
            return;
        }
        let Some(node) = self.inventory.instances.iter().find(|i| i.id == instance).cloned() else {
            self.dialog = None;
            return;
        };
        let has_range = self.inventory.cluster(&node.cluster).is_some_and(|c| c.serverless.is_some());
        let classes: Vec<String> = match result {
            Ok(classes) => classes.into_iter().filter(|c| c != "db.serverless" || has_range).collect(),
            Err(e) => {
                self.dialog = None;
                return ctx.toast(Level::Error, format!("Could not read the classes AWS offers: {}", e.short()));
            }
        };
        if classes.is_empty() {
            self.dialog = None;
            return ctx.toast(Level::Warn, format!("AWS offers no class for {} {}", node.engine, node.engine_version));
        }
        let items = classes
            .iter()
            .map(|c| {
                let mut detail = rds::class_detail(c);
                if node.pending_class.as_deref() == Some(c.as_str()) {
                    detail.push_str(" · pending");
                }
                Item::new(c.as_str(), detail).current(*c == node.class)
            })
            .collect();
        let cursor = classes.iter().position(|c| *c == node.class).unwrap_or(0);
        let title = format!("New class for {} (now {})", node.id, node.class);
        self.dialog = Some(Dialog::Class(Picker::new(title, "choose", items, false).with_cursor(cursor), instance, classes));
    }

    /// Explains `action` and asks for it, unless the cluster cannot take it as it is now.
    fn ask(&mut self, action: Action, ctx: &mut Ctx) {
        let Some(cluster) = action.cluster(&self.inventory.instances).map(str::to_string) else { return };
        let Some(c) = self.inventory.cluster(&cluster).cloned() else { return };
        let instances = self.inventory.instances_of(&cluster);
        // What the form then edits (a name, a range) is checked again on submit.
        if let Some(why) = rds::refusal(&action, &c, &instances) {
            self.dialog = None;
            return ctx.toast(Level::Warn, why);
        }
        let form = action_form(&action, &c, &instances, &self.env_name);
        self.dialog = Some(Dialog::Confirm { action, form, busy: false });
    }

    fn submit_change(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Confirm { action, form, busy }) = &mut self.dialog else { return };
        let action = match with_form(action, form) {
            Ok(a) => a,
            Err(e) => return form.fail(e),
        };
        let Some(c) = action.cluster(&self.inventory.instances).and_then(|id| self.inventory.cluster(id)) else { return };
        if let Some(why) = rds::refusal(&action, c, &self.inventory.instances_of(&c.id)) {
            return form.fail(why);
        }
        *busy = true;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Applied { result: rds::apply(&aws, &action), action });
    }

    fn on_applied(&mut self, action: Action, result: aws::Result<()>, ctx: &mut Ctx) {
        match result {
            Ok(()) => {
                self.dialog = None;
                ctx.toast(Level::Success, action.accepted());
                self.watch_until = Some(Instant::now() + WATCH_AFTER_CHANGE);
                // The change shows in the events too.
                self.events = None;
                self.load_inventory(ctx);
                self.fetch_tab(ctx, true);
            }
            Err(e) => match &mut self.dialog {
                Some(Dialog::Confirm { form, busy, .. }) => {
                    *busy = false;
                    form.fail(e.short());
                }
                _ => ctx.toast(Level::Error, e.short()),
            },
        }
    }

    fn dialog_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(dialog) = &mut self.dialog else { return };
        match dialog {
            Dialog::Classes { .. } => {
                if key.code == KeyCode::Esc {
                    self.dialog = None;
                }
            }
            Dialog::Class(picker, instance, classes) => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.dialog = None,
                Outcome::Done(chosen) => {
                    let Some(class) = chosen.first().and_then(|&i| classes.get(i)).cloned() else { return };
                    let instance = instance.clone();
                    // A writer restarting stops writes: the maintenance window is offered first.
                    let writer = self.inventory.instances.iter().any(|i| i.id == instance && i.writer);
                    self.ask(Action::SetClass { instance, class, now: !writer }, ctx);
                }
            },
            Dialog::Confirm { busy: true, .. } => {}
            Dialog::Confirm { action, form, .. } => match form.handle(key) {
                FormEvent::Submit => self.submit_change(ctx),
                FormEvent::Cancel => self.dialog = None,
                FormEvent::Pending => {
                    // What a class change does depends on when it applies.
                    if let (Action::SetClass { .. }, Ok(edited)) = (&*action, with_form(action, form)) {
                        let cluster = edited.cluster(&self.inventory.instances).and_then(|id| self.inventory.cluster(id));
                        if let Some(c) = cluster {
                            form.note = Some(rds::explain(&edited, c, &self.inventory.instances_of(&c.id)));
                        }
                    }
                }
            },
        }
    }

    fn render_dialog(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let color = if is_production(&self.env_name) { theme::ERR } else { theme::ACCENT };
        match &mut self.dialog {
            None => {}
            Some(Dialog::Classes { instance }) => {
                let body = Text::from(vec![
                    Line::raw(""),
                    Line::from(vec![
                        Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()),
                        Span::raw(format!("Reading the classes AWS offers for {instance}…")),
                    ]),
                ]);
                widgets::popup(f, widgets::popup_area(area, 64, 5), "Instance class", theme::ACCENT, body);
            }
            Some(Dialog::Class(picker, ..)) => {
                let rect = widgets::popup_area(area, 76, picker.height(area.height.saturating_sub(4)));
                picker.render(f, rect, color);
            }
            Some(Dialog::Confirm { form, busy, .. }) => {
                form.render(f, area, color);
                if *busy {
                    let body = Text::from(vec![
                        Line::raw(""),
                        Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw("Asking AWS…")]),
                    ]);
                    widgets::popup(f, widgets::popup_area(area, 34, 5), "Aurora", theme::ACCENT, body);
                }
            }
        }
    }

    // ---- rendering ---------------------------------------------------------------------------

    fn cluster_health(&self, c: &Cluster) -> rds::Health {
        let pulse = self.pulse();
        self.inventory
            .instances_of(&c.id)
            .iter()
            .map(|i| rds::node_health(&i.status, pulse.and_then(|p| p.last(&i.id, "cpu")), pulse.and_then(|p| p.max(&i.id, "lag"))))
            .max()
            .unwrap_or(rds::Health::Ok)
            .max(rds::status_health(&c.status))
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search clusters");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = match &self.load {
            LoadState::Loading => format!(" {} Aurora clusters ", theme::spinner(view.tick)),
            _ => format!(" Aurora clusters · {} ", self.rows.len()),
        };
        if self.rows.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading | LoadState::Idle => Line::styled("Loading clusters…", theme::muted()),
                LoadState::Loaded if !self.filter.value.is_empty() => Line::styled("No cluster matches", theme::muted()),
                LoadState::Loaded => Line::styled(format!("No Aurora clusters in {}", self.region), theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let shape = |c: &Cluster| {
            let readers = self.inventory.instances_of(&c.id).iter().filter(|i| !i.writer).count();
            format!("{} · {readers} reader{}", c.capacity_label(), if readers == 1 { "" } else { "s" })
        };
        let name_w = self.inventory.clusters.iter().map(|c| c.id.chars().count()).max().unwrap_or(10).clamp(10, 32);
        let engine_w = self.inventory.clusters.iter().map(|c| c.engine_label().chars().count()).max().unwrap_or(10).min(40);
        let shape_w = self.inventory.clusters.iter().map(|c| shape(c).chars().count()).max().unwrap_or(10);
        // CPU and lag first; the engine goes when the row would not fit.
        let with_engine = width >= 2 + name_w + 2 + engine_w + 2 + shape_w + 19 + 12;
        let pulse = self.pulse();
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|&i| {
                let c = &self.inventory.clusters[i];
                let instances = self.inventory.instances_of(&c.id);
                let health = self.cluster_health(c);
                let mut spans = vec![
                    Span::styled("● ", Style::new().fg(health_color(health))),
                    Span::styled(format!("{:<name_w$}", ellipsize(&c.id, name_w)), Style::new().bold()),
                ];
                if with_engine {
                    spans.push(Span::styled(format!("  {:<engine_w$}", ellipsize(&c.engine_label(), engine_w)), theme::muted()));
                }
                spans.push(Span::raw(format!("  {:<shape_w$}", shape(c))));
                if let (Some(p), Some(w)) = (pulse, c.writer()) {
                    let cpu = p.last(w, "cpu");
                    let spark = sparkline_to(&columns(p.points(w, "cpu"), p.window, 8, Agg::Mean), 100.0);
                    let color = health_color(rds::node_health("available", cpu, None));
                    spans.push(Span::styled("  CPU ", theme::muted()));
                    spans.push(Span::styled(format!("{spark} {:>4}", cpu.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into())), Style::new().fg(color)));
                    let lag = instances.iter().filter(|i| !i.writer).filter_map(|i| p.max(&i.id, "lag")).reduce(f64::max);
                    if let Some(lag) = lag {
                        spans.push(Span::styled("  lag ", theme::muted()));
                        spans.push(Span::styled(fmt_ms(lag), Style::new().fg(health_color(rds::node_health("available", None, Some(lag))))));
                    }
                }
                if c.status != "available" {
                    spans.push(Span::styled(format!("  {}", c.status), Style::new().fg(health_color(rds::status_health(&c.status)))));
                }
                if let Some((g, primary)) = &c.global {
                    spans.push(Span::styled(format!("  global {g}{}", if *primary { " (primary)" } else { "" }), theme::muted()));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        // The selected cluster's endpoints and nodes below the list, when there is room.
        let list_h = (self.rows.len() as u16 + 2).max(5);
        let preview = self.list_cluster().map(|c| c.id.clone()).filter(|_| list_area.height >= list_h + 8);
        let [list_area, preview_area] = match preview {
            Some(_) => list_area.layout(&Layout::vertical([Constraint::Length(list_h), Constraint::Fill(1)])),
            None => [list_area, Rect::default()],
        };
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
        if let Some(id) = preview {
            let (Some(c), instances, endpoints) = (self.inventory.cluster(&id), self.inventory.instances_of(&id), self.inventory.endpoints_of(&id)) else {
                return;
            };
            let t = Topology { cluster: c, instances: &instances, endpoints: &endpoints, pulse: self.pulse(), selected: usize::MAX, volume: None };
            let block = widgets::panel(Line::from(vec![Span::raw(format!(" {id} ")), Span::styled("· Enter opens ", theme::muted())]), false);
            let inner = block.inner(preview_area);
            f.render_widget(block, preview_area);
            let (lines, _) = topology::list(&t, inner.width as usize);
            f.render_widget(Paragraph::new(Text::from(lines)), inner);
        }
    }

    fn tabs_line(&self, node: Option<&Instance>) -> Line<'static> {
        let mut spans = Vec::new();
        for tab in TABS {
            let extra = match tab {
                Tab::Sql if node.is_some_and(|n| !n.pi_enabled) => " off",
                _ => "",
            };
            let style = if tab == self.tab { Style::new().fg(Color::Black).bg(theme::ACCENT).bold() } else { theme::muted() };
            spans.push(Span::styled(format!(" {}{extra} ", tab.label()), style));
            spans.push(Span::raw(" "));
        }
        Line::from(spans)
    }

    fn render_cluster(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.hits.clear();
        let Some(c) = self.cluster().cloned() else { return };
        let health = self.cluster_health(&c);
        let block = widgets::panel(Line::from(format!(" {} ", c.id)).bold(), true)
            .title_top(Line::styled(format!(" ● {} ", c.status), Style::new().fg(health_color(health)).bold()).right_aligned());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [head, tabs, body] = inner.layout(&Layout::vertical([Constraint::Length(1), Constraint::Length(2), Constraint::Fill(1)]));
        let mut about = format!("{} · {} · {} · {} {}", c.engine_label(), c.capacity_label(), c.storage_label(), self.env_name, self.region);
        if let Some((g, primary)) = &c.global {
            about.push_str(&format!(" · global {g} ({})", if *primary { "primary" } else { "secondary" }));
        }
        f.render_widget(Paragraph::new(Line::styled(ellipsize(&about, head.width as usize), theme::muted())), head);
        let node = self.node().cloned();
        f.render_widget(Paragraph::new(self.tabs_line(node.as_ref())), tabs);
        match self.tab {
            Tab::Topology => self.render_topology(f, body, &c),
            Tab::Performance | Tab::Sql => {
                let Some(node) = node else {
                    f.render_widget(widgets::placeholder(Line::styled("The cluster has no instances", theme::muted())), body);
                    return;
                };
                let [bar, rest] = body.layout(&Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]));
                let count = self.instances().len();
                f.render_widget(Paragraph::new(views::node_bar(&node, self.node, count, Some(self.range))), bar);
                let width = rest.width as usize;
                let (lines, rows) = if self.tab == Tab::Performance {
                    (views::performance(&c, &node, self.metrics.get(&(c.id.clone(), self.range)).map(|(_, r)| r), width, view.tick), Vec::new())
                } else {
                    let data = self.insights.get(&(node.id.clone(), self.range)).map(|(_, d)| d);
                    views::insights(&node, &self.region, data, self.sql_cursor, width, view.tick)
                };
                if let Some(&at) = self.sql_cursor.and_then(|n| rows.get(n)) {
                    self.keep_visible(at as u16, rest.height);
                }
                f.render_widget(Paragraph::new(Text::from(lines)).scroll((self.scroll, 0)), rest);
            }
            Tab::Events => {
                let instances = self.instances();
                let lines = views::events(
                    &c,
                    &instances,
                    self.events.as_ref().map(|(_, r)| r),
                    self.maintenance.as_ref(),
                    body.width as usize,
                    view.now,
                    view.tick,
                );
                f.render_widget(Paragraph::new(Text::from(lines)).scroll((self.scroll, 0)), body);
            }
            Tab::Config => {
                let lines = views::config(&c, &self.instances());
                f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.scroll, 0)), body);
            }
        }
    }

    fn keep_visible(&mut self, at: u16, height: u16) {
        let height = height.max(1);
        if at < self.scroll {
            self.scroll = at;
        } else if at >= self.scroll + height {
            self.scroll = at + 1 - height;
        }
    }

    fn render_topology(&mut self, f: &mut Frame, body: Rect, c: &Cluster) {
        let instances = self.instances();
        let endpoints = self.endpoints();
        let pulse = self.pulse.as_ref().and_then(|(_, r)| r.as_ref().ok());
        let t = Topology {
            cluster: c,
            instances: &instances,
            endpoints: &endpoints,
            pulse,
            selected: self.pick,
            volume: pulse.and_then(|p| p.last(&c.id, "volume")),
        };
        if instances.is_empty() {
            f.render_widget(widgets::placeholder(Line::styled("The cluster has no instances", theme::muted())), body);
            return;
        }
        // The diagram with the details beside it, or below it, or a list when it does not fit.
        let side = body.width >= 100 + SIDE_W;
        let diagram_w = if side { body.width - SIDE_W - 2 } else { body.width };
        let drawn = topology::diagram(&t, diagram_w as usize);
        let (mut lines, item_rows, hits) = match &drawn {
            Some((canvas, hits)) => (canvas.lines(0, 0, canvas.w, canvas.h), hits.iter().map(|h| (h.item, h.y)).collect::<Vec<_>>(), hits.clone()),
            None => {
                let (lines, rows) = topology::list(&t, diagram_w as usize);
                (lines, rows.into_iter().enumerate().collect(), Vec::new())
            }
        };
        let details = topology::details(&t, if side { SIDE_W as usize - 2 } else { body.width as usize });
        let diagram_h = lines.len();
        if !side {
            lines.push(Line::raw(""));
            lines.extend(details.clone());
        }
        if let Some(&(_, row)) = item_rows.iter().find(|(item, _)| *item == self.pick) {
            self.keep_visible(row as u16, body.height.min(diagram_h as u16).max(1));
        }
        let [left, right] = if side {
            body.layout(&Layout::horizontal([Constraint::Length(diagram_w), Constraint::Fill(1)]))
        } else {
            [body, Rect::default()]
        };
        f.render_widget(Paragraph::new(Text::from(lines)).scroll((self.scroll, 0)), left);
        for h in hits {
            let y = h.y as i32 - self.scroll as i32;
            if y < 0 || y as u16 >= left.height {
                continue;
            }
            let rect = Rect::new(left.x + h.x as u16, left.y + y as u16, h.w as u16, (h.h as u16).min(left.height - y as u16));
            self.hits.push((rect.intersection(left), h.item));
        }
        if side {
            let block = widgets::panel(" Details ", false);
            let inner = block.inner(right);
            f.render_widget(block, right);
            f.render_widget(Paragraph::new(Text::from(details)).wrap(Wrap { trim: false }), inner);
        }
    }
}

/// The confirmation of `action`: what it is about, what can still be edited, what it does,
/// and the environment's name to type in production.
fn action_form(action: &Action, c: &Cluster, instances: &[&Instance], env: &str) -> Form {
    let node = |id: &str| {
        instances.iter().find(|i| i.id == id).map(|i| format!("{} · {} · {}", i.id, i.role(), i.class)).unwrap_or_else(|| id.to_string())
    };
    let cluster = Field::fixed("Cluster", format!("{} · {} · {}", c.id, c.engine_label(), c.status));
    let (title, fields, submit) = match action {
        Action::Failover { to, .. } => (
            action.title(),
            vec![
                cluster,
                Field::fixed("Writer now", c.writer().map(node).unwrap_or_else(|| "none".into())),
                Field::fixed("New writer", to.as_deref().map(node).unwrap_or_else(|| "the reader AWS picks by promotion tier".into())),
            ],
            "fail over",
        ),
        Action::Reboot { instance } => (action.title(), vec![Field::fixed("Instance", node(instance)), cluster], "reboot"),
        Action::Stop { .. } | Action::Start { .. } => {
            let names: Vec<&str> = instances.iter().map(|i| i.id.as_str()).collect();
            let submit = if matches!(action, Action::Stop { .. }) { "stop" } else { "start" };
            (action.title(), vec![cluster, Field::fixed("Instances", names.join(", "))], submit)
        }
        Action::SetClass { instance, class, now } => (
            format!("Change the class of {instance}"),
            vec![
                Field::fixed("Instance", node(instance)),
                Field::fixed("New class", format!("{class} · {}", rds::class_detail(class))),
                Field::choice("Apply", &APPLY, if *now { APPLY[0] } else { APPLY[1] }),
            ],
            "change",
        ),
        Action::SetCapacity { min, max, .. } => (
            format!("Serverless v2 range of {}", c.id),
            vec![cluster, Field::text("Minimum ACU", rds::fmt_acu(*min)), Field::text("Maximum ACU", rds::fmt_acu(*max))],
            "change",
        ),
        Action::Snapshot { id, .. } => (format!("Snapshot {}", c.id), vec![cluster, Field::text("Name", id.clone())], "create"),
    };
    let mut form = Form::new(title, fields).submit_label(submit).enter_submits();
    form.note = Some(rds::explain(action, c, instances));
    if is_production(env) {
        form = form.confirm_with(env);
        if matches!(action, Action::Failover { .. } | Action::Reboot { .. } | Action::Stop { .. } | Action::Start { .. }) {
            // Nothing to edit: straight to typing the environment's name.
            form.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
    }
    form
}

/// `action` with what its form edits: a class change's timing, an ACU range, a snapshot's
/// name.
fn with_form(action: &Action, form: &Form) -> Result<Action, String> {
    Ok(match action {
        Action::SetClass { instance, class, .. } => {
            Action::SetClass { instance: instance.clone(), class: class.clone(), now: form.get("Apply") == APPLY[0] }
        }
        Action::SetCapacity { cluster, .. } => {
            let number = |label: &str| {
                let text = form.get(label).trim();
                text.parse::<f64>().map_err(|_| format!("{label}: '{text}' is not a number"))
            };
            let (min, max) = (number("Minimum ACU")?, number("Maximum ACU")?);
            rds::check_acu(min, max)?;
            Action::SetCapacity { cluster: cluster.clone(), min, max }
        }
        Action::Snapshot { cluster, .. } => {
            let id = form.get("Name").trim().to_string();
            rds::check_snapshot_name(&id)?;
            Action::Snapshot { cluster: cluster.clone(), id }
        }
        other => other.clone(),
    })
}

impl Module for Aurora {
    fn title(&self) -> &'static str {
        "Aurora"
    }

    fn description(&self) -> &'static str {
        "Aurora clusters · topology, performance, top SQL"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Aurora::new();
        let env = ctx.env();
        self.env_name = env.name.clone();
        self.region = env.region.clone();
        self.profile = env.profile.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) || self.loaded_at.elapsed() >= LIST_FRESH {
            self.load_inventory(ctx);
        } else if self.pulse.as_ref().is_none_or(|(at, _)| at.elapsed() >= PULSE_EVERY) {
            self.fetch_pulse(ctx);
        }
        self.fetch_tab(ctx, false);
    }

    /// Shows the cluster `target` (its identifier or ARN).
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        self.zoom = None;
        self.pending_open = Some(target.to_string());
        if matches!(self.load, LoadState::Loaded) && !self.try_open(ctx) {
            self.pending_open = None;
            ctx.toast(Level::Warn, format!("Aurora cluster not found here: {target}"));
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() {
            self.dialog_key(key, ctx);
            return Handled::Yes;
        }
        if let Some((_, view)) = &mut self.zoom {
            if let LogKey::Copy = view.key(key) {
                let text = view.lines().join("\n");
                copy(ctx, "statement", &text, "the statement");
            }
            if !view.zoomed {
                self.zoom = None;
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
                    KeyCode::Up => self.list_state.select(Some(selected.saturating_sub(1))),
                    KeyCode::Down => self.list_state.select(Some((selected + 1).min(self.rows.len().saturating_sub(1)))),
                    _ => {}
                },
                InputEvent::Moved => {}
            }
            return Handled::Yes;
        }
        // Keys that work on the list and on a cluster alike.
        match key.code {
            KeyCode::Char('y') => {
                self.copy_host(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('Y') => {
                self.copy_connect(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('c') => {
                self.copy_report(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('o') => {
                self.open_console(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('r') => {
                self.load_inventory(ctx);
                self.fetch_pulse(ctx);
                self.fetch_tab(ctx, true);
                return Handled::Yes;
            }
            KeyCode::Char('f') => {
                self.start_failover(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('x') => {
                self.start_stop(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('S') => {
                self.start_snapshot(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('b') => {
                self.start_reboot(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('M') => {
                self.start_resize(ctx);
                return Handled::Yes;
            }
            _ => {}
        }
        if self.open.is_none() {
            let selected = self.list_state.selected().unwrap_or(0);
            let last = self.rows.len().saturating_sub(1);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.list_state.select(Some(selected.saturating_sub(1))),
                KeyCode::Down | KeyCode::Char('j') => self.list_state.select(Some((selected + 1).min(last))),
                KeyCode::Enter | KeyCode::Right => self.open_cluster(ctx),
                KeyCode::Char('/') => self.filtering = true,
                KeyCode::Esc if !self.filter.value.is_empty() => {
                    self.filter.clear();
                    self.refilter();
                }
                _ => return Handled::No,
            }
            return Handled::Yes;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => {
                if self.sql_cursor.is_some() {
                    self.sql_cursor = None;
                } else {
                    self.open = None;
                    self.scroll = 0;
                }
            }
            KeyCode::Left => self.step_tab(false, ctx),
            KeyCode::Right | KeyCode::Tab => self.step_tab(true, ctx),
            KeyCode::BackTab => self.step_tab(false, ctx),
            KeyCode::Char('m') => {
                self.range = self.range.next();
                self.sql_cursor = None;
                self.fetch_tab(ctx, false);
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                let delta = if matches!(key.code, KeyCode::Up | KeyCode::Char('k')) { -1 } else { 1 };
                match self.tab {
                    Tab::Topology => self.move_pick(delta),
                    Tab::Sql if self.sql_cursor.is_some() => {
                        let count = self.statements();
                        self.sql_cursor = self.sql_cursor.map(|n| (n as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize);
                    }
                    Tab::Performance | Tab::Sql => self.move_node(delta, ctx),
                    Tab::Events | Tab::Config => self.scroll = (self.scroll as isize + delta).max(0) as u16,
                }
            }
            KeyCode::Enter => match self.tab {
                // A node opens its metrics.
                Tab::Topology if self.pick >= self.endpoints().len() => self.set_tab(Tab::Performance, ctx),
                Tab::Sql if self.sql_cursor.is_some() => self.zoom_statement(),
                Tab::Sql if self.statements() > 0 => self.sql_cursor = Some(0),
                _ => {}
            },
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Handled {
        if self.zoom.is_some() {
            return Handled::No;
        }
        let at = (event.column, event.row).into();
        if self.open.is_none() {
            if !self.list_area.contains(at) {
                return Handled::No;
            }
            let selected = self.list_state.selected().unwrap_or(0);
            let last = self.rows.len().saturating_sub(1);
            match event.kind {
                MouseEventKind::ScrollUp => self.list_state.select(Some(selected.saturating_sub(1))),
                MouseEventKind::ScrollDown => self.list_state.select(Some((selected + 1).min(last))),
                MouseEventKind::Down(MouseButton::Left) => {
                    // Rows start below the filter bar and the panel border.
                    let row = event.row.saturating_sub(self.list_area.y + 2) as usize;
                    let index = self.list_state.offset() + row;
                    if index <= last {
                        if self.list_state.selected() == Some(index) {
                            self.open_cluster(ctx);
                        } else {
                            self.list_state.select(Some(index));
                        }
                    }
                }
                _ => return Handled::No,
            }
            return Handled::Yes;
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(2),
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_add(2),
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(&(_, item)) = self.hits.iter().find(|(r, _)| r.contains(at)) else { return Handled::No };
                self.pick = item;
                if let Some(n) = item.checked_sub(self.endpoints().len()) {
                    self.node = n;
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Inventory(result) => {
                self.in_flight.remove("inventory");
                match result {
                    Ok(inventory) => {
                        // The selected node stays selected when the order changes (a failover
                        // moves the new writer first).
                        let kept = self.node().map(|n| n.id.clone());
                        let on_node = self.pick >= self.endpoints().len();
                        self.inventory = inventory;
                        self.load = LoadState::Loaded;
                        self.refilter();
                        if self.open.as_deref().is_some_and(|id| self.inventory.cluster(id).is_none()) {
                            self.open = None;
                        }
                        if let Some(at) = kept.and_then(|id| self.instances().iter().position(|i| i.id == id)) {
                            self.node = at;
                            if on_node {
                                self.pick = self.endpoints().len() + at;
                            }
                        }
                        let items = self.endpoints().len() + self.instances().len();
                        self.pick = self.pick.min(items.saturating_sub(1));
                        self.node = self.node.min(self.instances().len().saturating_sub(1));
                        self.try_open(ctx);
                        self.fetch_pulse(ctx);
                    }
                    Err(e) => {
                        if self.inventory.clusters.is_empty() {
                            self.load = LoadState::Failed(e);
                        } else {
                            ctx.toast(Level::Error, format!("Could not list the clusters: {}", e.short()));
                        }
                    }
                }
            }
            Msg::Pulse(result) => {
                self.in_flight.remove("pulse");
                // A failed refresh keeps the previous figures.
                if result.is_ok() || self.pulse.is_none() {
                    self.pulse = Some((Instant::now(), result));
                } else if let Some((at, _)) = &mut self.pulse {
                    *at = Instant::now();
                }
            }
            Msg::Metrics { cluster, range, result } => {
                self.in_flight.remove(&format!("metrics {cluster} {}", range.label()));
                self.metrics.insert((cluster, range), (Instant::now(), result));
            }
            Msg::Insights { instance, range, data } => {
                self.in_flight.remove(&format!("insights {instance} {}", range.label()));
                self.insights.insert((instance, range), (Instant::now(), *data));
            }
            Msg::Events(result) => {
                self.in_flight.remove("events");
                self.events = Some((Instant::now(), result));
            }
            Msg::Maintenance(result) => self.maintenance = Some(result),
            Msg::Report { cluster, result } => {
                self.in_flight.remove(&format!("report {cluster}"));
                match result {
                    Ok(md) => copy(ctx, &format!("{cluster}-diagnosis"), &md, &format!("the performance diagnosis of {cluster}")),
                    Err(e) => ctx.toast(Level::Error, format!("Could not write the diagnosis: {}", e.short())),
                }
            }
            Msg::Classes { instance, result } => self.on_classes(instance, result, ctx),
            Msg::Applied { action, result } => self.on_applied(action, result, ctx),
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if !active || !matches!(self.load, LoadState::Loaded) {
            return;
        }
        if self.pulse.as_ref().is_some_and(|(at, _)| at.elapsed() >= PULSE_EVERY) {
            self.fetch_pulse(ctx);
        }
        let changing = self.inventory.clusters.iter().map(|c| &c.status).chain(self.inventory.instances.iter().map(|i| &i.status)).any(|s| {
            matches!(rds::status_health(s), rds::Health::Changing) || s == "stopping"
        });
        let watching = self.watch_until.is_some_and(|t| Instant::now() < t);
        if (changing || watching) && self.loaded_at.elapsed() >= CHANGING_EVERY {
            self.load_inventory(ctx);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some((title, zoomed)) = &mut self.zoom {
            let title = title.clone();
            zoomed.render_zoomed(f, area, &title);
            return;
        }
        if self.open.is_some() && self.cluster().is_some() {
            self.render_cluster(f, area, view);
        } else {
            self.render_list(f, area, view);
        }
        self.render_dialog(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.dialog {
            Some(Dialog::Classes { .. }) => return vec![("Esc", "cancel")],
            Some(Dialog::Class(..)) => return vec![("↑↓", "class"), ("Enter", "choose"), ("Esc", "cancel")],
            Some(Dialog::Confirm { .. }) => return vec![("Enter", "confirm"), ("Tab", "next field"), ("Esc", "cancel")],
            None => {}
        }
        if self.zoom.is_some() {
            return vec![("/", "search"), ("y", "copy"), ("Esc", "back")];
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        if self.open.is_none() {
            return vec![
                ("↑↓", "cluster"),
                ("Enter", "open"),
                ("y", "copy writer endpoint"),
                ("Y", "copy connect command"),
                ("c", "copy diagnosis for AI"),
                ("f", "fail over"),
                ("x", "stop / start"),
                ("S", "snapshot"),
                ("/", "search"),
                ("o", "console"),
                ("r", "reload"),
            ];
        }
        let mut hints = vec![("←→", "view")];
        match self.tab {
            Tab::Topology => hints.extend([("↑↓", "endpoint / node"), ("Enter", "node metrics"), ("y", "copy endpoint"), ("Y", "copy connect command")]),
            Tab::Performance => hints.extend([("↑↓", "node"), ("m", "range"), ("y", "copy endpoint")]),
            Tab::Sql if self.sql_cursor.is_some() => hints.extend([("↑↓", "statement"), ("Enter", "whole statement"), ("y", "copy statement")]),
            Tab::Sql => hints.extend([("↑↓", "node"), ("Enter", "statements"), ("m", "range")]),
            Tab::Events | Tab::Config => hints.extend([("↑↓", "scroll")]),
        }
        if self.tab == Tab::Topology && self.pick >= self.endpoints().len() && self.node().is_some_and(|n| !n.writer) {
            hints.push(("f", "fail over to this reader"));
        } else {
            hints.push(("f", "fail over"));
        }
        hints.extend([("b", "reboot node"), ("M", "class / ACU"), ("x", "stop / start"), ("S", "snapshot")]);
        hints.extend([("c", "copy diagnosis for AI"), ("o", "console"), ("r", "reload"), ("Esc", "clusters")]);
        hints
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading) || !self.in_flight.is_empty() || matches!(self.dialog, Some(Dialog::Confirm { busy: true, .. }))
    }

    fn capturing(&self) -> bool {
        self.filtering || self.zoom.is_some() || self.dialog.is_some()
    }

    fn wants_mouse(&self) -> bool {
        self.zoom.is_none()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.dialog {
            Some(Dialog::Confirm { form, busy: false, .. }) => return form.paste(text),
            Some(Dialog::Class(picker, ..)) => return picker.paste(text),
            _ => {}
        }
        if self.filtering {
            self.filter.insert_str(text, false);
            self.refilter();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster() -> Cluster {
        Cluster {
            id: "c".into(),
            status: "available".into(),
            engine: "aurora-postgresql".into(),
            engine_version: "16.4".into(),
            serverless: Some(rds::Capacity { min: 4.0, max: 16.0 }),
            ..Default::default()
        }
    }

    fn press(form: &mut Form, code: KeyCode) -> FormEvent {
        form.handle(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn forms_read_what_they_edit() {
        let c = cluster();
        let acu = Action::SetCapacity { cluster: "c".into(), min: 4.0, max: 16.0 };
        let mut form = action_form(&acu, &c, &[], "dev");
        form.field_mut("Minimum ACU").unwrap().input = TextInput::with_value("1");
        assert_eq!(with_form(&acu, &form), Ok(Action::SetCapacity { cluster: "c".into(), min: 1.0, max: 16.0 }));
        form.field_mut("Maximum ACU").unwrap().input = TextInput::with_value("lots");
        assert!(with_form(&acu, &form).unwrap_err().contains("not a number"));

        let snapshot = Action::Snapshot { cluster: "c".into(), id: "c-2026-10-22-1730".into() };
        let mut form = action_form(&snapshot, &c, &[], "dev");
        assert_eq!(with_form(&snapshot, &form), Ok(snapshot.clone()));
        form.field_mut("Name").unwrap().input = TextInput::with_value("before upgrade");
        assert!(with_form(&snapshot, &form).is_err());

        let node = Instance { id: "r".into(), cluster: "c".into(), class: "db.r6g.large".into(), status: "available".into(), ..Default::default() };
        let class = Action::SetClass { instance: "r".into(), class: "db.r7g.large".into(), now: true };
        let mut form = action_form(&class, &c, &[&node], "dev");
        assert_eq!(form.focused_label(), Some("Apply"));
        press(&mut form, KeyCode::Right);
        assert_eq!(with_form(&class, &form), Ok(Action::SetClass { instance: "r".into(), class: "db.r7g.large".into(), now: false }));
    }

    #[test]
    fn production_asks_for_its_name_first() {
        let c = cluster();
        let stop = Action::Stop { cluster: "c".into() };
        let mut dev = action_form(&stop, &c, &[], "dev");
        assert!(matches!(press(&mut dev, KeyCode::Enter), FormEvent::Submit));
        let mut prod = action_form(&stop, &c, &[], "prod");
        assert!(matches!(press(&mut prod, KeyCode::Enter), FormEvent::Pending));
        for ch in "prod".chars() {
            press(&mut prod, KeyCode::Char(ch));
        }
        assert!(matches!(press(&mut prod, KeyCode::Enter), FormEvent::Submit));
    }
}

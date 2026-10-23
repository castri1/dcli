//! Redshift: provisioned clusters with their health; a cluster's nodes (the leader and the
//! compute nodes with live CPU and disk, skew flagged), metrics, the queries running, queued
//! and slowest with the locks between them, the tables that need upkeep, events, and
//! configuration with snapshots. Pauses, resumes, resizes, takes manual snapshots and
//! cancels queries (asking first; typing the environment name in production). Copies the
//! endpoint, a connection command and a performance diagnosis for an AI agent.
//!
//! Queries and tables come from system tables through the Data API, as a database identity
//! saved per environment and cluster (`aws::redshift_data`). Serverless workgroups would be
//! listed next to the clusters: the list rows and `Nodes` are what would grow a second kind.

mod nodes;
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
use crate::aws::rds::Metrics;
use crate::aws::redshift::{self, Activity, Cluster, Event, NodeOption, Snapshot, TableInfo, Wlm};
use crate::aws::redshift_data::{self, Identity};
use crate::aws::secrets::{self, Secret};
use crate::aws;
use crate::config::is_production;
use crate::tui::chart::{Agg, columns, sparkline_to};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, now_secs};
use nodes::{Nodes, health_color};
use views::Sort;

/// How often the live figures (CPU, disk, connections) are read again while shown.
const PULSE_EVERY: Duration = Duration::from_secs(60);
/// Clusters are listed again after this when the module is opened again…
const LIST_FRESH: Duration = Duration::from_secs(300);
/// …and this often while one is changing (resizing, pausing, resuming…).
const CHANGING_EVERY: Duration = Duration::from_secs(20);
const DATA_FRESH: Duration = Duration::from_secs(120);
const EVENTS_FRESH: Duration = Duration::from_secs(300);
/// Width of the details beside the nodes diagram.
const SIDE_W: u16 = 48;

const SIGN_IN: [&str; 2] = ["database user", "secret"];

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Nodes,
    Performance,
    Queries,
    Tables,
    Events,
    Config,
}

const TABS: [Tab; 6] = [Tab::Nodes, Tab::Performance, Tab::Queries, Tab::Tables, Tab::Events, Tab::Config];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Nodes => "Nodes",
            Tab::Performance => "Performance",
            Tab::Queries => "Queries",
            Tab::Tables => "Tables",
            Tab::Events => "Events",
            Tab::Config => "Configuration",
        }
    }

    /// Runs SQL through the Data API.
    fn sql(self) -> bool {
        matches!(self, Tab::Queries | Tab::Tables)
    }
}

enum Msg {
    Clusters(aws::Result<Vec<Cluster>>),
    Pulse(aws::Result<Metrics>),
    Metrics { cluster: String, range: Range, result: aws::Result<Metrics> },
    Activity { cluster: String, activity: Box<Activity> },
    Tables { cluster: String, result: aws::Result<Vec<TableInfo>> },
    Events(aws::Result<Vec<Event>>),
    Snapshots { cluster: String, result: aws::Result<Vec<Snapshot>> },
    Wlm { cluster: String, result: aws::Result<Wlm> },
    Secrets { cluster: String, database: String, result: aws::Result<Vec<Secret>> },
    Options { cluster: String, result: aws::Result<Vec<NodeOption>> },
    Done { cluster: String, what: String, result: aws::Result<()> },
    Report { cluster: String, result: aws::Result<String> },
}

#[derive(Clone)]
enum Action {
    Pause(String),
    Resume(String),
    Resize { cluster: String, option: NodeOption },
    Cancel { cluster: String, query: Box<redshift::Query> },
}

enum Dialog {
    Identity { form: Form, cluster: String },
    /// A secret for the identity: (picker, cluster, database, ARNs by item).
    Secrets(Picker, String, String, Vec<String>),
    /// The shape to resize to: (picker, cluster, options by item).
    Resize(Picker, String, Vec<NodeOption>),
    Snapshot { form: Form, cluster: String, busy: bool },
    Confirm { title: String, lines: Vec<String>, typed: Option<(String, TextInput)>, action: Action, busy: bool, error: Option<String> },
}

pub struct Redshift {
    clusters: Vec<Cluster>,
    load: LoadState,
    loaded_at: Instant,
    filter: TextInput,
    filtering: bool,
    /// Indexes in `clusters` shown, after the filter.
    rows: Vec<usize>,
    list_state: ListState,
    /// The cluster shown (Enter on the list); `None` shows the list.
    open: Option<String>,
    tab: Tab,
    /// The node selected (index in the cluster's nodes): the Nodes and Performance tabs'.
    node: usize,
    range: Range,
    pulse: Option<(Instant, aws::Result<Metrics>)>,
    metrics: HashMap<(String, Range), (Instant, aws::Result<Metrics>)>,
    activity: HashMap<String, (Instant, Activity)>,
    tables: HashMap<String, (Instant, aws::Result<Vec<TableInfo>>)>,
    events: Option<(Instant, aws::Result<Vec<Event>>)>,
    snapshots: HashMap<String, (Instant, aws::Result<Vec<Snapshot>>)>,
    wlm: HashMap<String, aws::Result<Wlm>>,
    /// Database identities by cluster, read from the saved file when first needed.
    identities: HashMap<String, Option<Identity>>,
    /// Clusters whose identity form already opened by itself.
    asked: HashSet<String>,
    in_flight: HashSet<String>,
    /// Queries tab: the query selected, while the queries have the keys.
    query_cursor: Option<usize>,
    sort: Sort,
    /// A statement, full screen.
    zoom: Option<(String, LogView)>,
    scroll: u16,
    dialog: Option<Dialog>,
    /// A cluster another module asked to show, opened once the list loads.
    pending_open: Option<String>,
    env_name: String,
    region: String,
    profile: String,
    list_area: Rect,
    /// Where the diagram's boxes are on screen, for clicks.
    hits: Vec<(Rect, usize)>,
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

impl Redshift {
    pub fn new() -> Redshift {
        Redshift {
            clusters: Vec::new(),
            load: LoadState::Idle,
            loaded_at: Instant::now(),
            filter: TextInput::default(),
            filtering: false,
            rows: Vec::new(),
            list_state: ListState::default(),
            open: None,
            tab: Tab::Nodes,
            node: 0,
            range: Range::Day,
            pulse: None,
            metrics: HashMap::new(),
            activity: HashMap::new(),
            tables: HashMap::new(),
            events: None,
            snapshots: HashMap::new(),
            wlm: HashMap::new(),
            identities: HashMap::new(),
            asked: HashSet::new(),
            in_flight: HashSet::new(),
            query_cursor: None,
            sort: Sort::Size,
            zoom: None,
            scroll: 0,
            dialog: None,
            pending_open: None,
            env_name: String::new(),
            region: String::new(),
            profile: String::new(),
            list_area: Rect::default(),
            hits: Vec::new(),
        }
    }

    // ---- data ------------------------------------------------------------------------------

    fn load_clusters(&mut self, ctx: &mut Ctx) {
        if !self.in_flight.insert("clusters".into()) {
            return;
        }
        if self.clusters.is_empty() {
            self.load = LoadState::Loading;
        }
        self.loaded_at = Instant::now();
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Clusters(redshift::describe_clusters(&aws, None)));
    }

    fn fetch_pulse(&mut self, ctx: &mut Ctx) {
        if self.clusters.is_empty() || !self.in_flight.insert("pulse".into()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        let clusters = self.clusters.clone();
        ctx.spawn(move || {
            let refs: Vec<&Cluster> = clusters.iter().collect();
            Msg::Pulse(redshift::pulse(&aws, &refs, now_secs()))
        });
    }

    fn identity(&self, cluster: &str) -> Option<&Identity> {
        self.identities.get(cluster).and_then(Option::as_ref)
    }

    /// Reads the saved identity of `cluster` the first time it is needed.
    fn load_identity(&mut self, cluster: &str) {
        if !self.identities.contains_key(cluster) {
            let saved = redshift_data::saved_identity(&self.env_name, cluster);
            self.identities.insert(cluster.to_string(), saved);
        }
    }

    /// Reads what the current tab shows, unless it is here and fresh (or `force`).
    fn fetch_tab(&mut self, ctx: &mut Ctx, force: bool) {
        let Some(c) = self.cluster().cloned() else { return };
        let aws = ctx.env().aws.clone();
        let range = self.range;
        match self.tab {
            Tab::Nodes => {}
            Tab::Performance => {
                let key = (c.id.clone(), range);
                let fresh = self.metrics.get(&key).is_some_and(|(at, r)| r.is_ok() && at.elapsed() < DATA_FRESH);
                if c.paused() || (fresh && !force) || !self.in_flight.insert(format!("metrics {} {}", c.id, range.label())) {
                    return;
                }
                ctx.spawn(move || Msg::Metrics { result: redshift::cluster_metrics(&aws, &c, range, now_secs()), cluster: c.id, range });
            }
            Tab::Queries | Tab::Tables => {
                self.load_identity(&c.id);
                let Some(identity) = self.identity(&c.id).cloned() else {
                    // The first time, the form opens by itself.
                    if !c.paused() && self.asked.insert(c.id.clone()) && self.dialog.is_none() {
                        self.ask_identity(&c);
                    }
                    return;
                };
                if c.paused() {
                    return;
                }
                if self.tab == Tab::Queries {
                    let fresh = self.activity.get(&c.id).is_some_and(|(at, _)| at.elapsed() < DATA_FRESH);
                    if (fresh && !force) || !self.in_flight.insert(format!("activity {}", c.id)) {
                        return;
                    }
                    ctx.spawn(move || Msg::Activity { activity: Box::new(redshift::activity(&aws, &c.id, &identity)), cluster: c.id });
                } else {
                    let fresh = self.tables.get(&c.id).is_some_and(|(at, r)| r.is_ok() && at.elapsed() < EVENTS_FRESH);
                    if (fresh && !force) || !self.in_flight.insert(format!("tables {}", c.id)) {
                        return;
                    }
                    ctx.spawn(move || Msg::Tables { result: redshift::tables(&aws, &c.id, &identity), cluster: c.id });
                }
            }
            Tab::Events => {
                let fresh = self.events.as_ref().is_some_and(|(at, r)| r.is_ok() && at.elapsed() < EVENTS_FRESH);
                if (fresh && !force) || !self.in_flight.insert("events".into()) {
                    return;
                }
                ctx.spawn(move || Msg::Events(redshift::describe_events(&aws, 24 * 7)));
            }
            Tab::Config => {
                self.load_identity(&c.id);
                let fresh = self.snapshots.get(&c.id).is_some_and(|(at, r)| r.is_ok() && at.elapsed() < EVENTS_FRESH);
                if (!fresh || force) && self.in_flight.insert(format!("snapshots {}", c.id)) {
                    let (aws, id) = (aws.clone(), c.id.clone());
                    ctx.spawn(move || Msg::Snapshots { result: redshift::describe_snapshots(&aws, Some(&id)), cluster: id });
                }
                if let Some((group, _)) = c.parameter_groups.first().cloned()
                    && (force || !self.wlm.contains_key(&c.id))
                    && self.in_flight.insert(format!("wlm {}", c.id))
                {
                    ctx.spawn(move || Msg::Wlm { result: redshift::describe_wlm(&aws, &group), cluster: c.id });
                }
            }
        }
    }

    /// The rows again after the filter changed: the selected cluster stays selected while it
    /// shows, else the first row is.
    fn refilter(&mut self) {
        let keep = self.list_cluster().map(|c| c.id.clone());
        self.rebuild(keep, false);
    }

    /// The rows after `keep` (the id of the cluster selected before new data replaced the
    /// list) was looked for: wherever it moved it stays selected; when it is gone after a
    /// refresh (`new_data`), nothing is, so that a key cannot act on whatever took its row.
    fn rebuild(&mut self, keep: Option<String>, new_data: bool) {
        let query = self.filter.value.trim().to_lowercase();
        self.rows = (0..self.clusters.len()).filter(|i| matches_filter(&self.clusters[*i].id, &query)).collect();
        let index = match keep.map(|k| self.rows.iter().position(|i| self.clusters[*i].id == k)) {
            Some(found) if new_data => found,
            Some(Some(found)) => Some(found),
            _ => Some(0),
        };
        self.list_state.select(if self.rows.is_empty() { None } else { index });
    }

    /// Opens the cluster asked for by another module, once the list has it.
    fn try_open(&mut self, ctx: &mut Ctx) -> bool {
        let Some(target) = self.pending_open.clone() else { return false };
        let Some(i) = self.clusters.iter().position(|c| c.id == target) else { return false };
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

    fn list_cluster(&self) -> Option<&Cluster> {
        self.clusters.get(*self.rows.get(self.list_state.selected()?)?)
    }

    fn cluster(&self) -> Option<&Cluster> {
        let id = self.open.as_deref()?;
        self.clusters.iter().find(|c| c.id == id)
    }

    /// The cluster the keys act on: the open one, or the one under the list's cursor.
    fn target(&self) -> Option<&Cluster> {
        if self.open.is_some() { self.cluster() } else { self.list_cluster() }
    }

    fn pulse(&self) -> Option<&Metrics> {
        self.pulse.as_ref().and_then(|(_, r)| r.as_ref().ok())
    }

    fn open_cluster(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.list_cluster().map(|c| c.id.clone()) else { return };
        self.open = Some(c);
        self.tab = Tab::Nodes;
        self.node = 0;
        self.scroll = 0;
        self.query_cursor = None;
        self.fetch_tab(ctx, false);
    }

    fn set_tab(&mut self, tab: Tab, ctx: &mut Ctx) {
        self.tab = tab;
        self.scroll = 0;
        self.query_cursor = None;
        self.fetch_tab(ctx, false);
    }

    fn step_tab(&mut self, forward: bool, ctx: &mut Ctx) {
        let at = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
        let next = if forward { (at + 1) % TABS.len() } else { (at + TABS.len() - 1) % TABS.len() };
        self.set_tab(TABS[next], ctx);
    }

    fn move_node(&mut self, delta: isize, wrap: bool) {
        let count = self.cluster().map_or(0, |c| c.nodes.len());
        if count == 0 {
            return;
        }
        self.node = if wrap {
            (self.node as isize + delta).rem_euclid(count as isize) as usize
        } else {
            (self.node as isize + delta).clamp(0, count as isize - 1) as usize
        };
        self.scroll = 0;
    }

    fn listed_queries(&self) -> Vec<redshift::Query> {
        let Some(id) = self.open.as_deref() else { return Vec::new() };
        self.activity.get(id).map(|(_, a)| views::listed(a).into_iter().cloned().collect()).unwrap_or_default()
    }

    /// The selected query, and whether it is running or queued now (cancellable).
    fn selected_query(&self) -> Option<(redshift::Query, bool)> {
        let n = self.query_cursor?;
        let id = self.open.as_deref()?;
        let (_, a) = self.activity.get(id)?;
        let live = a.running.as_ref().map_or(0, Vec::len);
        let q = views::listed(a).get(n).map(|q| (*q).clone())?;
        Some((q, n < live))
    }

    // ---- dialogs and actions -------------------------------------------------------------------

    fn typed_name(&self) -> Option<String> {
        is_production(&self.env_name).then(|| self.env_name.clone())
    }

    fn confirm(&mut self, title: String, lines: Vec<String>, action: Action) {
        let typed = self.typed_name().map(|n| (n, TextInput::default()));
        self.dialog = Some(Dialog::Confirm { title, lines, typed, action, busy: false, error: None });
    }

    fn ask_identity(&mut self, c: &Cluster) {
        let current = self.identity(&c.id).cloned();
        let database = current.as_ref().map(|i| i.database.clone()).unwrap_or_else(|| c.database());
        let user = current.as_ref().and_then(|i| i.db_user.clone()).or_else(|| c.master_username.clone()).unwrap_or_default();
        let secret = current.as_ref().and_then(|i| i.secret_arn.clone()).unwrap_or_default();
        let mode = if secret.is_empty() { SIGN_IN[0] } else { SIGN_IN[1] };
        let mut form = Form::new(
            format!("Database identity · {}", c.id),
            vec![
                Field::fixed("Cluster", c.id.clone()),
                Field::text("Database", database),
                Field::choice("Sign in with", &SIGN_IN, mode),
                Field::text("Database user", user).optional("with temporary credentials (redshift:GetClusterCredentials)"),
                Field::text("Secret ARN", secret).optional("empty: pick one from Secrets Manager"),
            ],
        )
        .submit_label("save");
        form.note = Some(
            "Queries and tables run SQL on system views through the Data API, as this identity. dcli saves it in \
             ~/.deployment-cli/redshift.json (no password) and never sees a password."
                .into(),
        );
        self.dialog = Some(Dialog::Identity { form, cluster: c.id.clone() });
    }

    fn submit_identity(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Identity { form, cluster }) = &mut self.dialog else { return };
        let database = form.get("Database").trim().to_string();
        let cluster = cluster.clone();
        if form.get("Sign in with") == SIGN_IN[0] {
            let user = form.get("Database user").trim().to_string();
            if user.is_empty() {
                return form.fail("A database user is required to sign in with temporary credentials");
            }
            return self.save_identity(&cluster, Identity { database, db_user: Some(user), secret_arn: None }, ctx);
        }
        let arn = form.get("Secret ARN").trim().to_string();
        if arn.starts_with("arn:") {
            return self.save_identity(&cluster, Identity { database, db_user: None, secret_arn: Some(arn) }, ctx);
        }
        if !arn.is_empty() {
            return form.fail("Paste the secret's ARN (arn:aws:secretsmanager:…), or leave it empty to pick one");
        }
        self.dialog = None;
        ctx.toast(Level::Info, "Listing secrets…");
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let mut all = Vec::new();
            let mut token: Option<String> = None;
            let result = loop {
                match secrets::list_secrets(&aws, token.as_deref()) {
                    Ok((page, next)) => {
                        all.extend(page);
                        match next {
                            Some(t) if all.len() < 2000 => token = Some(t),
                            _ => break Ok(all),
                        }
                    }
                    Err(e) => break Err(e),
                }
            };
            Msg::Secrets { cluster, database, result }
        });
    }

    fn save_identity(&mut self, cluster: &str, identity: Identity, ctx: &mut Ctx) {
        self.dialog = None;
        if let Err(e) = redshift_data::save_identity(&self.env_name, cluster, &identity) {
            ctx.toast(Level::Warn, format!("Using the identity for now; could not save it: {e}"));
        } else {
            ctx.toast(Level::Success, format!("{cluster} runs queries {}", identity.describe().replacen(&identity.database, &format!("in {}", identity.database), 1)));
        }
        self.identities.insert(cluster.to_string(), Some(identity));
        self.activity.remove(cluster);
        self.tables.remove(cluster);
        self.fetch_tab(ctx, true);
    }

    fn on_secrets(&mut self, cluster: String, database: String, result: aws::Result<Vec<Secret>>, ctx: &mut Ctx) {
        let list = match result {
            Ok(list) => list,
            Err(e) => return ctx.toast(Level::Error, format!("Could not list the secrets: {}", e.short())),
        };
        if list.is_empty() {
            return ctx.toast(Level::Warn, "No secrets in Secrets Manager here: paste the ARN of one (i)");
        }
        // Secrets that look like Redshift's first.
        let about_redshift = |s: &Secret| {
            let mut text = s.name.to_lowercase();
            for t in &s.tags {
                text.push_str(&format!(" {} {}", t.key.to_lowercase(), t.value.to_lowercase()));
            }
            text.contains("redshift") || text.contains(&cluster.to_lowercase())
        };
        let mut sorted: Vec<&Secret> = list.iter().collect();
        sorted.sort_by_key(|s| (!about_redshift(s), s.name.to_lowercase()));
        let items = sorted
            .iter()
            .map(|s| {
                let detail = if about_redshift(s) { format!("redshift · {}", s.description.clone().unwrap_or_default()) } else { s.description.clone().unwrap_or_default() };
                Item::new(s.name.clone(), detail)
            })
            .collect();
        let arns = sorted.iter().map(|s| s.arn.clone()).collect();
        let picker = Picker::new(format!("Secret with the user and password for {cluster}"), "use", items, false);
        self.dialog = Some(Dialog::Secrets(picker, cluster, database, arns));
    }

    fn start_pause(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.target().cloned() else { return };
        match c.status.as_str() {
            "available" => self.confirm(
                format!("Pause {}", c.id),
                vec![
                    format!("Pause {} ({})?", c.id, c.shape()),
                    "Running queries stop and connections close; nothing runs until it is resumed.".into(),
                    "Compute is not billed while paused; storage and snapshots still are.".into(),
                    "Resuming takes a few minutes (p again, once it is paused).".into(),
                ],
                Action::Pause(c.id),
            ),
            "paused" => self.confirm(
                format!("Resume {}", c.id),
                vec![format!("Resume {} ({})?", c.id, c.shape()), "It takes a few minutes; the endpoint answers again once it is available.".into()],
                Action::Resume(c.id),
            ),
            s => ctx.toast(Level::Info, format!("{} is {s}: it can be paused when available and resumed when paused", c.id)),
        }
    }

    fn start_resize(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.target().cloned() else { return };
        if c.status != "available" {
            return ctx.toast(Level::Info, format!("{} is {}: a resize starts from an available cluster", c.id, c.status));
        }
        if !self.in_flight.insert(format!("options {}", c.id)) {
            return;
        }
        ctx.toast(Level::Info, format!("Reading the shapes {} can be resized to…", c.id));
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Options { result: redshift::resize_options(&aws, &c.id), cluster: c.id });
    }

    fn on_options(&mut self, cluster: String, result: aws::Result<Vec<NodeOption>>, ctx: &mut Ctx) {
        self.in_flight.remove(&format!("options {cluster}"));
        let Some(c) = self.clusters.iter().find(|c| c.id == cluster).cloned() else { return };
        let options = match result {
            Ok(o) => o,
            Err(e) => return ctx.toast(Level::Error, format!("Could not read the resize options: {}", e.short())),
        };
        let others: Vec<NodeOption> = options.into_iter().filter(|o| !(o.node_type == c.node_type && o.nodes == c.nodes_count)).collect();
        if others.is_empty() {
            let cli = format!("{} redshift {} {} --resize --node-type TYPE --nodes N --classic", crate::util::program(), self.env_name, c.id);
            return ctx.toast(Level::Warn, format!("Elastic resize offers no other shape for {}. A classic resize (hours, read-only meanwhile): {cli}", c.id));
        }
        let items = others
            .iter()
            .map(|o| {
                let delta = if o.node_type == c.node_type {
                    let d = i64::from(o.nodes) - i64::from(c.nodes_count);
                    format!("{d:+} node{}", if d.abs() == 1 { "" } else { "s" })
                } else {
                    "other node type".into()
                };
                let disk = o.disk_percent.map(|d| format!("disk ≈ {d:.0}%")).unwrap_or_else(|| "disk ?".into());
                Item::new(o.shape(), format!("{disk} · {delta}"))
            })
            .collect();
        let picker = Picker::new(format!("Resize {} ({}) to", c.id, c.shape()), "choose", items, false);
        self.dialog = Some(Dialog::Resize(picker, c.id, others));
    }

    fn start_snapshot(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.target().cloned() else { return };
        if c.status != "available" {
            return ctx.toast(Level::Info, format!("{} is {}: snapshots are taken of an available cluster", c.id, c.status));
        }
        let mut form = Form::new(
            format!("Manual snapshot · {}", c.id),
            vec![Field::fixed("Cluster", c.id.clone()), Field::text("Name", redshift::snapshot_name(&c.id, now_secs()))],
        )
        .submit_label("take snapshot")
        .enter_submits();
        form.note = Some("Manual snapshots stay until deleted (or their retention ends) and their storage is billed meanwhile.".into());
        if let Some(name) = self.typed_name() {
            form = form.confirm_with(&name);
        }
        self.dialog = Some(Dialog::Snapshot { form, cluster: c.id, busy: false });
    }

    fn start_cancel(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.cluster().cloned() else { return };
        let Some((q, live)) = self.selected_query() else {
            return ctx.toast(Level::Info, "Select a running query first (Enter, then ↑↓)");
        };
        let Some(pid) = q.pid.and_then(redshift::valid_pid).filter(|_| live) else {
            return ctx.toast(Level::Info, "Only a query running or queued now can be cancelled");
        };
        let running = q.started.map(|s| format!(", {} so far", crate::tui::chart::fmt_ms((now_secs() - s).max(0.0) * 1000.0))).unwrap_or_default();
        self.confirm(
            format!("Cancel query · {}", c.id),
            vec![
                format!("Cancel the {} query of session {pid} ({}{running})?", q.status, q.user),
                ellipsize(&views::one_line(&q.text), 300),
                "It stops and its transaction rolls back. You need to own the session or be a superuser.".into(),
            ],
            Action::Cancel { cluster: c.id, query: Box::new(q) },
        );
    }

    fn run(&mut self, action: Action, ctx: &mut Ctx) {
        let aws = ctx.env().aws.clone();
        match action {
            Action::Pause(id) => {
                let note = self.cost_note(&id, |c| (vec![c.node_type.clone()], format!("Paused {} ({})", c.id, c.shape())));
                ctx.spawn(move || Msg::Done {
                    result: redshift::pause(&aws, &id).inspect(|()| note.iter().for_each(|n| n.record())),
                    what: format!("{id} is pausing"),
                    cluster: id,
                })
            }
            Action::Resume(id) => ctx.spawn(move || Msg::Done { result: redshift::resume(&aws, &id), what: format!("{id} is resuming"), cluster: id }),
            Action::Resize { cluster, option } => {
                let note = self.cost_note(&cluster, |c| {
                    (vec![c.node_type.clone(), option.node_type.clone()], format!("Resized {} from {} to {}", c.id, c.shape(), option.shape()))
                });
                ctx.spawn(move || Msg::Done {
                    result: redshift::resize(&aws, &cluster, &option.node_type, option.nodes, false).inspect(|()| note.iter().for_each(|n| n.record())),
                    what: format!("{cluster} is resizing to {}", option.shape()),
                    cluster,
                })
            }
            Action::Cancel { cluster, query } => {
                let Some(identity) = self.identity(&cluster).cloned() else { return };
                let pid = query.pid.unwrap_or_default();
                ctx.spawn(move || Msg::Done {
                    result: redshift::cancel_query(&aws, &cluster, &identity, &query),
                    what: format!("Cancelled the query of session {pid}"),
                    cluster,
                });
            }
        }
    }

    /// What to record in the savings ledger once an action on `cluster` that changes the
    /// bill is made: its node types and a description, from `what`; nothing when the cluster
    /// is no longer listed (there would be nothing to verify it by).
    fn cost_note(&self, cluster: &str, what: impl Fn(&Cluster) -> (Vec<String>, String)) -> Option<crate::costs::ledger::Note> {
        let (types, text) = self.clusters.iter().find(|c| c.id == cluster).map(what)?;
        let mut usage: Vec<String> = Vec::new();
        for t in types {
            for u in crate::costs::ledger::usage_of_instance("Redshift", &t) {
                if !usage.contains(&u) {
                    usage.push(u);
                }
            }
        }
        Some(crate::costs::ledger::Note::new(&self.env_name, &self.region, "Redshift", cluster, usage, text))
    }

    fn on_done(&mut self, cluster: String, what: String, result: aws::Result<()>, ctx: &mut Ctx) {
        match result {
            Ok(()) => {
                self.dialog = None;
                ctx.toast(Level::Success, what);
                self.activity.remove(&cluster);
                self.snapshots.remove(&cluster);
                self.load_clusters(ctx);
                self.fetch_tab(ctx, true);
            }
            Err(e) => match &mut self.dialog {
                Some(Dialog::Confirm { busy, error, .. }) => {
                    *busy = false;
                    *error = Some(e.short());
                }
                Some(Dialog::Snapshot { form, busy, .. }) => {
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
            Dialog::Identity { form, .. } => match form.handle(key) {
                FormEvent::Pending => {}
                FormEvent::Cancel => self.dialog = None,
                FormEvent::Submit => self.submit_identity(ctx),
            },
            Dialog::Secrets(picker, cluster, database, arns) => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.dialog = None,
                Outcome::Done(chosen) => {
                    let Some(arn) = chosen.first().and_then(|i| arns.get(*i)).cloned() else { return };
                    let (cluster, database) = (cluster.clone(), database.clone());
                    self.save_identity(&cluster, Identity { database, db_user: None, secret_arn: Some(arn) }, ctx);
                }
            },
            Dialog::Resize(picker, cluster, options) => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.dialog = None,
                Outcome::Done(chosen) => {
                    let Some(option) = chosen.first().and_then(|i| options.get(*i)).cloned() else { return };
                    let Some(c) = self.clusters.iter().find(|c| c.id == *cluster).cloned() else { return };
                    let disk = option.disk_percent.map(|d| format!("Estimated disk use after: {d:.0}%.")).unwrap_or_default();
                    self.confirm(
                        format!("Resize {}", c.id),
                        vec![
                            format!("Resize {} from {} to {}?", c.id, c.shape(), option.shape()),
                            "Elastic resize: it takes minutes, during which the cluster is unavailable for a short while; open connections drop and queries in flight fail.".into(),
                            disk,
                        ],
                        Action::Resize { cluster: c.id, option },
                    );
                }
            },
            Dialog::Snapshot { form, cluster, busy } => {
                if *busy {
                    return;
                }
                match form.handle(key) {
                    FormEvent::Pending => {}
                    FormEvent::Cancel => self.dialog = None,
                    FormEvent::Submit => {
                        let name = form.get("Name").trim().to_string();
                        if !redshift::valid_snapshot_name(&name) {
                            return form.fail("Use lower-case letters, digits and single hyphens, starting with a letter");
                        }
                        *busy = true;
                        let (aws, cluster) = (ctx.env().aws.clone(), cluster.clone());
                        ctx.spawn(move || Msg::Done { result: redshift::create_snapshot(&aws, &cluster, &name), what: format!("Taking the snapshot {name}"), cluster });
                    }
                }
            }
            Dialog::Confirm { busy: true, .. } => {}
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

    // ---- copying -----------------------------------------------------------------------------

    fn copy_endpoint(&mut self, ctx: &mut Ctx) {
        if self.tab == Tab::Queries
            && self.open.is_some()
            && let Some((q, _)) = self.selected_query()
        {
            return copy(ctx, "query", &q.text, "the query");
        }
        match self.target().and_then(|c| Some((c.endpoint.clone()?, c.port.unwrap_or(5439)))) {
            Some((host, port)) => copy(ctx, "endpoint", &format!("{host}:{port}"), "the endpoint"),
            None => ctx.toast(Level::Info, "No endpoint to copy (a paused cluster has none)"),
        }
    }

    fn copy_connect(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.target().cloned() else { return };
        if c.endpoint.is_none() {
            return ctx.toast(Level::Info, "No endpoint to connect to (a paused cluster has none)");
        }
        self.load_identity(&c.id);
        let identity = self.identity(&c.id);
        let user = identity.and_then(|i| i.db_user.clone());
        let database = identity.map(|i| i.database.clone());
        let command = redshift::connect_command(&c, user.as_deref(), database.as_deref(), &self.region, &self.profile);
        copy(ctx, "connect", &command, "the connection command (temporary password at run time)");
    }

    fn copy_report(&mut self, ctx: &mut Ctx) {
        let Some(id) = self.target().map(|c| c.id.clone()) else { return };
        if !self.in_flight.insert(format!("report {id}")) {
            return;
        }
        self.load_identity(&id);
        let identity = self.identity(&id).cloned();
        ctx.toast(Level::Info, format!("Writing the performance diagnosis of {id} (last {})…", self.range.label()));
        let (aws, env, region, range) = (ctx.env().aws.clone(), self.env_name.clone(), self.region.clone(), self.range);
        ctx.spawn(move || Msg::Report { result: redshift::gather_report(&aws, &env, &region, &id, identity.as_ref(), range, now_secs()), cluster: id });
    }

    fn open_console(&self, ctx: &mut Ctx) {
        if let Some(c) = self.target() {
            let url = redshift::console_url(&self.region, &c.id);
            if !crate::platform::open_url(&url) {
                ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
            }
        }
    }

    fn zoom_query(&mut self) {
        let Some((q, _)) = self.selected_query() else { return };
        let mut view = LogView::default();
        view.extend(q.text.lines().map(str::to_string));
        view.zoomed = true;
        view.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        let who = q.pid.map(|p| format!("session {p}")).unwrap_or_else(|| "query".into());
        self.zoom = Some((format!("{who} · {} · {}", q.user, q.status), view));
    }

    // ---- rendering ---------------------------------------------------------------------------

    fn cluster_health(&self, c: &Cluster) -> redshift::Health {
        let by_status = redshift::status_health(&c.status);
        if by_status != redshift::Health::Ok {
            return by_status;
        }
        let Some(p) = self.pulse() else { return by_status };
        let worst = c
            .nodes
            .iter()
            .map(|n| {
                let key = redshift::node_key(&c.id, n);
                redshift::use_health(p.last(&key, "cpu"), if n.leader() { None } else { p.last(&key, "disk") })
            })
            .max()
            .unwrap_or(redshift::Health::Ok);
        if p.last(&c.id, "health").is_some_and(|h| h < 0.5) { redshift::Health::Failing } else { worst }
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
            LoadState::Loading => format!(" {} Redshift clusters ", theme::spinner(view.tick)),
            _ => format!(" Redshift clusters · {} ", self.rows.len()),
        };
        if self.rows.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading | LoadState::Idle => Line::styled("Loading clusters…", theme::muted()),
                LoadState::Loaded if !self.filter.value.is_empty() => Line::styled("No cluster matches", theme::muted()),
                LoadState::Loaded => Line::styled(format!("No provisioned Redshift clusters in {}", self.region), theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let name_w = self.clusters.iter().map(|c| c.id.chars().count()).max().unwrap_or(10).clamp(10, 32);
        let shape_w = self.clusters.iter().map(|c| c.shape().chars().count()).max().unwrap_or(10).min(24);
        let pulse = self.pulse();
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|&i| {
                let c = &self.clusters[i];
                let health = self.cluster_health(c);
                let mut spans = vec![
                    Span::styled("● ", Style::new().fg(health_color(health))),
                    Span::styled(format!("{:<name_w$}", ellipsize(&c.id, name_w)), Style::new().bold()),
                    Span::raw(format!("  {:<shape_w$}", ellipsize(&c.shape(), shape_w))),
                ];
                if c.status != "available" {
                    spans.push(Span::styled(format!("  {}", c.status), Style::new().fg(health_color(redshift::status_health(&c.status)))));
                }
                if let (Some(p), false) = (pulse, c.paused()) {
                    let cpu = p.last(&c.id, "cpu_all");
                    let spark = sparkline_to(&columns(p.points(&c.id, "cpu_all"), p.window, 8, Agg::Mean), 100.0);
                    spans.push(Span::styled("  CPU ", theme::muted()));
                    spans.push(Span::styled(
                        format!("{spark} {:>4}", cpu.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into())),
                        Style::new().fg(health_color(redshift::use_health(cpu, None))),
                    ));
                    if let Some(d) = p.last(&c.id, "disk_all") {
                        spans.push(Span::styled("  disk ", theme::muted()));
                        spans.push(Span::styled(format!("{d:.0}%"), Style::new().fg(health_color(redshift::use_health(None, Some(d))))));
                    }
                    if let Some(n) = p.last(&c.id, "connections") {
                        spans.push(Span::styled(format!("  {n:.0} conn"), theme::muted()));
                    }
                    if let Some(q) = p.last(&c.id, "queued").filter(|q| *q >= 1.0) {
                        spans.push(Span::styled(format!("  {q:.0} queued"), Style::new().fg(theme::WARN)));
                    }
                }
                if let Some(note) = redshift::status_note(c) {
                    spans.push(Span::styled(format!("  {note}"), Style::new().fg(theme::RUNNING)));
                }
                if c.publicly_accessible {
                    spans.push(Span::styled("  ⚠ public", Style::new().fg(theme::WARN)));
                }
                if !c.encrypted {
                    spans.push(Span::styled("  ⚠ unencrypted", Style::new().fg(theme::WARN)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        // The selected cluster's nodes below the list, when there is room.
        let list_h = (self.rows.len() as u16 + 2).max(5);
        let preview = self.list_cluster().cloned().filter(|c| list_area.height >= list_h + c.nodes.len() as u16 + 5);
        let [list_area, preview_area] = match preview {
            Some(_) => list_area.layout(&Layout::vertical([Constraint::Length(list_h), Constraint::Fill(1)])),
            None => [list_area, Rect::default()],
        };
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
        if let Some(c) = preview {
            let t = Nodes { cluster: &c, pulse: self.pulse(), selected: usize::MAX };
            let block = widgets::panel(Line::from(vec![Span::raw(format!(" {} ", c.id)), Span::styled("· Enter opens ", theme::muted())]), false);
            let inner = block.inner(preview_area);
            f.render_widget(block, preview_area);
            let (mut lines, _) = nodes::list(&t, inner.width as usize);
            lines.push(Line::raw(""));
            lines.push(nodes::summary(&t));
            f.render_widget(Paragraph::new(Text::from(lines)), inner);
        }
    }

    fn tabs_line(&self, c: &Cluster) -> Line<'static> {
        let mut spans = Vec::new();
        for tab in TABS {
            let extra = if tab.sql() && self.identity(&c.id).is_none() { " ·" } else { "" };
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
        let mut about = format!("{} · version {} · {} {}", c.shape(), c.version.clone().unwrap_or_default(), self.env_name, self.region);
        if let Some(note) = redshift::status_note(&c) {
            about.push_str(&format!(" · {note}"));
        }
        f.render_widget(Paragraph::new(Line::styled(ellipsize(&about, head.width as usize), theme::muted())), head);
        f.render_widget(Paragraph::new(self.tabs_line(&c)), tabs);
        let width = body.width as usize;
        match self.tab {
            Tab::Nodes => self.render_nodes(f, body, &c),
            Tab::Performance => {
                let node_count = c.nodes.len();
                let Some(node) = c.nodes.get(self.node.min(node_count.saturating_sub(1))).cloned() else { return };
                let [bar, rest] = body.layout(&Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]));
                f.render_widget(Paragraph::new(views::node_bar(&node, self.node, node_count, self.range)), bar);
                let lines = views::performance(&c, &node, self.metrics.get(&(c.id.clone(), self.range)).map(|(_, r)| r), width, view.tick);
                f.render_widget(Paragraph::new(Text::from(lines)).scroll((self.scroll, 0)), rest);
            }
            Tab::Queries => {
                let identity = self.identity(&c.id).cloned();
                let data = self.activity.get(&c.id).map(|(_, a)| a);
                let (lines, rows) = views::queries(&c, identity.as_ref(), data, self.query_cursor, width, view.now, view.tick);
                if let Some(&at) = self.query_cursor.and_then(|n| rows.get(n)) {
                    self.keep_visible(at as u16, body.height);
                }
                f.render_widget(Paragraph::new(Text::from(lines)).scroll((self.scroll, 0)), body);
            }
            Tab::Tables => {
                let identity = self.identity(&c.id).cloned();
                let lines = views::tables(&c, identity.as_ref(), self.tables.get(&c.id).map(|(_, r)| r), self.sort, width, view.tick);
                f.render_widget(Paragraph::new(Text::from(lines)).scroll((self.scroll, 0)), body);
            }
            Tab::Events => {
                let lines = views::events(&c, self.events.as_ref().map(|(_, r)| r), width, view.now, view.tick);
                f.render_widget(Paragraph::new(Text::from(lines)).scroll((self.scroll, 0)), body);
            }
            Tab::Config => {
                let identity = self.identity(&c.id).cloned();
                let lines = views::config(&c, self.wlm.get(&c.id), self.snapshots.get(&c.id).map(|(_, r)| r), identity.as_ref(), view.tick);
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

    fn render_nodes(&mut self, f: &mut Frame, body: Rect, c: &Cluster) {
        let t = Nodes { cluster: c, pulse: self.pulse.as_ref().and_then(|(_, r)| r.as_ref().ok()), selected: self.node };
        let side = body.width >= 100 + SIDE_W;
        let diagram_w = if side { body.width - SIDE_W - 2 } else { body.width };
        let drawn = nodes::diagram(&t, diagram_w as usize);
        let (mut lines, item_rows, hits) = match &drawn {
            Some((canvas, hits)) => (canvas.lines(0, 0, canvas.w, canvas.h), hits.iter().map(|h| (h.item, h.y)).collect::<Vec<_>>(), hits.clone()),
            None => {
                let (lines, rows) = nodes::list(&t, diagram_w as usize);
                (lines, rows.into_iter().enumerate().collect(), Vec::new())
            }
        };
        lines.push(Line::raw(""));
        lines.push(nodes::summary(&t));
        if c.paused() {
            lines.push(Line::styled("Paused: p resumes it.", theme::muted()));
        }
        let details = nodes::details(&t, if side { SIDE_W as usize - 2 } else { body.width as usize });
        let diagram_h = lines.len();
        if !side {
            lines.push(Line::raw(""));
            lines.extend(details.clone());
        }
        if let Some(&(_, row)) = item_rows.iter().find(|(item, _)| *item == self.node) {
            self.keep_visible(row as u16, body.height.min(diagram_h as u16).max(1));
        }
        let [left, right] = if side { body.layout(&Layout::horizontal([Constraint::Length(diagram_w), Constraint::Fill(1)])) } else { [body, Rect::default()] };
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

    fn render_dialog(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let danger = if self.typed_name().is_some() { theme::ERR } else { theme::ACCENT };
        match &mut self.dialog {
            None => {}
            Some(Dialog::Identity { form, .. }) => form.render(f, area, theme::ACCENT),
            Some(Dialog::Secrets(picker, ..) | Dialog::Resize(picker, ..)) => {
                let rect = widgets::popup_area(area, 80, picker.height(area.height.saturating_sub(4)));
                picker.render(f, rect, theme::ACCENT);
            }
            Some(Dialog::Snapshot { form, busy, .. }) => {
                form.render(f, area, danger);
                if *busy {
                    let body = Text::from(vec![
                        Line::raw(""),
                        Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw("Taking the snapshot…")]),
                    ]);
                    widgets::popup(f, widgets::popup_area(area, 34, 5), "Redshift", theme::ACCENT, body);
                }
            }
            Some(Dialog::Confirm { title, lines, typed, busy, error, .. }) => {
                let mut text: Vec<Line> = vec![Line::raw("")];
                text.extend(lines.iter().filter(|l| !l.is_empty()).map(|l| Line::raw(l.clone())));
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
                    text.push(Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw("Working…")]));
                } else {
                    text.push(widgets::hints_line(&[("Enter", "confirm"), ("Esc", "cancel")]));
                }
                let color = if typed.is_some() { theme::ERR } else { theme::ACCENT };
                let rect = widgets::popup_area(area, 84, widgets::wrapped_height(&text, 80) + 2);
                widgets::popup(f, rect, title, color, Text::from(text));
                if let Some((row, col)) = cursor {
                    f.set_cursor_position((rect.x + 2 + col, rect.y + 1 + row));
                }
            }
        }
    }
}

impl Module for Redshift {
    fn title(&self) -> &'static str {
        "Redshift"
    }

    fn description(&self) -> &'static str {
        "Redshift clusters · nodes, queries, WLM queue, table health"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Redshift::new();
        let env = ctx.env();
        self.env_name = env.name.clone();
        self.region = env.region.clone();
        self.profile = env.profile.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) || self.loaded_at.elapsed() >= LIST_FRESH {
            self.load_clusters(ctx);
        } else if self.pulse.as_ref().is_none_or(|(at, _)| at.elapsed() >= PULSE_EVERY) {
            self.fetch_pulse(ctx);
        }
        self.fetch_tab(ctx, false);
    }

    /// Shows the cluster `target` (its identifier).
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        self.zoom = None;
        self.dialog = None;
        self.pending_open = Some(target.to_string());
        if matches!(self.load, LoadState::Loaded) && !self.try_open(ctx) {
            self.pending_open = None;
            ctx.toast(Level::Warn, format!("Redshift cluster not found here: {target}"));
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
                copy(ctx, "query", &text, "the query");
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
            KeyCode::Char('y') => self.copy_endpoint(ctx),
            KeyCode::Char('Y') => self.copy_connect(ctx),
            KeyCode::Char('c') => self.copy_report(ctx),
            KeyCode::Char('o') => self.open_console(ctx),
            KeyCode::Char('p') => self.start_pause(ctx),
            KeyCode::Char('R') => self.start_resize(ctx),
            KeyCode::Char('s') => self.start_snapshot(ctx),
            KeyCode::Char('i') => {
                if let Some(c) = self.target().cloned() {
                    self.load_identity(&c.id);
                    self.ask_identity(&c);
                }
            }
            KeyCode::Char('r') => {
                self.load_clusters(ctx);
                self.fetch_pulse(ctx);
                self.fetch_tab(ctx, true);
            }
            _ => {
                return if self.open.is_none() { self.list_key(key, ctx) } else { self.cluster_key(key, ctx) };
            }
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Handled {
        if self.zoom.is_some() || self.dialog.is_some() {
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
                self.node = item;
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Clusters(result) => {
                self.in_flight.remove("clusters");
                match result {
                    Ok(clusters) => {
                        let keep = self.list_cluster().map(|c| c.id.clone());
                        self.clusters = clusters;
                        self.load = LoadState::Loaded;
                        self.rebuild(keep, true);
                        if self.open.is_some() && self.cluster().is_none() {
                            self.open = None;
                        }
                        self.node = self.node.min(self.cluster().map_or(0, |c| c.nodes.len().saturating_sub(1)));
                        self.try_open(ctx);
                        self.fetch_pulse(ctx);
                    }
                    Err(e) => {
                        if self.clusters.is_empty() {
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
            Msg::Activity { cluster, activity } => {
                self.in_flight.remove(&format!("activity {cluster}"));
                // The selected query stays selected wherever the new list puts it, or nothing is.
                if self.open.as_deref() == Some(cluster.as_str()) {
                    let keep = self.query_cursor.zip(self.activity.get(&cluster)).and_then(|(n, (_, old))| views::query_key(old, n));
                    self.query_cursor = keep.and_then(|k| views::find_query(&activity, &k));
                }
                self.activity.insert(cluster, (Instant::now(), *activity));
            }
            Msg::Tables { cluster, result } => {
                self.in_flight.remove(&format!("tables {cluster}"));
                self.tables.insert(cluster, (Instant::now(), result));
            }
            Msg::Events(result) => {
                self.in_flight.remove("events");
                self.events = Some((Instant::now(), result));
            }
            Msg::Snapshots { cluster, result } => {
                self.in_flight.remove(&format!("snapshots {cluster}"));
                self.snapshots.insert(cluster, (Instant::now(), result));
            }
            Msg::Wlm { cluster, result } => {
                self.in_flight.remove(&format!("wlm {cluster}"));
                self.wlm.insert(cluster, result);
            }
            Msg::Secrets { cluster, database, result } => self.on_secrets(cluster, database, result, ctx),
            Msg::Options { cluster, result } => self.on_options(cluster, result, ctx),
            Msg::Done { cluster, what, result } => self.on_done(cluster, what, result, ctx),
            Msg::Report { cluster, result } => {
                self.in_flight.remove(&format!("report {cluster}"));
                match result {
                    Ok(md) => copy(ctx, &format!("{cluster}-diagnosis"), &md, &format!("the performance diagnosis of {cluster}")),
                    Err(e) => ctx.toast(Level::Error, format!("Could not write the diagnosis: {}", e.short())),
                }
            }
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if !active || !matches!(self.load, LoadState::Loaded) {
            return;
        }
        if self.pulse.as_ref().is_some_and(|(at, _)| at.elapsed() >= PULSE_EVERY) {
            self.fetch_pulse(ctx);
        }
        let changing = self.clusters.iter().any(|c| redshift::status_health(&c.status) == redshift::Health::Changing || c.status == "pausing");
        if changing && self.loaded_at.elapsed() >= CHANGING_EVERY {
            self.load_clusters(ctx);
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
            Some(Dialog::Identity { .. } | Dialog::Snapshot { .. }) => return vec![("Tab", "next field"), ("Enter", "save"), ("Esc", "cancel")],
            Some(Dialog::Secrets(..) | Dialog::Resize(..)) => return vec![("↑↓", "move"), ("Enter", "choose"), ("Esc", "cancel")],
            Some(Dialog::Confirm { .. }) => return vec![("Enter", "confirm"), ("Esc", "cancel")],
            None => {}
        }
        if self.zoom.is_some() {
            return vec![("/", "search"), ("y", "copy"), ("Esc", "back")];
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        let pause = match self.target().map(|c| c.status.as_str()) {
            Some("paused") => "resume",
            _ => "pause",
        };
        if self.open.is_none() {
            return vec![
                ("↑↓", "cluster"),
                ("Enter", "open"),
                ("p", pause),
                ("R", "resize"),
                ("s", "snapshot"),
                ("y", "copy endpoint"),
                ("Y", "copy connect command"),
                ("c", "copy diagnosis for AI"),
                ("/", "search"),
                ("o", "console"),
                ("r", "reload"),
            ];
        }
        let mut hints = vec![("←→", "view")];
        match self.tab {
            Tab::Nodes => hints.extend([("↑↓", "node"), ("Enter", "node metrics")]),
            Tab::Performance => hints.extend([("↑↓", "node"), ("m", "range")]),
            Tab::Queries if self.query_cursor.is_some() => hints.extend([("↑↓", "query"), ("Enter", "whole query"), ("x", "cancel query"), ("y", "copy query")]),
            Tab::Queries => hints.extend([("Enter", "select queries"), ("i", "identity")]),
            Tab::Tables => hints.extend([("t", "sort"), ("i", "identity"), ("↑↓", "scroll")]),
            Tab::Events | Tab::Config => hints.extend([("↑↓", "scroll")]),
        }
        hints.extend([("p", pause), ("R", "resize"), ("s", "snapshot"), ("Y", "copy connect"), ("c", "diagnosis for AI"), ("r", "reload"), ("Esc", "clusters")]);
        hints
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || !self.in_flight.is_empty()
            || matches!(self.dialog, Some(Dialog::Confirm { busy: true, .. } | Dialog::Snapshot { busy: true, .. }))
    }

    fn capturing(&self) -> bool {
        self.filtering || self.zoom.is_some() || self.dialog.is_some()
    }

    fn wants_mouse(&self) -> bool {
        self.zoom.is_none()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.dialog {
            Some(Dialog::Identity { form, .. } | Dialog::Snapshot { form, busy: false, .. }) => form.paste(text),
            Some(Dialog::Secrets(picker, ..) | Dialog::Resize(picker, ..)) => picker.paste(text),
            Some(Dialog::Confirm { typed: Some((_, input)), busy: false, .. }) => input.insert_str(text, false),
            Some(_) => {}
            None if self.filtering => {
                self.filter.insert_str(text, false);
                self.refilter();
            }
            None => {}
        }
    }
}

impl Redshift {
    fn list_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
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
        Handled::Yes
    }

    fn cluster_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => {
                if self.query_cursor.is_some() {
                    self.query_cursor = None;
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
                self.fetch_tab(ctx, false);
            }
            KeyCode::Char('t') if self.tab == Tab::Tables => {
                self.sort = self.sort.next();
                self.scroll = 0;
            }
            KeyCode::Char('x') if self.tab == Tab::Queries => self.start_cancel(ctx),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                let delta = if matches!(key.code, KeyCode::Up | KeyCode::Char('k')) { -1 } else { 1 };
                match self.tab {
                    Tab::Nodes => self.move_node(delta, false),
                    Tab::Performance => self.move_node(delta, true),
                    Tab::Queries if self.query_cursor.is_some() => {
                        let count = self.listed_queries().len();
                        self.query_cursor = self.query_cursor.map(|n| (n as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize);
                    }
                    _ => self.scroll = (self.scroll as isize + delta).max(0) as u16,
                }
            }
            KeyCode::Enter => match self.tab {
                Tab::Nodes => self.set_tab(Tab::Performance, ctx),
                Tab::Queries if self.query_cursor.is_some() => self.zoom_query(),
                Tab::Queries if !self.listed_queries().is_empty() => self.query_cursor = Some(0),
                _ => {}
            },
            _ => return Handled::No,
        }
        Handled::Yes
    }
}

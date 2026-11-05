//! EKS: the region's clusters with their Kubernetes version and support (extended support costs
//! about six times the standard cluster-hour); a cluster's overview, node groups, add-ons (and
//! newer versions), upgrade insights, who has access, and — read inside the cluster — the
//! deployments and pods that are not well, with a pod's warning events and logs. `K` opens k9s
//! on the cluster with a kubeconfig whose tokens come from dcli; `c` copies Markdown for an AI
//! agent. Read only: changes inside the cluster are k9s's or kubectl's job.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::eks::{self, Cluster, ClusterVersion, Detail, Support};
use crate::aws::{self, AwsError, k8s, sts};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, now_secs};

const DEBOUNCE: Duration = Duration::from_millis(200);
/// Failing pods the AI report reads events and logs of, and the log lines it keeps.
const REPORT_PODS: usize = 5;
const REPORT_LINES: usize = 40;
/// Log lines read for a pod.
const POD_LINES: usize = 500;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Overview,
    Nodes,
    Addons,
    Insights,
    Access,
    Workloads,
}

const TABS: [Tab; 6] = [Tab::Overview, Tab::Nodes, Tab::Addons, Tab::Insights, Tab::Access, Tab::Workloads];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Nodes => "Node groups",
            Tab::Addons => "Add-ons",
            Tab::Insights => "Insights",
            Tab::Access => "Access",
            Tab::Workloads => "Workloads",
        }
    }
}

enum Msg {
    Overview(aws::Result<eks::Overview>),
    Detail { cluster: String, result: aws::Result<Detail> },
    Workloads { cluster: String, result: Result<k8s::Workloads, String> },
    /// A pod's (or deployment's) warning events and a container's logs, for the zoomed view.
    Look { cluster: String, title: String, lines: Vec<String> },
    Report { cluster: String, result: Result<String, String> },
}

/// A row of the Workloads tab.
#[derive(Clone, Copy, PartialEq)]
enum Work {
    Deployment(usize),
    Pod(usize),
}

/// What the zoomed view shows: a pod's events and logs (`p` switches between the current and
/// the previous run), or a deployment's.
struct Zoom {
    title: String,
    view: LogView,
    pod: Option<PodRef>,
    /// What it shows has been read.
    ready: bool,
}

impl Zoom {
    fn reading(title: String, pod: Option<PodRef>) -> Zoom {
        Zoom { title, view: LogView::default(), pod, ready: false }
    }
}

#[derive(Clone)]
struct PodRef {
    namespace: String,
    name: String,
    container: Option<String>,
    previous: bool,
}

pub struct Eks {
    clusters: Vec<Cluster>,
    versions: Vec<ClusterVersion>,
    load: LoadState,
    filter: TextInput,
    filtering: bool,
    rows: Vec<usize>,
    list_state: ListState,
    tab: Tab,
    focused: bool,
    cursor: usize,
    scroll: u16,
    details: HashMap<String, aws::Result<Detail>>,
    workloads: HashMap<String, Result<k8s::Workloads, String>>,
    in_flight: HashSet<(String, &'static str)>,
    pending: Option<(String, Instant)>,
    /// Insights shown open (by id).
    expanded: HashSet<String>,
    /// Workloads: every deployment, not only those not healthy.
    show_all: bool,
    zoom: Option<Zoom>,
    pending_open: Option<String>,
    env_name: String,
    region: String,
    config_file: Option<PathBuf>,
    list_area: Rect,
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

/// Why the inside of `cluster` could not be read, with what to do.
fn explain(aws: &aws::Aws, cluster: &Cluster, e: &AwsError) -> String {
    let principal = matches!(e, AwsError::Api { status: 401 | 403, .. }).then(|| sts::caller_principal(aws).ok()).flatten();
    k8s::explain(e, cluster, principal.as_deref())
}

fn mark(ok: bool, warn: bool) -> (&'static str, Style) {
    if !ok {
        ("✗", Style::new().fg(theme::ERR))
    } else if warn {
        ("!", Style::new().fg(theme::WARN))
    } else {
        ("✓", Style::new().fg(theme::OK))
    }
}

impl Eks {
    pub fn new() -> Eks {
        Eks {
            clusters: Vec::new(),
            versions: Vec::new(),
            load: LoadState::Idle,
            filter: TextInput::default(),
            filtering: false,
            rows: Vec::new(),
            list_state: ListState::default(),
            tab: Tab::Overview,
            focused: false,
            cursor: 0,
            scroll: 0,
            details: HashMap::new(),
            workloads: HashMap::new(),
            in_flight: HashSet::new(),
            pending: None,
            expanded: HashSet::new(),
            show_all: false,
            zoom: None,
            pending_open: None,
            env_name: String::new(),
            region: String::new(),
            config_file: None,
            list_area: Rect::default(),
        }
    }

    // ---- the list ----------------------------------------------------------------------

    fn load_clusters(&mut self, ctx: &mut Ctx) {
        self.load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Overview(eks::overview(&aws)));
    }

    fn refilter(&mut self) {
        let keep = self.selected().map(|c| c.name.clone());
        let query = self.filter.value.trim().to_lowercase();
        self.rows = (0..self.clusters.len()).filter(|&i| matches_filter(&self.clusters[i].name, &query)).collect();
        let index = keep.and_then(|k| self.rows.iter().position(|&i| self.clusters[i].name == k)).unwrap_or(0);
        self.list_state.select(if self.rows.is_empty() { None } else { Some(index) });
    }

    fn selected(&self) -> Option<&Cluster> {
        self.list_state.selected().and_then(|i| self.rows.get(i)).map(|&i| &self.clusters[i])
    }

    fn select(&mut self, index: usize) {
        if self.rows.is_empty() {
            return;
        }
        let index = index.min(self.rows.len() - 1);
        let changed = self.list_state.selected() != Some(index);
        self.list_state.select(Some(index));
        if changed {
            self.cursor = 0;
            self.scroll = 0;
            self.focused = false;
        }
        if let Some(c) = self.selected() {
            self.pending = Some((c.name.clone(), Instant::now()));
        }
    }

    fn try_open(&mut self) -> bool {
        let Some(target) = self.pending_open.clone() else { return false };
        let Some(i) = self.rows.iter().position(|&i| self.clusters[i].name == target) else { return false };
        self.pending_open = None;
        self.select(i);
        true
    }

    fn detail(&self) -> Option<&Detail> {
        self.details.get(&self.selected()?.name)?.as_ref().ok()
    }

    /// The selected cluster as last described (the detail's, else the list's).
    fn cluster(&self) -> Option<Cluster> {
        self.detail().map(|d| d.cluster.clone()).or_else(|| self.selected().cloned())
    }

    fn load_detail(&mut self, name: &str, ctx: &mut Ctx) {
        if !self.in_flight.insert((name.to_string(), "detail")) {
            return;
        }
        let aws = ctx.env().aws.clone();
        let versions = self.versions.clone();
        let cluster = name.to_string();
        ctx.spawn(move || Msg::Detail { result: eks::detail(&aws, &cluster, &versions), cluster });
    }

    fn load_workloads(&mut self, cluster: Cluster, ctx: &mut Ctx) {
        if !self.in_flight.insert((cluster.name.clone(), "workloads")) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = k8s::Client::new(aws.clone(), &cluster).and_then(|c| k8s::workloads(&c)).map_err(|e| explain(&aws, &cluster, &e));
            Msg::Workloads { cluster: cluster.name.clone(), result }
        });
    }

    /// What the current tab needs, if not read yet.
    fn load_tab(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.selected().cloned() else { return };
        if !self.details.contains_key(&c.name) {
            self.load_detail(&c.name, ctx);
        }
        if self.tab == Tab::Workloads && !self.workloads.contains_key(&c.name) {
            let cluster = self.cluster().unwrap_or(c);
            self.load_workloads(cluster, ctx);
        }
    }

    fn set_tab(&mut self, tab: Tab, ctx: &mut Ctx) {
        self.tab = tab;
        self.cursor = 0;
        self.scroll = 0;
        self.load_tab(ctx);
        if self.row_count() == 0 {
            self.focused = false;
        }
    }

    fn step_tab(&mut self, forward: bool, ctx: &mut Ctx) {
        let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
        let next = if forward { (i + 1) % TABS.len() } else { (i + TABS.len() - 1) % TABS.len() };
        self.set_tab(TABS[next], ctx);
    }

    // ---- rows of the tabs ----------------------------------------------------------------

    fn work_rows(&self) -> Vec<Work> {
        let Some(Ok(w)) = self.selected().and_then(|c| self.workloads.get(&c.name)) else { return Vec::new() };
        let mut rows: Vec<Work> =
            w.deployments.iter().enumerate().filter(|(_, d)| self.show_all || d.problem().is_some()).map(|(i, _)| Work::Deployment(i)).collect();
        rows.extend((0..w.failing.len()).map(Work::Pod));
        rows
    }

    fn row_count(&self) -> usize {
        let Some(d) = self.detail() else { return 0 };
        match self.tab {
            Tab::Overview => 0,
            Tab::Nodes => d.nodegroups.as_ref().map_or(0, Vec::len),
            Tab::Addons => d.addons.as_ref().map_or(0, Vec::len),
            Tab::Insights => d.insights.as_ref().map_or(0, Vec::len),
            Tab::Access => d.access.as_ref().ok().and_then(|a| a.as_ref()).map_or(0, Vec::len),
            Tab::Workloads => self.work_rows().len(),
        }
    }

    fn activate_row(&mut self, ctx: &mut Ctx) {
        match self.tab {
            Tab::Insights => {
                let id = self.detail().and_then(|d| d.insights.as_ref().ok()?.get(self.cursor).map(|i| i.id.clone()));
                if let Some(id) = id
                    && !self.expanded.remove(&id)
                {
                    self.expanded.insert(id);
                }
            }
            Tab::Workloads => {
                let Some(row) = self.work_rows().get(self.cursor).copied() else { return };
                let Some(cluster) = self.cluster() else { return };
                let Some(Ok(w)) = self.workloads.get(&cluster.name) else { return };
                match row {
                    Work::Pod(i) => {
                        let (pod, why) = &w.failing[i];
                        let target = PodRef {
                            namespace: pod.namespace.clone(),
                            name: pod.name.clone(),
                            container: why.container.clone(),
                            previous: why.previous,
                        };
                        let head = vec![format!("{} · {}", why.reason, why.detail)];
                        self.look_pod(cluster, target, head, ctx);
                    }
                    Work::Deployment(i) => {
                        let d = w.deployments[i].clone();
                        let mut head = vec![format!(
                            "{}/{} · {}/{} ready · {} updated",
                            d.namespace,
                            d.name,
                            d.ready,
                            d.desired,
                            d.updated
                        )];
                        head.extend(d.conditions.iter().map(|c| {
                            let why = [c.reason.as_deref(), c.message.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(": ");
                            format!("{} {} {why}", c.kind, c.status)
                        }));
                        let pods: Vec<String> = w
                            .failing
                            .iter()
                            .filter(|(p, _)| d.owns(p))
                            .map(|(p, why)| format!("  ✗ {} — {}: {}", p.name, why.reason, why.detail))
                            .collect();
                        if !pods.is_empty() {
                            head.push(String::new());
                            head.push("Its pods with a problem (Enter on one in the list shows its events and logs):".into());
                            head.extend(pods);
                        }
                        let aws = ctx.env().aws.clone();
                        let title = format!("{}/{} · deployment", d.namespace, d.name);
                        let name = cluster.name.clone();
                        self.zoom = Some(Zoom::reading(title.clone(), None));
                        ctx.spawn(move || {
                            let mut lines = head;
                            lines.push(String::new());
                            lines.push("── Warning events ──".into());
                            match k8s::Client::new(aws.clone(), &cluster).and_then(|c| c.events(&d.namespace, &d.name)) {
                                Ok(e) if e.is_empty() => lines.push("none".into()),
                                Ok(e) => lines.extend(e.iter().map(k8s::Event::line)),
                                Err(e) => lines.push(explain(&aws, &cluster, &e)),
                            }
                            Msg::Look { cluster: name, title, lines }
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// Opens the zoomed view of a pod: `head`, its warning events, and the logs of `target`'s
    /// container (of its previous run when `target.previous`).
    fn look_pod(&mut self, cluster: Cluster, target: PodRef, head: Vec<String>, ctx: &mut Ctx) {
        let which = if target.previous { "previous run" } else { "current run" };
        let title = format!("{}/{} · {which}", target.namespace, target.name);
        self.zoom = Some(Zoom::reading(title.clone(), Some(target.clone())));
        let aws = ctx.env().aws.clone();
        let name = cluster.name.clone();
        ctx.spawn(move || {
            let mut lines = head;
            match k8s::Client::new(aws.clone(), &cluster) {
                Err(e) => lines.push(explain(&aws, &cluster, &e)),
                Ok(client) => {
                    lines.push(String::new());
                    lines.push("── Warning events ──".into());
                    match client.events(&target.namespace, &target.name) {
                        Ok(e) if e.is_empty() => lines.push("none".into()),
                        Ok(e) => lines.extend(e.iter().map(k8s::Event::line)),
                        Err(e) => lines.push(explain(&aws, &cluster, &e)),
                    }
                    lines.push(String::new());
                    match &target.container {
                        None => lines.push("── No logs: the container has not run ──".into()),
                        Some(c) => {
                            lines.push(format!("── Logs of {c} ({which}; p switches) ──"));
                            match client.logs(&target.namespace, &target.name, c, target.previous, POD_LINES) {
                                Ok(text) if text.trim().is_empty() => lines.push("(empty)".into()),
                                Ok(text) => lines.extend(text.lines().map(str::to_string)),
                                Err(e) => lines.push(e.short()),
                            }
                        }
                    }
                }
            }
            Msg::Look { cluster: name, title, lines }
        });
    }

    fn copy_row(&self, ctx: &mut Ctx) {
        let Some(c) = self.selected() else { return };
        let text = if !self.focused {
            Some(c.name.clone())
        } else {
            let d = self.detail();
            match self.tab {
                Tab::Nodes => d.and_then(|d| d.nodegroups.as_ref().ok()?.get(self.cursor).map(|n| n.nodegroup_name.clone())),
                Tab::Addons => d.and_then(|d| d.addons.as_ref().ok()?.get(self.cursor).map(|a| format!("{} {}", a.addon_name, a.addon_version))),
                Tab::Insights => d.and_then(|d| d.insights.as_ref().ok()?.get(self.cursor).map(|i| i.name.clone())),
                Tab::Access => d.and_then(|d| d.access.as_ref().ok()?.as_ref()?.get(self.cursor).map(|e| e.principal_arn.clone())),
                Tab::Workloads => {
                    let w = self.workloads.get(&c.name).and_then(|w| w.as_ref().ok());
                    match (self.work_rows().get(self.cursor), w) {
                        (Some(Work::Deployment(i)), Some(w)) => Some(format!("{}/{}", w.deployments[*i].namespace, w.deployments[*i].name)),
                        (Some(Work::Pod(i)), Some(w)) => Some(format!("{}/{}", w.failing[*i].0.namespace, w.failing[*i].0.name)),
                        _ => None,
                    }
                }
                Tab::Overview => Some(c.name.clone()),
            }
        };
        if let Some(text) = text {
            copy(ctx, &c.name, &text, &text);
        }
    }

    /// `c`: Markdown for an AI agent about the selected cluster, read inside it too.
    fn copy_report(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.selected().cloned() else { return };
        if !self.in_flight.insert((c.name.clone(), "report")) {
            return;
        }
        let aws = ctx.env().aws.clone();
        let versions = self.versions.clone();
        let cached = self.detail().cloned();
        let (env, region) = (self.env_name.clone(), self.region.clone());
        ctx.toast(Level::Info, format!("Reading {} for the report…", c.name));
        ctx.spawn(move || {
            let result = cached.map(Ok).unwrap_or_else(|| eks::detail(&aws, &c.name, &versions)).map_err(|e| e.short()).map(|d| {
                let inside = k8s::Client::new(aws.clone(), &d.cluster)
                    .and_then(|client| k8s::findings(&client, REPORT_PODS, REPORT_LINES))
                    .map_err(|e| explain(&aws, &d.cluster, &e));
                eks::report(&env, &region, &d, Some(inside.as_ref().map_err(String::clone)), now_secs())
            });
            Msg::Report { cluster: c.name, result }
        });
    }

    /// `K`: k9s on the cluster, with dcli's kubeconfig for it.
    fn open_k9s(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.cluster() else { return };
        if c.endpoint.as_deref().is_none_or(str::is_empty) {
            return ctx.toast(Level::Warn, format!("{} has no API endpoint yet ({})", c.name, c.status));
        }
        match eks::write_kubeconfig(&c, &self.env_name, self.config_file.as_deref()) {
            Ok(path) => {
                let mut command = std::process::Command::new("k9s");
                command.arg("--kubeconfig").arg(&path).env("KUBECONFIG", &path);
                let missing = format!(
                    "k9s is not installed (https://k9scli.io). With kubectl: KUBECONFIG={} kubectl get pods -A",
                    crate::util::display_path(&path)
                );
                ctx.run_interactive(command, missing);
            }
            Err(e) => ctx.toast(Level::Error, e),
        }
    }

    // ---- rendering ---------------------------------------------------------------------

    fn support_of(&self, c: &Cluster) -> Support {
        Support::of(&c.version, &self.versions, now_secs())
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
            LoadState::Loading if self.clusters.is_empty() => format!(" {} Clusters ", theme::spinner(view.tick)),
            _ => format!(" Clusters · {} ", self.rows.len()),
        };
        if self.rows.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading clusters…", theme::muted()),
                _ => Line::styled(format!("No EKS clusters in {}", self.region), theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, !self.focused)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let now = now_secs();
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|&i| {
                let c = &self.clusters[i];
                let support = self.support_of(c);
                let healthy = c.status == "ACTIVE" && c.health.issues.is_empty();
                let (symbol, style) = mark(healthy, support.attention(now));
                let right = format!("{} · {}", c.version, if support.extended() { "EXTENDED".to_string() } else { support.short(now) });
                let name_w = width.saturating_sub(3 + right.chars().count());
                let name = ellipsize(&c.name, name_w);
                let pad = width.saturating_sub(2 + name.chars().count() + right.chars().count());
                let right_style = if support.attention(now) { Style::new().fg(theme::WARN) } else { theme::muted() };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{symbol} "), style),
                    Span::raw(name),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(right, right_style),
                ]))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, !self.focused)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_detail(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let Some(c) = self.cluster() else {
            f.render_widget(widgets::placeholder(Line::styled("Select a cluster", theme::muted())).block(widgets::panel(" Cluster ", false)), area);
            return;
        };
        let (symbol, style) = mark(c.status == "ACTIVE" && c.health.issues.is_empty(), false);
        let block = widgets::panel(Line::from(format!(" {} ", c.name)).bold(), self.focused)
            .title_top(Line::styled(format!(" {symbol} {} · Kubernetes {} ", c.status, c.version), style.bold()).right_aligned());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [tabs_area, body] = inner.layout(&Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(self.tabs_line()), tabs_area);
        let (lines, rows) = self.tab_lines(&c, view);
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

    fn tabs_line(&self) -> Line<'static> {
        let d = self.detail();
        let bad = |n: usize| if n > 0 { format!(" {n}✗") } else { String::new() };
        let mut spans = Vec::new();
        for tab in TABS {
            let extra = match (tab, d) {
                (Tab::Nodes, Some(d)) => bad(d.nodegroups.as_ref().map_or(0, |l| l.iter().filter(|n| !n.ok()).count())),
                (Tab::Addons, Some(d)) => bad(d.addons.as_ref().map_or(0, |l| l.iter().filter(|a| !a.ok()).count())),
                (Tab::Insights, Some(d)) => bad(d.insights.as_ref().map_or(0, |l| l.iter().filter(|i| !i.passing()).count())),
                (Tab::Workloads, _) => match self.selected().and_then(|c| self.workloads.get(&c.name)) {
                    Some(Ok(w)) => bad(w.failing.len() + w.unhealthy().len()),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            let label = format!(" {}{extra} ", tab.label());
            spans.push(if tab == self.tab { Span::styled(label, theme::selected()) } else { Span::styled(label, theme::muted()) });
            spans.push(Span::raw(" "));
        }
        Line::from(spans)
    }

    /// The lines of the current tab and, for each selectable row, the line it starts at.
    fn tab_lines(&self, c: &Cluster, view: &View) -> (Vec<Line<'static>>, Vec<usize>) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut rows: Vec<usize> = Vec::new();
        let loading = |what: &str| Line::styled(format!("{} Reading {what}…", theme::spinner(view.tick)), theme::muted());
        let err = |e: &AwsError| Line::styled(e.short(), Style::new().fg(theme::ERR));
        let cursor_style = |i: usize| if self.focused && i == self.cursor { theme::selected() } else { Style::new() };
        let detail = match self.details.get(&c.name) {
            Some(Ok(d)) => Some(d),
            Some(Err(e)) if self.tab != Tab::Workloads => return (vec![err(e)], rows),
            _ => None,
        };
        if detail.is_none() && self.tab != Tab::Workloads {
            if self.tab == Tab::Overview {
                lines.extend(self.overview(c, None));
            }
            lines.push(loading("the cluster"));
            return (lines, rows);
        }
        match self.tab {
            Tab::Overview => lines.extend(self.overview(c, detail)),
            Tab::Nodes => match &detail.map(|d| &d.nodegroups) {
                Some(Ok(list)) if list.is_empty() => {
                    let why = if c.auto_mode() { "No node groups: EKS Auto Mode runs the nodes" } else { "No managed node groups" };
                    lines.push(Line::styled(why, theme::muted()));
                }
                Some(Ok(list)) => {
                    for (i, n) in list.iter().enumerate() {
                        let skew = n.version.as_deref().is_some_and(|v| !v.is_empty() && v != c.version);
                        let (symbol, style) = mark(n.ok(), skew);
                        rows.push(lines.len());
                        let s = &n.scaling_config;
                        lines.push(Line::from(vec![
                            Span::styled(format!("{symbol} "), style),
                            Span::styled(format!("{:<20}", n.nodegroup_name), cursor_style(i).bold()),
                            Span::raw(format!(" {:<9} {} nodes ({}–{})", n.status, s.desired_size, s.min_size, s.max_size)),
                        ]));
                        let version = n.version.clone().unwrap_or_default();
                        let version_style = if skew { Style::new().fg(theme::WARN) } else { theme::muted() };
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("    {} · {} · ", n.instance_types.join(", "), n.capacity_type.as_deref().unwrap_or("ON_DEMAND")),
                                theme::muted(),
                            ),
                            Span::styled(format!("Kubernetes {version}"), version_style),
                            Span::styled(
                                format!(" · {} {}", n.ami_type.as_deref().unwrap_or(""), n.release_version.as_deref().unwrap_or("")),
                                theme::muted(),
                            ),
                        ]));
                        for issue in &n.health.issues {
                            lines.push(Line::styled(format!("    {}", issue.text()), Style::new().fg(theme::ERR)));
                        }
                    }
                }
                Some(Err(e)) => lines.push(err(e)),
                None => {}
            },
            Tab::Addons => match &detail.map(|d| &d.addons) {
                Some(Ok(list)) if list.is_empty() => lines.push(Line::styled("No EKS add-ons", theme::muted())),
                Some(Ok(list)) => {
                    for (i, a) in list.iter().enumerate() {
                        let (symbol, style) = mark(a.ok(), a.newer.is_some());
                        rows.push(lines.len());
                        let mut spans = vec![
                            Span::styled(format!("{symbol} "), style),
                            Span::styled(format!("{:<24}", a.addon_name), cursor_style(i).bold()),
                            Span::raw(format!(" {:<22} {}", a.addon_version, a.status)),
                        ];
                        if let Some(v) = &a.newer {
                            spans.push(Span::styled(format!("  → {v}"), Style::new().fg(theme::WARN)));
                        }
                        lines.push(Line::from(spans));
                        for issue in &a.health.issues {
                            lines.push(Line::styled(format!("    {}", issue.text()), Style::new().fg(theme::ERR)));
                        }
                    }
                }
                Some(Err(e)) => lines.push(err(e)),
                None => {}
            },
            Tab::Insights => match &detail.map(|d| &d.insights) {
                Some(Ok(list)) if list.is_empty() => lines.push(Line::styled("No insights yet", theme::muted())),
                Some(Ok(list)) => {
                    for (i, ins) in list.iter().enumerate() {
                        let status = ins.insight_status.status.as_str();
                        let (symbol, style) = mark(status != "ERROR", status != "PASSING");
                        rows.push(lines.len());
                        let open = self.expanded.contains(&ins.id);
                        lines.push(Line::from(vec![
                            Span::styled(format!("{symbol} {:<8} ", status), style),
                            Span::styled(ins.name.clone(), cursor_style(i)),
                            Span::styled(if open { "  ▾" } else if !ins.passing() { "  ▸" } else { "" }, theme::muted()),
                        ]));
                        if !open {
                            continue;
                        }
                        if !ins.insight_status.reason.is_empty() {
                            lines.push(Line::styled(format!("    {}", ins.insight_status.reason), theme::muted()));
                        }
                        if let Some(r) = &ins.recommendation {
                            lines.push(Line::raw(format!("    {r}")));
                        }
                        for dep in ins.category_specific_summary.iter().flat_map(|s| &s.deprecation_details) {
                            let to = dep.replaced_with.as_deref().map(|r| format!(" → {r}")).unwrap_or_default();
                            let removed = dep.stop_serving_version.as_deref().map(|v| format!(" (removed in {v})")).unwrap_or_default();
                            lines.push(Line::styled(format!("    {}{to}{removed}", dep.usage), Style::new().fg(theme::WARN)));
                            for s in &dep.client_stats {
                                let when = s.last_request_time.map(|t| format!(", last {}", fmt_ago(t))).unwrap_or_default();
                                lines.push(Line::styled(
                                    format!("      called by {} ({} requests in 30 days{when})", s.user_agent, s.number_of_requests_last30_days),
                                    theme::muted(),
                                ));
                            }
                        }
                        for r in ins.resources.iter().filter_map(|r| r.kubernetes_resource_uri.as_deref().or(r.arn.as_deref())) {
                            lines.push(Line::styled(format!("    {r}"), theme::muted()));
                        }
                    }
                }
                Some(Err(e)) => lines.push(err(e)),
                None => {}
            },
            Tab::Access => match &detail.map(|d| &d.access) {
                Some(Ok(None)) => {
                    lines.push(Line::raw("This cluster uses only the aws-auth ConfigMap (authentication mode CONFIG_MAP):"));
                    lines.push(Line::styled("who may use it is in kube-system/aws-auth, inside the cluster (k9s, K).", theme::muted()));
                }
                Some(Ok(Some(list))) if list.is_empty() => lines.push(Line::styled("No access entries", theme::muted())),
                Some(Ok(Some(list))) => {
                    for (i, e) in list.iter().enumerate() {
                        rows.push(lines.len());
                        let name = e.principal_arn.split(':').next_back().unwrap_or(&e.principal_arn).to_string();
                        lines.push(Line::from(vec![
                            Span::styled(name, cursor_style(i).bold()),
                            Span::styled(format!("  {}", e.kind), theme::muted()),
                        ]));
                        let policies: Vec<String> = e.policies.iter().map(eks::AccessPolicy::label).collect();
                        let what = if !policies.is_empty() {
                            policies.join(", ")
                        } else if !e.kubernetes_groups.is_empty() {
                            format!("groups {}", e.kubernetes_groups.join(", "))
                        } else {
                            "no access policy".to_string()
                        };
                        lines.push(Line::styled(format!("    {what}"), theme::muted()));
                    }
                }
                Some(Err(e)) => lines.push(err(e)),
                None => {}
            },
            Tab::Workloads => self.workload_lines(c, view, &mut lines, &mut rows),
        }
        (lines, rows)
    }

    fn overview(&self, c: &Cluster, d: Option<&Detail>) -> Vec<Line<'static>> {
        let now = now_secs();
        let support = d.map_or_else(|| self.support_of(c), |d| d.support.clone());
        let w = 12;
        let support_style = if support.attention(now) { Style::new().fg(theme::WARN).bold() } else { Style::new() };
        let mut lines = vec![
            widgets::kv("Kubernetes", Span::raw(format!("{} (platform {})", c.version, c.platform_version)), w),
            widgets::kv("Support", Span::styled(support.describe(c, now), support_style), w),
            widgets::kv(
                "After it",
                Span::raw(if c.support_type() == "STANDARD" { "AWS upgrades the cluster (upgrade policy STANDARD)" } else { "extended support (upgrade policy EXTENDED)" }),
                w,
            ),
            widgets::kv("Endpoint", Span::raw(c.endpoint_access()), w),
            widgets::kv("Auth mode", Span::raw(c.auth_mode().to_string()), w),
        ];
        if c.auto_mode() {
            let pools = c.compute_config.as_ref().map(|x| x.node_pools.join(", ")).unwrap_or_default();
            lines.push(widgets::kv("Auto Mode", Span::raw(format!("on · node pools {pools}")), w));
        }
        let logs = c.logs_enabled();
        lines.push(widgets::kv(
            "Logs",
            if logs.is_empty() { Span::styled("control plane logs off", theme::muted()) } else { Span::raw(format!("{} (l opens them)", logs.join(", "))) },
            w,
        ));
        if let Some(t) = c.created_at {
            lines.push(widgets::kv("Created", Span::raw(fmt_ago(t)), w));
        }
        if let Some(fargate) = d.and_then(|d| d.fargate.as_ref().ok()).filter(|f| !f.is_empty()) {
            let profiles: Vec<String> = fargate.iter().map(|f| format!("{} ({})", f.fargate_profile_name, f.selects())).collect();
            lines.push(widgets::kv("Fargate", Span::raw(profiles.join(", ")), w));
        }
        if let Some(d) = d {
            let problems = d.problems(now);
            lines.push(Line::raw(""));
            if problems.is_empty() {
                lines.push(Line::styled("✓ Nothing needs attention, as AWS sees the cluster", Style::new().fg(theme::OK)));
            } else {
                lines.push(widgets::section("Needs attention"));
                for p in problems {
                    lines.push(Line::styled(format!("! {p}"), Style::new().fg(theme::WARN)));
                }
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled("w: what is broken inside · K: k9s on it · c: copy for AI", theme::muted()));
        }
        lines
    }

    fn workload_lines(&self, c: &Cluster, view: &View, lines: &mut Vec<Line<'static>>, rows: &mut Vec<usize>) {
        let cursor_style = |i: usize| if self.focused && i == self.cursor { theme::selected() } else { Style::new() };
        let w = match self.workloads.get(&c.name) {
            None => {
                lines.push(Line::styled(format!("{} Reading inside the cluster…", theme::spinner(view.tick)), theme::muted()));
                return;
            }
            Some(Err(why)) => {
                lines.push(Line::styled(why.clone(), Style::new().fg(theme::ERR)));
                lines.push(Line::raw(""));
                lines.push(Line::styled("r reads again", theme::muted()));
                return;
            }
            Some(Ok(w)) => w,
        };
        let unhealthy = w.unhealthy().len();
        lines.push(Line::styled(
            format!(
                "{} namespaces · {} deployments ({unhealthy} not healthy) · {} pods ({} with a problem){}",
                w.namespaces,
                w.deployments.len(),
                w.pods,
                w.failing.len(),
                if w.truncated { " · more than dcli lists" } else { "" }
            ),
            theme::muted(),
        ));
        lines.push(Line::raw(""));
        let work = self.work_rows();
        let deployments = work.iter().filter(|r| matches!(r, Work::Deployment(_))).count();
        if deployments > 0 || self.show_all {
            lines.push(widgets::section(if self.show_all { "Deployments" } else { "Deployments not healthy" }));
        }
        for (i, row) in work.iter().enumerate() {
            match *row {
                Work::Deployment(d) => {
                    let d = &w.deployments[d];
                    let problem = d.problem();
                    let (symbol, style) = mark(problem.is_none(), false);
                    rows.push(lines.len());
                    lines.push(Line::from(vec![
                        Span::styled(format!("{symbol} "), style),
                        Span::styled(format!("{}/{}", d.namespace, d.name), cursor_style(i).bold()),
                        Span::raw(format!("  {}/{}", d.ready, d.desired)),
                        Span::styled(problem.map(|p| format!("  {p}")).unwrap_or_default(), theme::muted()),
                    ]));
                }
                Work::Pod(p) => {
                    if work.get(i.wrapping_sub(1)).is_none_or(|r| matches!(r, Work::Deployment(_))) {
                        if i > 0 {
                            lines.push(Line::raw(""));
                        }
                        lines.push(widgets::section("Pods with a problem"));
                    }
                    let (pod, why) = &w.failing[p];
                    rows.push(lines.len());
                    lines.push(Line::from(vec![
                        Span::styled("✗ ", Style::new().fg(theme::ERR)),
                        Span::styled(format!("{}/{}", pod.namespace, pod.name), cursor_style(i).bold()),
                        Span::styled(format!("  {}", why.reason), Style::new().fg(theme::ERR)),
                        Span::styled(if pod.restarts() > 0 { format!(" · {} restarts", pod.restarts()) } else { String::new() }, theme::muted()),
                    ]));
                    if !why.detail.is_empty() {
                        lines.push(Line::styled(format!("    {}", ellipsize(&why.detail, 300)), theme::muted()));
                    }
                }
            }
        }
        if work.is_empty() {
            lines.push(Line::styled("✓ Every deployment is ready and no pod has a problem", Style::new().fg(theme::OK)));
        }
        lines.push(Line::raw(""));
        let all = if self.show_all { "a: only those not healthy" } else { "a: every deployment" };
        lines.push(Line::styled(format!("Enter: events and logs · {all} · K: k9s"), theme::muted()));
    }
}

impl Module for Eks {
    fn title(&self) -> &'static str {
        "EKS"
    }

    fn description(&self) -> &'static str {
        "EKS clusters · version and support, node groups, add-ons, upgrade insights, access, failing pods, k9s"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Eks::new();
        self.env_name = ctx.env().name.clone();
        self.region = ctx.env().region.clone();
        self.config_file = match &ctx.shared.loaded.source {
            crate::config::Source::File(p) => Some(p.clone()),
            crate::config::Source::AwsProfiles => None,
        };
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load_clusters(ctx);
        }
    }

    /// Shows the cluster `target` (its name).
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        self.zoom = None;
        self.pending_open = Some(target.to_string());
        if matches!(self.load, LoadState::Loaded) && !self.try_open() {
            self.pending_open = None;
            ctx.toast(Level::Warn, format!("Cluster not found here: {target}"));
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some(zoom) = &mut self.zoom {
            if !zoom.ready {
                if key.code == KeyCode::Esc {
                    self.zoom = None;
                }
                return Handled::Yes;
            }
            // `p`: the other run of the pod's container.
            if key.code == KeyCode::Char('p')
                && let Some(pod) = zoom.pod.clone()
                && pod.container.is_some()
            {
                if let Some(c) = self.cluster() {
                    let target = PodRef { previous: !pod.previous, ..pod };
                    self.look_pod(c, target, Vec::new(), ctx);
                }
                return Handled::Yes;
            }
            if let LogKey::Copy = zoom.view.key(key) {
                let ok = crate::platform::copy_to_clipboard(&zoom.view.lines().join("\n"));
                ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Copied" } else { "Clipboard not available" });
            }
            if !zoom.view.zoomed {
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
                    KeyCode::Up => self.select(selected.saturating_sub(1)),
                    KeyCode::Down => self.select(selected + 1),
                    _ => {}
                },
                InputEvent::Moved => {}
            }
            return Handled::Yes;
        }
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
            KeyCode::Char('K') => {
                self.open_k9s(ctx);
                return Handled::Yes;
            }
            KeyCode::Char('w') => {
                self.set_tab(Tab::Workloads, ctx);
                return Handled::Yes;
            }
            KeyCode::Char('a') if self.tab == Tab::Workloads => {
                self.show_all = !self.show_all;
                self.cursor = 0;
                return Handled::Yes;
            }
            KeyCode::Char('l') => {
                match self.cluster() {
                    Some(c) if !c.logs_enabled().is_empty() => ctx.jump("Logs", c.log_group()),
                    Some(c) => ctx.toast(Level::Info, format!("{}'s control plane logs are off (EKS console → Observability)", c.name)),
                    None => {}
                }
                return Handled::Yes;
            }
            KeyCode::Char('o') => {
                if let Some(c) = self.selected() {
                    let url = eks::console_url(&self.region, &c.name);
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
                if self.tab == Tab::Overview {
                    self.set_tab(Tab::Workloads, ctx);
                }
                if self.row_count() > 0 {
                    self.focused = true;
                    self.cursor = self.cursor.min(self.row_count() - 1);
                }
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Char('r') => {
                if let Some(name) = self.selected().map(|c| c.name.clone()) {
                    self.details.remove(&name);
                    self.workloads.remove(&name);
                }
                self.load_clusters(ctx);
                self.load_tab(ctx);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if self.zoom.is_some() || !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
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
            Msg::Overview(result) => match result {
                Ok(o) => {
                    self.clusters = o.clusters;
                    self.versions = o.versions;
                    self.load = LoadState::Loaded;
                    self.refilter();
                    if !self.try_open() {
                        if let Some(target) = self.pending_open.take() {
                            ctx.toast(Level::Warn, format!("Cluster not found here: {target}"));
                        }
                        self.select(self.list_state.selected().unwrap_or(0));
                    }
                }
                Err(e) => self.load = LoadState::Failed(e),
            },
            Msg::Detail { cluster, result } => {
                self.in_flight.remove(&(cluster.clone(), "detail"));
                // The cluster as described now (endpoint, version) replaces the list's.
                if let Ok(d) = &result
                    && let Some(c) = self.clusters.iter_mut().find(|c| c.name == cluster)
                {
                    *c = d.cluster.clone();
                }
                self.details.insert(cluster, result);
            }
            Msg::Workloads { cluster, result } => {
                self.in_flight.remove(&(cluster.clone(), "workloads"));
                self.workloads.insert(cluster, result);
            }
            Msg::Look { cluster, title, lines } => {
                let current = self.selected().is_some_and(|c| c.name == cluster);
                if let Some(zoom) = self.zoom.as_mut().filter(|z| z.title == title)
                    && current
                {
                    let mut view = LogView::default();
                    view.extend(lines);
                    view.zoomed = true;
                    view.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
                    zoom.view = view;
                    zoom.ready = true;
                }
            }
            Msg::Report { cluster, result } => {
                self.in_flight.remove(&(cluster.clone(), "report"));
                match result {
                    Ok(md) => copy(ctx, &cluster, &md, &format!("the report on {cluster}. Paste it into your AI agent.")),
                    Err(e) => ctx.toast(Level::Error, e),
                }
            }
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, _active: bool) {
        if let Some((name, at)) = &self.pending
            && at.elapsed() >= DEBOUNCE
        {
            let name = name.clone();
            self.pending = None;
            if self.selected().is_some_and(|c| c.name == name) {
                self.load_tab(ctx);
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some(zoom) = &mut self.zoom {
            if !zoom.ready {
                // Still reading: a zoomed view with a spinner.
                let mut waiting = LogView::default();
                waiting.extend([format!("{} Reading…", theme::spinner(view.tick))]);
                waiting.zoomed = true;
                waiting.render_zoomed(f, area, &zoom.title);
                return;
            }
            let title = zoom.title.clone();
            zoom.view.render_zoomed(f, area, &title);
            return;
        }
        let [left, right] = if area.width < 100 {
            area.layout(&Layout::vertical([Constraint::Percentage(35), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(32), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        self.render_detail(f, right, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if let Some(z) = &self.zoom {
            let mut hints = vec![("/", "search"), ("y", "copy")];
            if z.pod.as_ref().is_some_and(|p| p.container.is_some()) {
                hints.push(("p", "other run"));
            }
            hints.push(("Esc", "back"));
            return hints;
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        if self.focused {
            let enter = match self.tab {
                Tab::Insights => ("Enter", "details"),
                Tab::Workloads => ("Enter", "events and logs"),
                _ => ("", ""),
            };
            let mut hints = vec![("↑↓", "row"), ("←→", "view")];
            if !enter.0.is_empty() {
                hints.push(enter);
            }
            hints.extend([("y", "copy"), ("K", "k9s"), ("Esc", "clusters")]);
            return hints;
        }
        vec![
            ("↑↓", "cluster"),
            ("←→", "view"),
            ("Enter", "rows"),
            ("w", "workloads"),
            ("K", "k9s"),
            ("c", "copy for AI"),
            ("l", "logs"),
            ("/", "search"),
            ("o", "console"),
            ("r", "reload"),
        ]
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading) || !self.in_flight.is_empty() || self.pending.is_some() || self.zoom.as_ref().is_some_and(|z| !z.ready)
    }

    fn capturing(&self) -> bool {
        self.filtering || self.zoom.is_some()
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

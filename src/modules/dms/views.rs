//! Drawing the DMS module: the pages (tasks, instances, endpoints), an open task's tabs
//! (overview, tables, settings) and the dialogs.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use super::{Dialog, Dms, PAGES, Page, TABS, Tab};
use crate::aws::dms::{self, Health, Metrics, PrepMode, Replication, Severity, TableState, TableStat};
use crate::config::is_production;
use crate::modules::common::{LoadState, filter_bar};
use crate::tui::chart::{Agg, Chart, fmt_count};
use crate::tui::module::View;
use crate::tui::theme;
use crate::tui::widgets;
use crate::util::{ellipsize, fmt_ago, fmt_bytes, fmt_utc};

const KW: usize = 13;

fn bytes(v: f64) -> String {
    fmt_bytes(v as i64)
}

/// `244M`, `1.2G`: bytes in a chart's narrow scale column.
fn short_bytes(v: f64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let (mut v, mut unit) = (v.max(0.0), 0);
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if v >= 10.0 || unit == 0 { format!("{v:.0}{}", UNITS[unit]) } else { format!("{v:.1}{}", UNITS[unit]) }
}

fn percent(v: f64) -> String {
    format!("{v:.0}%")
}

/// The symbol and colour of a task's health.
fn health_mark(r: &Replication, tick: usize) -> (&'static str, Color) {
    match r.health() {
        Health::Failed => ("✗", theme::ERR),
        Health::TableErrors => ("⚠", theme::ERR),
        Health::Running => ("●", theme::OK),
        Health::Changing => (theme::spinner(tick), theme::RUNNING),
        Health::Ready => ("○", theme::MUTED),
        Health::Stopped => ("■", theme::MUTED),
    }
}

fn severity_mark(s: Severity) -> (&'static str, Color) {
    match s {
        Severity::Good => ("●", theme::OK),
        Severity::Warn => ("▲", theme::WARN),
        Severity::Bad => ("✗", theme::ERR),
    }
}

fn state_mark(t: &TableStat) -> (&'static str, Color) {
    match t.state() {
        TableState::Error => ("✗", theme::ERR),
        TableState::Loading => ("⠿", theme::RUNNING),
        TableState::Queued => ("○", theme::MUTED),
        TableState::Completed if t.validation_problem() => ("▲", theme::WARN),
        TableState::Completed => ("✓", theme::OK),
        TableState::Cancelled | TableState::Missing | TableState::Other => ("■", theme::MUTED),
    }
}

impl Dms {
    // ---- pages -------------------------------------------------------------------------------

    pub(super) fn render_pages(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [tabs, bar, body] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Fill(1)]));
        let mut spans = Vec::new();
        for page in PAGES {
            let (label, count) = match page {
                Page::Tasks => ("Tasks", self.tasks.len()),
                Page::Instances => ("Instances", self.instances.len()),
                Page::Endpoints => ("Endpoints", self.endpoints.len()),
            };
            let style = if page == self.page { Style::new().fg(Color::Black).bg(theme::ACCENT).bold() } else { theme::muted() };
            spans.push(Span::styled(format!(" {label} · {count} "), style));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(format!("  ←→ page · m {} ", self.range.label()), theme::muted()));
        f.render_widget(Paragraph::new(Line::from(spans)), tabs);
        let placeholder = match self.page {
            Page::Tasks => "type / to search tasks (or an instance)",
            Page::Instances => "type / to search instances",
            Page::Endpoints => "type / to search endpoints",
        };
        let (line, cursor) = filter_bar(&self.filter, self.filtering, placeholder);
        f.render_widget(Paragraph::new(line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let [left, right] = if area.width < 100 {
            body.layout(&Layout::vertical([Constraint::Percentage(40), Constraint::Fill(1)]))
        } else {
            body.layout(&Layout::horizontal([Constraint::Percentage(42), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        match self.page {
            Page::Tasks => self.render_task_detail(f, right, view),
            Page::Instances => self.render_instance_detail(f, right, view),
            Page::Endpoints => self.render_endpoint_detail(f, right, view),
        }
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let (load, noun) = match self.page {
            Page::Endpoints => (&self.inventory, "endpoints"),
            Page::Tasks => (&self.load, "tasks"),
            Page::Instances => (&self.load, "replication instances"),
        };
        let title = match load {
            LoadState::Loading if self.visible.is_empty() => format!(" {} DMS ", theme::spinner(view.tick)),
            _ => format!(" {} · {} ", capitalize(noun), self.visible.len()),
        };
        if self.visible.is_empty() {
            let text = match load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading | LoadState::Idle => Line::styled(format!("Loading {noun}…"), theme::muted()),
                LoadState::Loaded if !self.filter.value.is_empty() => Line::styled("Nothing matches", theme::muted()),
                LoadState::Loaded => Line::styled(format!("No DMS {noun} in this account and region"), theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), area);
            return;
        }
        let width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self.visible.iter().map(|&i| self.list_row(i, width, view.tick)).collect();
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        let index = self.page_index();
        f.render_stateful_widget(list, area, &mut self.states[index]);
    }

    fn list_row(&self, i: usize, width: usize, tick: usize) -> ListItem<'static> {
        let row = |mark: (&str, Color), name: &str, tag: &str, right: Span<'static>| {
            let right_w = right.content.chars().count();
            let name_w = width.saturating_sub(right_w + tag.chars().count() + 4);
            let name = ellipsize(name, name_w);
            let pad = width.saturating_sub(2 + name.chars().count() + tag.chars().count() + right_w);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", mark.0), Style::new().fg(mark.1)),
                Span::raw(name),
                Span::styled(tag.to_string(), theme::muted()),
                Span::raw(" ".repeat(pad)),
                right,
            ]))
        };
        match self.page {
            Page::Tasks => {
                let t = &self.tasks[i];
                let s = &t.stats;
                let right = match t.health() {
                    Health::Failed => Span::styled("failed", Style::new().fg(theme::ERR).bold()),
                    Health::TableErrors => Span::styled(
                        format!("{} table{} in error", s.tables_errored, if s.tables_errored == 1 { "" } else { "s" }),
                        Style::new().fg(theme::ERR).bold(),
                    ),
                    Health::Running if t.loading() => Span::styled(format!("loading {:.0}%", s.full_load_percent.unwrap_or(0.0)), Style::new().fg(theme::RUNNING)),
                    Health::Stopped => Span::styled(format!("stopped {}", t.stopped_at().map(fmt_ago).unwrap_or_default()), theme::muted()),
                    _ => Span::styled(t.status.clone(), theme::muted()),
                };
                row(health_mark(t, tick), &t.id, &format!(" {}", t.migration.short()), right)
            }
            Page::Instances => {
                let inst = &self.instances[i];
                let on: Vec<&Replication> = self.tasks.iter().filter(|t| t.instance_arn.as_ref() == Some(&inst.arn)).collect();
                let running = on.iter().filter(|t| t.running()).count();
                let mark = match inst.status.as_str() {
                    "available" if running > 0 => ("●", theme::OK),
                    "available" => ("○", theme::MUTED),
                    "modifying" | "creating" | "rebooting" | "upgrading" => (theme::spinner(tick), theme::RUNNING),
                    _ => ("✗", theme::ERR),
                };
                let right = Span::styled(format!("{running}/{} running", on.len()), theme::muted());
                row(mark, &inst.id, &format!(" {}", inst.class.trim_start_matches("dms.")), right)
            }
            Page::Endpoints => {
                let e = &self.endpoints[i];
                let tests: Vec<&dms::Connection> = self.connections.iter().filter(|c| c.endpoint_arn == e.arn).collect();
                let testing = self.testing.iter().any(|(_, ep, _)| *ep == e.arn) || tests.iter().any(|c| c.status == "testing");
                let (right, mark) = if testing {
                    (Span::styled("testing…", Style::new().fg(theme::RUNNING)), (theme::spinner(tick), theme::RUNNING))
                } else if tests.iter().any(|c| c.status == "failed") {
                    (Span::styled("test failed", Style::new().fg(theme::ERR)), ("✗", theme::ERR))
                } else if !tests.is_empty() {
                    (Span::styled("tested", Style::new().fg(theme::OK)), ("✓", theme::OK))
                } else {
                    (Span::styled("not tested", theme::muted()), ("·", theme::MUTED))
                };
                row(mark, &e.id, &format!(" {} {}", e.role, e.engine_label), right)
            }
        }
    }

    fn render_task_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(t) = self.selected_task() else {
            f.render_widget(widgets::placeholder(Line::styled("Select a task", theme::muted())).block(widgets::panel(" Task ", false)), area);
            return;
        };
        let mut lines = self.task_summary(t, view);
        lines.push(Line::raw(""));
        let width = area.width.saturating_sub(4) as usize;
        match self.metrics_of(&t.arn) {
            Some(Ok(m)) => {
                let diagnosis = self.diagnosis_lines(t, m);
                let any = !diagnosis.is_empty();
                lines.extend(diagnosis);
                if t.migration.has_cdc() {
                    if any {
                        lines.push(Line::raw(""));
                    }
                    lines.extend(latency_chart(m, width, 2));
                }
            }
            Some(Err(e)) => lines.push(Line::styled(format!("Metrics: {}", e.short()), Style::new().fg(theme::ERR))),
            None => lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::styled(" reading metrics…", theme::muted())])),
        }
        lines.push(Line::raw(""));
        lines.push(widgets::hints_line(&[("Enter", "tables, metrics, settings"), ("c", "copy for AI")]));
        let (mark, color) = health_mark(t, view.tick);
        let block = widgets::panel(Line::from(format!(" {} ", t.id)).bold(), false)
            .title_top(Line::styled(format!(" {mark} {} ", t.status_label()), Style::new().fg(color).bold()).right_aligned());
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
    }

    /// Status, instance, endpoints, progress and failure of a task.
    fn task_summary(&self, t: &Replication, view: &View) -> Vec<Line<'static>> {
        let err = Style::new().fg(theme::ERR);
        let mut lines = vec![widgets::kv("Type", t.migration.label(), KW)];
        let instance = match self.instance_of(t) {
            Some(i) => format!("{} · {}", i.id, i.class_label()),
            None => t.instance.clone().unwrap_or_else(|| "?".into()),
        };
        lines.push(widgets::kv("Instance", instance, KW));
        for (label, arn) in [("Source", &t.source_arn), ("Target", &t.target_arn)] {
            match self.endpoint(arn) {
                Some(e) => {
                    lines.push(widgets::kv(label, format!("{} · {}", e.id, e.engine_label), KW));
                    let address = e.address();
                    if !address.is_empty() {
                        lines.push(Line::styled(format!("{:<KW$} {address}", ""), theme::muted()));
                    }
                }
                None => lines.push(widgets::kv(label, dms::resource_id(arn), KW)),
            }
        }
        let s = &t.stats;
        if t.migration.has_full_load() {
            let pct = s.full_load_percent.unwrap_or(0.0);
            let mut spans = vec![Span::styled(format!("{:<KW$} ", "Full load"), theme::muted())];
            spans.extend(widgets::progress_bar(pct / 100.0, 16, if s.tables_errored > 0 { theme::ERR } else { theme::OK }));
            spans.push(Span::raw(format!(" {pct:.0}%")));
            lines.push(Line::from(spans));
        }
        let mut counts = vec![Span::styled(format!("{:<KW$} ", "Tables"), theme::muted())];
        counts.push(Span::raw(format!("{} loaded · {} loading · {} queued · ", s.tables_loaded, s.tables_loading, s.tables_queued)));
        counts.push(Span::styled(format!("{} in error", s.tables_errored), if s.tables_errored > 0 { err.bold() } else { theme::muted() }));
        lines.push(Line::from(counts));
        if let Some(at) = s.start {
            lines.push(widgets::kv("Started", format!("{} · {}", fmt_ago(at), fmt_utc(at)), KW));
        }
        if let Some(at) = t.stopped_at() {
            lines.push(widgets::kv("Stopped", format!("{} · {}", fmt_ago(at), fmt_utc(at)), KW));
        }
        if let Some(reason) = &t.stop_reason {
            lines.push(widgets::kv("Stop reason", reason.clone(), KW));
        }
        if let Some(f) = &t.last_failure {
            lines.push(Line::from(vec![Span::styled(format!("{:<KW$} ", "Last failure"), theme::muted()), Span::styled(f.clone(), err)]));
        }
        if matches!(t.health(), Health::Changing) {
            lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::styled(format!(" {}…", t.status), theme::muted())]));
        }
        lines
    }

    fn diagnosis_lines(&self, t: &Replication, m: &Metrics) -> Vec<Line<'static>> {
        let memory = self.instance_of(t).and_then(dms::Instance::memory_bytes);
        dms::diagnose(t, m, memory)
            .into_iter()
            .map(|d| {
                let (mark, color) = severity_mark(d.severity);
                let style = if d.severity == Severity::Good { Style::new() } else { Style::new().fg(color) };
                Line::from(vec![Span::styled(format!("{mark} "), Style::new().fg(color)), Span::styled(d.text, style)])
            })
            .collect()
    }

    fn render_instance_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(i) = self.selected_instance() else {
            f.render_widget(widgets::placeholder(Line::styled("Select an instance", theme::muted())).block(widgets::panel(" Instance ", false)), area);
            return;
        };
        let mut lines = vec![widgets::kv("Class", i.class_label(), KW)];
        let mut status = vec![Span::styled(format!("{:<KW$} ", "Status"), theme::muted()), Span::raw(i.status.clone())];
        if let Some(c) = &i.pending_class {
            status.push(Span::styled(format!(" · → {c} pending"), Style::new().fg(theme::WARN)));
        }
        lines.push(Line::from(status));
        lines.push(widgets::kv("Engine", i.engine_version.clone(), KW));
        lines.push(widgets::kv("Storage", format!("{} GB", i.storage_gb), KW));
        lines.push(widgets::kv("Multi-AZ", if i.multi_az { "on (a standby in another zone)" } else { "off" }, KW));
        let public = if i.public { Span::styled("yes: reachable from the internet", Style::new().fg(theme::WARN)) } else { Span::raw("no") };
        lines.push(Line::from(vec![Span::styled(format!("{:<KW$} ", "Public"), theme::muted()), public]));
        if let Some(z) = &i.zone {
            lines.push(widgets::kv("Zone", z.clone(), KW));
        }
        if let Some(w) = &i.maintenance_window {
            lines.push(widgets::kv("Maintenance", w.clone(), KW));
        }
        if let Some(c) = i.created {
            lines.push(widgets::kv("Created", format!("{} · {}", fmt_ago(c), fmt_utc(c)), KW));
        }
        lines.push(Line::raw(""));
        lines.push(widgets::section("Tasks"));
        let on: Vec<&Replication> = self.tasks.iter().filter(|t| t.instance_arn.as_ref() == Some(&i.arn)).collect();
        if on.is_empty() {
            lines.push(Line::styled("No task runs here", theme::muted()));
        }
        for t in on {
            let (mark, color) = health_mark(t, view.tick);
            lines.push(Line::from(vec![
                Span::styled(format!("{mark} "), Style::new().fg(color)),
                Span::raw(t.id.clone()),
                Span::styled(format!("  {} · {}", t.migration.label(), t.status_label()), theme::muted()),
            ]));
        }
        lines.push(Line::raw(""));
        let width = area.width.saturating_sub(4) as usize;
        match self.metrics_of(&format!("instance:{}", i.id)) {
            Some(Ok(m)) => lines.extend(instance_charts(m, i.memory_bytes(), width)),
            Some(Err(e)) => lines.push(Line::styled(format!("Metrics: {}", e.short()), Style::new().fg(theme::ERR))),
            None => lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::styled(" reading metrics…", theme::muted())])),
        }
        lines.push(Line::raw(""));
        lines.push(widgets::hints_line(&[("S", "change its class"), ("Enter", "its tasks"), ("l", "task logs")]));
        f.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(widgets::panel(Line::from(format!(" {} ", i.id)).bold(), false)),
            area,
        );
    }

    fn render_endpoint_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(e) = self.selected_endpoint() else {
            f.render_widget(widgets::placeholder(Line::styled("Select an endpoint", theme::muted())).block(widgets::panel(" Endpoint ", false)), area);
            return;
        };
        let mut lines = vec![
            widgets::kv("Role", e.role.clone(), KW),
            widgets::kv("Engine", format!("{} ({})", e.engine_label, e.engine), KW),
            widgets::kv("Address", e.address(), KW),
        ];
        if let Some(u) = &e.username {
            lines.push(widgets::kv("User", u.clone(), KW));
        }
        lines.push(widgets::kv("Credentials", e.credentials.clone(), KW));
        if let Some(ssl) = &e.ssl_mode {
            let style = if ssl == "none" && e.role == "source" { Style::new().fg(theme::WARN) } else { Style::new() };
            lines.push(Line::from(vec![Span::styled(format!("{:<KW$} ", "SSL"), theme::muted()), Span::styled(ssl.clone(), style)]));
        }
        if let Some(x) = &e.extra {
            lines.push(widgets::kv("Attributes", x.clone(), KW));
        }
        lines.push(widgets::kv("Status", e.status.clone(), KW));
        lines.push(Line::raw(""));
        lines.push(widgets::section("Connection tests"));
        let tests: Vec<&dms::Connection> = self.connections.iter().filter(|c| c.endpoint_arn == e.arn).collect();
        if tests.is_empty() && !self.testing.iter().any(|(_, ep, _)| *ep == e.arn) {
            lines.push(Line::styled("Never tested from an instance · t tests it", theme::muted()));
        }
        for c in tests {
            let testing = c.status == "testing" || self.testing.iter().any(|(i, ep, _)| *ep == e.arn && *i == c.replication_instance_arn);
            let (mark, color) = match c.status.as_str() {
                _ if testing => (theme::spinner(view.tick), theme::RUNNING),
                "successful" => ("✓", theme::OK),
                "failed" => ("✗", theme::ERR),
                _ => ("·", theme::MUTED),
            };
            let status = if testing { "testing…".to_string() } else { c.status.clone() };
            lines.push(Line::from(vec![
                Span::styled(format!("{mark} "), Style::new().fg(color)),
                Span::raw(c.replication_instance_identifier.clone()),
                Span::styled(format!("  {status}"), Style::new().fg(color)),
            ]));
            if let Some(msg) = c.last_failure_message.as_ref().filter(|_| !testing) {
                lines.push(Line::styled(format!("  {msg}"), Style::new().fg(theme::ERR)));
            }
        }
        lines.push(Line::raw(""));
        lines.push(widgets::section("Used by"));
        let users: Vec<&Replication> = self.tasks.iter().filter(|t| t.source_arn == e.arn || t.target_arn == e.arn).collect();
        if users.is_empty() {
            lines.push(Line::styled("No task", theme::muted()));
        }
        for t in users {
            let (mark, color) = health_mark(t, view.tick);
            let side = if t.source_arn == e.arn { "reads from it" } else { "writes to it" };
            lines.push(Line::from(vec![Span::styled(format!("{mark} "), Style::new().fg(color)), Span::raw(t.id.clone()), Span::styled(format!("  {side}"), theme::muted())]));
        }
        lines.push(Line::raw(""));
        lines.push(widgets::hints_line(&[("t", "test the connection"), ("o", "console")]));
        f.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(widgets::panel(Line::from(format!(" {} ", e.id)).bold(), false)),
            area,
        );
    }

    // ---- an open task ------------------------------------------------------------------------

    pub(super) fn render_task(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let Some(t) = self.open_task().cloned() else {
            let text = match self.open.as_ref().and_then(|v| v.full.as_ref()) {
                Some(Err(e)) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                _ => Line::styled("Loading the task…", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(" DMS task ", true)), area);
            return;
        };
        let (mark, color) = health_mark(&t, view.tick);
        let block = widgets::panel(Line::from(format!(" {} ", t.id)).bold(), true)
            .title_top(Line::styled(format!(" {mark} {} ", t.status_label()), Style::new().fg(color).bold()).right_aligned());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [head, tabs, body] = inner.layout(&Layout::vertical([Constraint::Length(1), Constraint::Length(2), Constraint::Fill(1)]));
        let engines = |arn: &str| self.endpoint(arn).map_or_else(|| "?".to_string(), |e| e.engine_label.clone());
        let instance = self.instance_of(&t).map_or_else(|| t.instance.clone().unwrap_or_default(), |i| format!("{} ({})", i.id, i.class));
        let about = format!("{} · {instance} · {} → {} · {} {}", t.migration.label(), engines(&t.source_arn), engines(&t.target_arn), self.env_name, self.region);
        f.render_widget(Paragraph::new(Line::styled(ellipsize(&about, head.width as usize), theme::muted())), head);
        f.render_widget(Paragraph::new(self.tabs_line()), tabs);
        let tab = self.open.as_ref().map_or(Tab::Overview, |v| v.tab);
        match tab {
            Tab::Overview => self.render_overview(f, body, &t, view),
            Tab::Tables => self.render_tables(f, body, view),
            Tab::Settings => self.render_settings(f, body, &t),
        }
    }

    fn tabs_line(&self) -> Line<'static> {
        let Some(v) = &self.open else { return Line::raw("") };
        let mut spans = Vec::new();
        for tab in TABS {
            let extra = match (tab, &v.tables) {
                (Tab::Tables, Some(Ok(list))) => {
                    let errors = list.iter().filter(|t| t.state() == TableState::Error).count();
                    if errors > 0 { format!(" {} · {errors} ✗", list.len()) } else { format!(" {}", list.len()) }
                }
                _ => String::new(),
            };
            let style = if tab == v.tab { Style::new().fg(Color::Black).bg(theme::ACCENT).bold() } else { theme::muted() };
            spans.push(Span::styled(format!(" {}{extra} ", tab.label()), style));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(format!("  m {}", self.range.label()), theme::muted()));
        Line::from(spans)
    }

    fn render_overview(&self, f: &mut Frame, area: Rect, t: &Replication, view: &View) {
        let Some(v) = &self.open else { return };
        let width = area.width.saturating_sub(1) as usize;
        let mut lines = self.task_summary(t, view);
        lines.push(Line::raw(""));
        match self.metrics_of(&t.arn) {
            Some(Ok(m)) => {
                lines.push(widgets::section("What the metrics say"));
                let diagnosis = self.diagnosis_lines(t, m);
                if diagnosis.is_empty() {
                    lines.push(Line::styled("Nothing stands out in this window.", theme::muted()));
                }
                lines.extend(diagnosis);
                lines.push(Line::raw(""));
                if t.migration.has_cdc() {
                    lines.extend(latency_chart(m, width, 3));
                    let mut waiting = Chart::new("Changes waiting", m.points("incoming"), m.window);
                    waiting.agg = Agg::Max;
                    waiting.rows = 2;
                    waiting.axis = false;
                    waiting.summary = vec![Span::styled(last_max(m, "incoming", fmt_count), theme::muted())];
                    lines.extend(waiting.lines(width));
                    let mut disk = Chart::new("Changes on disk", m.points("disk_target"), m.window);
                    disk.agg = Agg::Max;
                    disk.rows = 2;
                    disk.color = theme::WARN;
                    disk.stacked = Some(m.points("disk_source"));
                    disk.stacked_color = theme::APPROVAL;
                    disk.axis = false;
                    disk.summary = vec![Span::styled(
                        format!("target {} · source {}", last_max(m, "disk_target", fmt_count), last_max(m, "disk_source", fmt_count)),
                        theme::muted(),
                    )];
                    lines.extend(disk.lines(width));
                    let mut rows = Chart::new("Rows applied/s", m.points("rows_target"), m.window);
                    rows.agg = Agg::Mean;
                    rows.rows = 2;
                    rows.color = theme::OK;
                    rows.axis = !t.loading() && t.instance.is_none();
                    rows.summary = vec![Span::styled(last_max(m, "rows_target", fmt_count), theme::muted())];
                    lines.extend(rows.lines(width));
                }
                if t.loading() {
                    let mut load = Chart::new("Full load rows/s", m.points("full_load_rows"), m.window);
                    load.agg = Agg::Mean;
                    load.rows = 2;
                    load.color = theme::OK;
                    load.axis = t.instance.is_none();
                    load.summary = vec![Span::styled(last_max(m, "full_load_rows", fmt_count), theme::muted())];
                    lines.extend(load.lines(width));
                }
                if t.instance.is_some() {
                    lines.push(Line::raw(""));
                    lines.push(widgets::section(&format!("Instance {}", t.instance.as_deref().unwrap_or_default())));
                    lines.extend(instance_charts(m, self.instance_of(t).and_then(dms::Instance::memory_bytes), width));
                }
            }
            Some(Err(e)) => lines.push(Line::styled(format!("Metrics: {}", e.short()), Style::new().fg(theme::ERR))),
            None => lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::styled(" reading metrics…", theme::muted())])),
        }
        lines.push(Line::raw(""));
        lines.push(widgets::section("Errors in the task log, last 24 h"));
        match &v.errors {
            None => lines.push(Line::styled("…", theme::muted())),
            Some(Err(e)) => lines.push(Line::styled(e.short(), theme::muted())),
            Some(Ok(errors)) if errors.is_empty() => lines.push(Line::styled("None (or logging is off) · l opens the log", theme::muted())),
            Some(Ok(errors)) => {
                for e in errors {
                    lines.push(Line::styled(e.clone(), Style::new().fg(theme::ERR)));
                }
                lines.push(Line::styled("l opens the whole log", theme::muted()));
            }
        }
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((v.scroll, 0)), area);
    }

    fn render_tables(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let Some(v) = &self.open else { return };
        let [bar, rest] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let shown = v.shown();
        let total = match &v.tables {
            Some(Ok(list)) => list.len(),
            _ => 0,
        };
        let mut spans = vec![Span::styled(format!(" f {} ", v.table_filter.label()), theme::accent()), Span::styled(format!("· {} of {total} ", shown.len()), theme::muted())];
        if !v.marked.is_empty() {
            spans.push(Span::styled(format!("· {} marked (R reloads them) ", v.marked.len()), Style::new().fg(theme::WARN)));
        }
        let mut cursor = None;
        if v.searching {
            let (line, col) = v.search.line(" / ", "table name");
            cursor = Some(spans.iter().map(|s| s.content.chars().count()).sum::<usize>() as u16 + col);
            spans.extend(line.spans);
        } else if !v.search.value.is_empty() {
            spans.push(Span::styled(format!(" / {} ", v.search.value), theme::accent()));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let [list_area, detail] = if rest.width >= 120 {
            rest.layout(&Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]))
        } else {
            rest.layout(&Layout::vertical([Constraint::Fill(1), Constraint::Length(9)]))
        };
        self.list_area = list_area;
        match &v.tables {
            None => {
                f.render_widget(widgets::placeholder(Line::styled(format!("{} reading the tables…", theme::spinner(view.tick)), theme::muted())).block(widgets::panel(" Tables ", true)), list_area);
                return;
            }
            Some(Err(e)) => {
                f.render_widget(widgets::placeholder(Line::styled(e.short(), Style::new().fg(theme::ERR))).block(widgets::panel(" Tables ", true)), list_area);
                return;
            }
            Some(Ok(_)) if shown.is_empty() => {
                f.render_widget(widgets::placeholder(Line::styled("No table here · f changes the filter", theme::muted())).block(widgets::panel(" Tables ", true)), list_area);
                return;
            }
            Some(Ok(_)) => {}
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let with_validation = width >= 90;
        let rest_w = if with_validation { 64 } else { 44 };
        let name_w = shown.iter().map(|t| t.name().chars().count()).max().unwrap_or(10).clamp(10, width.saturating_sub(rest_w).max(10));
        let items: Vec<ListItem> = shown
            .iter()
            .map(|t| {
                let (mark, color) = state_mark(t);
                let marked = v.marked.contains(&(t.schema_name.clone(), t.table_name.clone()));
                let cdc = format!("{}/{}/{}", fmt_count(t.inserts as f64), fmt_count(t.updates as f64), fmt_count(t.deletes as f64));
                ListItem::new(Line::from(vec![
                    Span::styled(if marked { "▣ " } else { "  " }, Style::new().fg(theme::WARN)),
                    Span::styled(format!("{mark} "), Style::new().fg(color)),
                    Span::raw(format!("{:<name_w$} ", ellipsize(&t.name(), name_w))),
                    Span::styled(format!("{:<11}", t.state().label()), Style::new().fg(if t.state() == TableState::Error { theme::ERR } else { theme::MUTED })),
                    Span::raw(format!("{:>8} ", fmt_count(t.full_load_rows as f64))),
                    Span::styled(format!("{cdc:<18}"), theme::muted()),
                    if with_validation {
                        let style = if t.validation_problem() { Style::new().fg(theme::WARN) } else { theme::muted() };
                        Span::styled(ellipsize(&t.validation_label(), 22), style)
                    } else {
                        Span::raw("")
                    },
                ]))
            })
            .collect();
        let columns = if with_validation { "· state · loaded rows · CDC ins/upd/del · validation " } else { "· state · loaded rows · CDC ins/upd/del " };
        let title = Line::from(vec![Span::raw(" Tables "), Span::styled(columns, theme::muted())]);
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        let selected = v.selected_table().cloned();
        if let Some(v) = &mut self.open {
            f.render_stateful_widget(list, list_area, &mut v.table_state);
        }
        let Some(t) = selected else { return };
        let mut lines = vec![
            widgets::kv("State", t.table_state.clone(), KW),
            widgets::kv("Loaded rows", format!("{} ({} with errors)", t.full_load_rows, t.full_load_error_rows), KW),
            widgets::kv("Changes", format!("{} inserts · {} updates · {} deletes · {} DDL", t.inserts, t.updates, t.deletes, t.ddls), KW),
        ];
        let validation_style = if t.validation_problem() { Style::new().fg(theme::WARN) } else { Style::new() };
        lines.push(Line::from(vec![Span::styled(format!("{:<KW$} ", "Validation"), theme::muted()), Span::styled(t.validation_label(), validation_style)]));
        if let Some(d) = &t.validation_state_details {
            lines.push(Line::styled(format!("{:<KW$} {d}", ""), Style::new().fg(theme::WARN)));
        }
        if let (Some(s), Some(e)) = (t.full_load_start_time, t.full_load_end_time) {
            lines.push(widgets::kv("Loaded", format!("{} → {}", fmt_utc(s), fmt_utc(e)), KW));
        }
        if let Some(u) = t.last_update_time {
            lines.push(widgets::kv("Last change", fmt_ago(u), KW));
        }
        f.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(widgets::panel(Line::from(format!(" {} ", t.name())).bold(), false)),
            detail,
        );
    }

    fn render_settings(&self, f: &mut Frame, area: Rect, t: &Replication) {
        let Some(v) = &self.open else { return };
        let mut lines = Vec::new();
        match &t.settings {
            Some(settings) => {
                lines.push(widgets::section("Settings"));
                for (k, value) in dms::settings_summary(settings) {
                    lines.push(widgets::kv(&k, value, KW));
                }
                let prep = PrepMode::of(Some(settings));
                lines.push(Line::raw(""));
                lines.push(Line::styled(format!("A full reload (R) {}.", prep.effect()), Style::new().fg(theme::WARN)));
            }
            None => lines.push(Line::styled("Reading the settings…", theme::muted())),
        }
        if let Some(mappings) = &t.table_mappings {
            lines.push(Line::raw(""));
            lines.push(widgets::section("Table mappings"));
            for rule in dms::mapping_lines(mappings) {
                lines.push(Line::raw(format!("  {rule}")));
            }
        }
        lines.push(Line::raw(""));
        lines.push(widgets::section("Identity"));
        lines.push(widgets::kv("ARN", t.arn.clone(), KW));
        lines.push(widgets::kv("Resource id", t.resource_id.clone(), KW));
        if let Some((group, stream)) = t.log_location() {
            lines.push(widgets::kv("Log", format!("{group} / {stream}"), KW));
        }
        if let Some(cp) = &t.recovery_checkpoint {
            lines.push(widgets::kv("Checkpoint", cp.clone(), KW));
        }
        if let Some(p) = &t.cdc_start_position {
            lines.push(widgets::kv("CDC start", p.clone(), KW));
        }
        if let Some(c) = t.created {
            lines.push(widgets::kv("Created", format!("{} · {}", fmt_ago(c), fmt_utc(c)), KW));
        }
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((v.scroll, 0)), area);
    }

    // ---- dialogs -----------------------------------------------------------------------------

    pub(super) fn render_dialog(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let color = if is_production(&self.env_name) { theme::ERR } else { theme::ACCENT };
        match &mut self.dialog {
            None => {}
            Some(Dialog::ReloadWhat(picker, ..) | Dialog::Classes(picker, ..) | Dialog::TestFrom(picker, ..)) => {
                let rect = widgets::popup_area(area, 76, picker.height(area.height.saturating_sub(4)));
                picker.render(f, rect, color);
            }
            Some(Dialog::ReloadTables { form, .. } | Dialog::Scale { form, .. }) => {
                let rect = widgets::popup_area(area, 84, area.height.saturating_sub(4).min(18));
                form.render(f, rect, color);
            }
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
                    text.push(Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw("Working…")]));
                } else {
                    text.push(widgets::hints_line(&[("Enter", "confirm"), ("Esc", "cancel")]));
                }
                let rect = widgets::popup_area(area, 84, widgets::wrapped_height(&text, 80) + 2);
                widgets::popup(f, rect, title, color, Text::from(text));
                if let Some((row, col)) = cursor {
                    f.set_cursor_position((rect.x + 2 + col, rect.y + 1 + row));
                }
            }
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// `now 38 s · max 1.2 min`
fn last_max(m: &Metrics, id: &str, fmt: fn(f64) -> String) -> String {
    match (m.last(id), m.max(id)) {
        (Some(l), Some(x)) => format!("now {} · max {}", fmt(l), fmt(x)),
        _ => "no data".into(),
    }
}

/// The target's CDC latency with the part spent reading the source inside each bar.
fn latency_chart(m: &Metrics, width: usize, rows: usize) -> Vec<Line<'static>> {
    if m.points("latency_target").is_empty() {
        return vec![Line::from(vec![
            Span::styled("CDC latency", Style::new().fg(theme::RUNNING).bold()),
            Span::styled("  none reported in this window: no changes are being replicated", theme::muted()),
        ])];
    }
    let mut chart = Chart::new("CDC latency", m.points("latency_target"), m.window);
    chart.agg = Agg::Max;
    chart.rows = rows;
    chart.color = theme::RUNNING;
    chart.stacked = Some(m.points("latency_source"));
    chart.stacked_color = theme::ACCENT;
    chart.fmt = dms::fmt_secs;
    chart.threshold = Some(300.0);
    chart.axis = false;
    chart.summary = vec![Span::styled(
        format!("target {} · source {}", last_max(m, "latency_target", dms::fmt_secs), last_max(m, "latency_source", dms::fmt_secs)),
        theme::muted(),
    )];
    chart.lines(width)
}

/// Freeable memory, swap and CPU of an instance (series `i_…`).
fn instance_charts(m: &Metrics, memory: Option<f64>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut free = Chart::new("Freeable memory", m.points("i_freeable"), m.window);
    free.agg = Agg::Mean;
    free.rows = 2;
    free.color = theme::APPROVAL;
    free.fmt = short_bytes;
    free.axis = false;
    let of = memory.map(|total| format!(" of {}", bytes(total))).unwrap_or_default();
    let low = match (m.last("i_freeable"), memory) {
        (Some(f), Some(total)) => f < total * 0.1,
        _ => false,
    };
    free.summary = vec![Span::styled(
        format!("now {}{of} · least {}", m.last("i_freeable").map(bytes).unwrap_or_else(|| "—".into()), m.min("i_freeable").map(bytes).unwrap_or_else(|| "—".into())),
        if low { Style::new().fg(theme::ERR) } else { theme::muted() },
    )];
    out.extend(free.lines(width));
    let swap = m.max("i_swap").unwrap_or(0.0);
    if swap > 0.0 {
        let mut chart = Chart::new("Swap", m.points("i_swap"), m.window);
        chart.agg = Agg::Max;
        chart.rows = 2;
        chart.color = theme::WARN;
        chart.fmt = short_bytes;
        chart.axis = false;
        chart.summary = vec![Span::styled(last_max(m, "i_swap", bytes), Style::new().fg(theme::WARN))];
        out.extend(chart.lines(width));
    } else {
        out.push(Line::from(vec![Span::styled("Swap", Style::new().fg(theme::WARN).bold()), Span::styled("  none", theme::muted())]));
    }
    let mut cpu = Chart::new("CPU", m.points("i_cpu"), m.window);
    cpu.agg = Agg::Max;
    cpu.rows = 2;
    cpu.color = theme::OK;
    cpu.fmt = percent;
    cpu.threshold = Some(90.0);
    cpu.summary = vec![Span::styled(last_max(m, "i_cpu", percent), theme::muted())];
    out.extend(cpu.lines(width));
    out
}

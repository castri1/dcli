//! Savings: what could cost less in the current environment, from its resources and their
//! usage over the last days, measured in the units AWS bills (dcli keeps no prices). The
//! scan runs when the module opens, every service in parallel; each finding shows its
//! evidence and caveats and opens its resource, and two kinds can be applied here: a log
//! group's retention and a queue's long polling. The Lambda memory analysis runs on demand
//! because Logs Insights bills the data it scans: it says how much first.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{filter_bar, matches_filter};
use crate::aws::{self, AwsError, logs, sqs};
use crate::config::is_production;
use crate::savings::lambda::{self as memory, Estimate, MemoryRun};
use crate::savings::{self, CHECKS, DEFAULT_DAYS, Finding, Fix, Impact, Outcome, short_name};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, now_secs};

/// Retention choices offered for a log group (CloudWatch accepts only some numbers).
const RETENTIONS: [&str; 6] = ["7 days", "30 days", "90 days", "180 days", "1 year", "2 years"];
/// Long polling wait times offered.
const WAITS: [&str; 3] = ["20 seconds", "10 seconds", "5 seconds"];
/// Windows offered for the memory analysis.
const WINDOWS: [&str; 4] = ["14 days", "7 days", "3 days", "1 day"];

enum Msg {
    Checked { service: &'static str, result: aws::Result<Vec<Finding>> },
    Scanned,
    Estimated(aws::Result<Estimate>),
    MemoryProgress(usize, usize),
    Memory(aws::Result<MemoryRun>),
    Applied { key: String, result: aws::Result<String> },
}

enum Check {
    Running,
    Done(usize),
    Failed(AwsError),
}

enum MemoryState {
    Idle,
    Estimating,
    Running(usize, usize),
    Done { functions: usize, bytes: f64, at: f64, silent: usize },
    Failed(String),
}

enum Dialog {
    Fix { key: String, fix: Fix, form: Form, busy: bool },
    Memory { estimate: Estimate, form: Form },
}

pub struct Savings {
    env_name: String,
    region: String,
    days: u32,
    /// Each service of the scan and how it went, in the order of [`CHECKS`].
    checks: Vec<(&'static str, Check)>,
    scanning: bool,
    /// When the last scan ended (Unix seconds).
    scanned_at: Option<f64>,
    found: Vec<Finding>,
    /// Findings of the Lambda memory analysis.
    memory_found: Vec<Finding>,
    memory: MemoryState,
    /// Everything found, most important first; `rows` are the shown ones.
    all: Vec<Finding>,
    rows: Vec<usize>,
    list_state: ListState,
    filter: TextInput,
    filtering: bool,
    service: Option<&'static str>,
    min_impact: Impact,
    detail_scroll: u16,
    dialog: Option<Dialog>,
    /// Findings whose fix was applied (see [`key_of`]).
    applied: HashSet<String>,
    list_area: Rect,
    /// The user moved through the list during the scan: findings arriving keep the selection.
    touched: bool,
}

/// Names a finding across scans.
fn key_of(f: &Finding) -> String {
    format!("{}\u{0}{}\u{0}{}", f.service, f.target, f.summary)
}

fn impact_color(impact: Impact) -> Color {
    match impact {
        Impact::High => theme::ERR,
        Impact::Medium => theme::WARN,
        Impact::Low => theme::ACCENT,
    }
}

/// `30 days` → 30, `1 year` → 365 (the retention values CloudWatch accepts).
fn retention_days(choice: &str) -> u32 {
    match choice {
        "1 year" => 365,
        "2 years" => 731,
        c => c.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(30),
    }
}

fn leading_number(choice: &str) -> u32 {
    choice.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// The confirmation of a memory analysis: what it reads and how much Insights would scan.
/// Shared with the Lambda module.
pub(super) fn memory_form(estimate: &Estimate, title: String) -> Form {
    let window = format!("{} days", estimate.days);
    let selected = if WINDOWS.contains(&window.as_str()) { window } else { WINDOWS[0].to_string() };
    let mut fields = vec![
        Field::fixed("Functions", estimate.functions.len().to_string()),
        Field::choice("Window", &WINDOWS, &selected),
        Field::fixed("Logs to scan", String::new()),
    ];
    if !estimate.shared.is_empty() {
        fields.insert(1, Field::fixed("Left out", format!("{} sharing a log group", estimate.shared.len())));
    }
    let mut form = Form::new(title, fields).submit_label("analyse").enter_submits().focus_on("Window");
    form.note = Some(
        "Logs Insights reads the whole log groups over the window and bills the data it scans; the query keeps the \
         REPORT line of each invocation (memory used, billed time)."
            .into(),
    );
    update_memory_form(&mut form, estimate);
    form
}

/// The estimate for the window chosen in the form.
pub(super) fn chosen_estimate(form: &Form, estimate: &Estimate) -> Estimate {
    estimate.with_days(leading_number(form.get("Window")).max(1))
}

/// Shows the bytes to scan for the window chosen.
pub(super) fn update_memory_form(form: &mut Form, estimate: &Estimate) {
    let chosen = chosen_estimate(form, estimate);
    if let Some(field) = form.field_mut("Logs to scan") {
        field.input = TextInput::with_value(format!("≈ {} (what the groups received)", savings::size(chosen.bytes)));
    }
}

impl Savings {
    pub fn new() -> Savings {
        Savings {
            env_name: String::new(),
            region: String::new(),
            days: DEFAULT_DAYS,
            checks: Vec::new(),
            scanning: false,
            scanned_at: None,
            found: Vec::new(),
            memory_found: Vec::new(),
            memory: MemoryState::Idle,
            all: Vec::new(),
            rows: Vec::new(),
            list_state: ListState::default(),
            filter: TextInput::default(),
            filtering: false,
            service: None,
            min_impact: Impact::Low,
            detail_scroll: 0,
            dialog: None,
            applied: HashSet::new(),
            list_area: Rect::default(),
            touched: false,
        }
    }

    fn scan(&mut self, ctx: &mut Ctx) {
        if self.scanning {
            return ctx.toast(Level::Info, "Still scanning");
        }
        self.scanning = true;
        self.touched = false;
        self.found.clear();
        self.checks = CHECKS.iter().map(|(service, _)| (*service, Check::Running)).collect();
        self.rebuild();
        let (aws, days) = (ctx.env().aws.clone(), self.days);
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            savings::run(&aws, days, &[], &|service, result| {
                em.emit(Msg::Checked { service, result: result.clone() });
            });
            em.emit(Msg::Scanned);
        });
    }

    fn selected(&self) -> Option<&Finding> {
        self.all.get(*self.rows.get(self.list_state.selected()?)?)
    }

    /// Merges scan and memory findings and applies the filters, keeping the selection.
    fn rebuild(&mut self) {
        let keep = self.selected().map(key_of);
        self.all = self.found.iter().chain(&self.memory_found).cloned().collect();
        savings::sort(&mut self.all);
        let query = self.filter.value.trim().to_lowercase();
        self.rows = (0..self.all.len())
            .filter(|&i| {
                let f = &self.all[i];
                f.impact >= self.min_impact
                    && self.service.is_none_or(|s| s == f.service)
                    && (query.is_empty()
                        || matches_filter(&f.resource, &query)
                        || matches_filter(&f.summary, &query)
                        || matches_filter(f.service, &query))
            })
            .collect();
        let index = keep.and_then(|k| self.rows.iter().position(|&i| key_of(&self.all[i]) == k)).unwrap_or(0);
        self.list_state.select(if self.rows.is_empty() { None } else { Some(index.min(self.rows.len() - 1)) });
    }

    fn select(&mut self, index: usize) {
        if self.rows.is_empty() {
            return;
        }
        let index = index.min(self.rows.len() - 1);
        if self.list_state.selected() != Some(index) {
            self.detail_scroll = 0;
        }
        self.list_state.select(Some(index));
    }

    /// Services found so far, for the service filter.
    fn next_service(&mut self) {
        let mut present: Vec<&'static str> = Vec::new();
        for (service, _) in CHECKS {
            if self.all.iter().any(|f| f.service == *service) {
                present.push(service);
            }
        }
        self.service = match self.service.and_then(|s| present.iter().position(|p| *p == s)) {
            None => present.first().copied(),
            Some(i) => present.get(i + 1).copied(),
        };
        self.rebuild();
        self.select(0);
    }

    fn next_impact(&mut self) {
        self.min_impact = match self.min_impact {
            Impact::Low => Impact::Medium,
            Impact::Medium => Impact::High,
            Impact::High => Impact::Low,
        };
        self.rebuild();
        self.select(0);
    }

    fn open_fix(&mut self, ctx: &mut Ctx) {
        let Some(f) = self.selected() else { return };
        let Some(fix) = f.fix.clone() else {
            return ctx.toast(Level::Info, "Nothing dcli can apply for this one: see the recommendation");
        };
        let key = key_of(f);
        if self.applied.contains(&key) {
            return ctx.toast(Level::Info, "Already applied");
        }
        let mut form = match &fix {
            Fix::LogRetention { group } => {
                let mut form = Form::new(
                    "Log retention",
                    vec![Field::fixed("Log group", group.clone()), Field::choice("Keep", &RETENTIONS, "30 days")],
                );
                form.note = Some("Events older than this are deleted, for good. Export to S3 first what must be kept.".into());
                form
            }
            Fix::LongPolling { queue_url } => {
                let mut form = Form::new(
                    "Long polling",
                    vec![Field::fixed("Queue", sqs::queue_name(queue_url).to_string()), Field::choice("Wait up to", &WAITS, "20 seconds")],
                );
                form.note = Some(
                    "Receives that do not set their own WaitTimeSeconds wait for messages this long instead of answering \
                     empty at once."
                        .into(),
                );
                form
            }
        }
        .submit_label("apply");
        if is_production(&self.env_name) {
            form = form.confirm_with(&self.env_name);
        }
        self.dialog = Some(Dialog::Fix { key, fix, form, busy: false });
    }

    fn submit_fix(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Fix { key, fix, form, busy }) = &mut self.dialog else { return };
        *busy = true;
        let (aws, key, fix) = (ctx.env().aws.clone(), key.clone(), fix.clone());
        let (days, wait) = (retention_days(form.get("Keep")), leading_number(form.get("Wait up to")));
        ctx.spawn(move || {
            let result = match &fix {
                Fix::LogRetention { group } => logs::put_retention_policy(&aws, group, days)
                    .map(|()| format!("{group} keeps {days} days of events")),
                Fix::LongPolling { queue_url } => {
                    sqs::set_attributes(&aws, queue_url, &[("ReceiveMessageWaitTimeSeconds", &wait.to_string())])
                        .map(|()| format!("{} waits up to {wait} s for messages", sqs::queue_name(queue_url)))
                }
            };
            Msg::Applied { key, result }
        });
    }

    fn start_estimate(&mut self, ctx: &mut Ctx) {
        if matches!(self.memory, MemoryState::Estimating | MemoryState::Running(..)) {
            return ctx.toast(Level::Info, "The memory analysis is already running");
        }
        self.memory = MemoryState::Estimating;
        let (aws, days) = (ctx.env().aws.clone(), self.days);
        ctx.spawn(move || Msg::Estimated(memory::memory_estimate(&aws, None, days)));
    }

    fn submit_memory(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Memory { estimate, form }) = self.dialog.take() else { return };
        let estimate = chosen_estimate(&form, &estimate);
        self.memory = MemoryState::Running(0, estimate.functions.len());
        let aws = ctx.env().aws.clone();
        ctx.toast(Level::Info, format!("Analysing memory: {}", estimate.describe()));
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let result = memory::memory_analysis(&aws, &estimate, &|done, total| {
                em.emit(Msg::MemoryProgress(done, total));
            });
            em.emit(Msg::Memory(result));
        });
    }

    fn on_memory(&mut self, result: aws::Result<MemoryRun>, ctx: &mut Ctx) {
        let run = match result {
            Ok(run) => run,
            Err(e) => {
                ctx.toast(Level::Error, format!("Memory analysis failed: {}", e.short()));
                self.memory = MemoryState::Failed(e.short());
                return;
            }
        };
        self.memory_found = run.uses.iter().filter_map(memory::memory_finding).map(|f| Finding { service: "Lambda", ..f }).collect();
        ctx.toast(
            Level::Success,
            format!(
                "Memory of {} functions read · {} scanned · {} could use less",
                run.uses.len(),
                savings::size(run.bytes_scanned),
                self.memory_found.len()
            ),
        );
        self.memory = MemoryState::Done { functions: run.uses.len(), bytes: run.bytes_scanned, at: now_secs(), silent: run.silent.len() };
        self.rebuild();
    }

    fn outcome(&self) -> Outcome {
        let errors = self
            .checks
            .iter()
            .filter_map(|(s, c)| match c {
                Check::Failed(e) => Some((*s, e.clone())),
                _ => None,
            })
            .collect();
        Outcome { findings: self.all.clone(), errors, days: self.days, at: self.scanned_at.unwrap_or_else(now_secs) }
    }

    fn copy_report(&self, ctx: &mut Ctx) {
        if self.scanning {
            return ctx.toast(Level::Info, "Wait for the scan to finish");
        }
        let md = savings::report(&self.env_name, &self.region, &self.outcome());
        if crate::platform::copy_to_clipboard(&md) {
            return ctx.toast(Level::Success, "Copied the savings plan. Paste it into your AI agent.");
        }
        match crate::platform::write_temp_file(&format!("savings-{}", self.env_name), &md) {
            Ok(path) => ctx.toast(Level::Success, format!("Saved the savings plan to {}", path.display())),
            Err(e) => ctx.toast(Level::Error, format!("Could not save the plan: {e}")),
        }
    }

    fn copy_finding(&self, ctx: &mut Ctx) {
        let Some(f) = self.selected() else { return };
        let mut text = format!(
            "{} ({}, {} impact)\nFound: {}\nChange: {}\nReduction: {}\n",
            f.resource,
            f.service,
            f.impact.label(),
            f.summary,
            f.recommendation,
            f.reduction
        );
        for (title, lines) in [("Evidence", &f.evidence), ("Caveats", &f.caveats)] {
            if !lines.is_empty() {
                text.push_str(&format!("{title}:\n"));
                for l in lines {
                    text.push_str(&format!("  - {l}\n"));
                }
            }
        }
        let ok = crate::platform::copy_to_clipboard(&text);
        ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Finding copied" } else { "Clipboard not available" });
    }

    /// The scan's summary, each service's state, the memory analysis and the errors.
    fn status_lines(&self, view: &View) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let (high, medium, low) = self.all.iter().fold((0, 0, 0), |(h, m, l), f| match f.impact {
            Impact::High => (h + 1, m, l),
            Impact::Medium => (h, m + 1, l),
            Impact::Low => (h, m, l + 1),
        });
        let mut head = vec![Span::styled(format!(" Last {} days", self.days), theme::bold())];
        match (self.scanning, self.scanned_at) {
            (true, _) => head.push(Span::styled(format!(" · {} scanning", theme::spinner(view.tick)), theme::accent())),
            (false, Some(at)) => head.push(Span::styled(format!(" · scanned {}", fmt_ago(at)), theme::muted())),
            _ => {}
        }
        head.push(Span::raw(format!("   {} findings  ", self.all.len())));
        head.push(Span::styled(format!("●●● {high}  "), Style::new().fg(theme::ERR)));
        head.push(Span::styled(format!("●●○ {medium}  "), Style::new().fg(theme::WARN)));
        head.push(Span::styled(format!("●○○ {low}"), Style::new().fg(theme::ACCENT)));
        head.push(Span::styled("   no prices: reductions are in the units AWS bills", theme::muted()));
        lines.push(Line::from(head));

        let mut chips = vec![Span::raw(" ")];
        for (service, check) in &self.checks {
            let (symbol, style, count) = match check {
                Check::Running => (theme::spinner(view.tick).to_string(), theme::accent(), String::new()),
                Check::Done(n) => ("✓".to_string(), Style::new().fg(theme::OK), format!(" {n}")),
                Check::Failed(_) => ("✗".to_string(), Style::new().fg(theme::ERR), String::new()),
            };
            chips.push(Span::styled(symbol, style));
            chips.push(Span::raw(format!(" {}{count}   ", short_name(service))));
        }
        lines.push(Line::from(chips));

        let memory = match &self.memory {
            MemoryState::Idle => Span::styled("Lambda memory: not read · m reads it with Logs Insights (billed per GB scanned)", theme::muted()),
            MemoryState::Estimating => Span::styled(format!("Lambda memory: {} measuring the logs to scan…", theme::spinner(view.tick)), theme::accent()),
            MemoryState::Running(done, total) => Span::styled(
                format!("Lambda memory: {} reading the REPORT lines ({done} of {total} log groups)…", theme::spinner(view.tick)),
                theme::accent(),
            ),
            MemoryState::Done { functions, bytes, at, silent } => {
                let silent = if *silent > 0 { format!(", {silent} without invocations") } else { String::new() };
                Span::styled(
                    format!("Lambda memory: {functions} functions read{silent} · {} scanned · {}", savings::size(*bytes), fmt_ago(*at)),
                    theme::muted(),
                )
            }
            MemoryState::Failed(e) => Span::styled(format!("Lambda memory: {e}"), Style::new().fg(theme::ERR)),
        };
        lines.push(Line::from(vec![Span::raw(" "), memory]));
        for (service, check) in &self.checks {
            if let Check::Failed(e) = check {
                lines.push(Line::from(vec![
                    Span::styled(format!(" ✗ {service}: "), Style::new().fg(theme::ERR)),
                    Span::styled(e.short(), theme::muted()),
                ]));
            }
        }
        lines
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search resources and findings");
        let mut bar_line = line;
        let service = self.service.map_or("all".to_string(), |s| short_name(s).to_string());
        let impact = match self.min_impact {
            Impact::Low => "all",
            Impact::Medium => "medium and high",
            Impact::High => "high",
        };
        bar_line.spans.push(Span::styled(format!("   s service: {service} · i impact: {impact}"), theme::muted()));
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = if self.rows.len() == self.all.len() {
            format!(" {} findings ", self.all.len())
        } else {
            format!(" {} of {} findings ", self.rows.len(), self.all.len())
        };
        if self.rows.is_empty() {
            let text = if self.scanning {
                Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" looking at the resources…")])
            } else if self.all.is_empty() && self.scanned_at.is_some() {
                Line::styled("Nothing found to change", Style::new().fg(theme::OK))
            } else {
                Line::styled("No finding matches the filters", theme::muted())
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        // Columns as wide as what they show, within limits; the finding takes the rest.
        let widest = |text: fn(&Finding) -> &str| self.rows.iter().map(|&i| text(&self.all[i]).chars().count()).max().unwrap_or(0);
        let reduction_w = widest(|f| &f.reduction).clamp(12, (width / 3).max(12));
        let rest = width.saturating_sub(4 + 9 + reduction_w + 2);
        let resource_w = widest(|f| &f.resource).clamp(8, (rest / 3).max(8));
        let summary_w = rest.saturating_sub(resource_w + 1);
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|&i| {
                let f = &self.all[i];
                let applied = self.applied.contains(&key_of(f));
                let dots = if applied { Span::styled("✓  ", Style::new().fg(theme::OK)) } else { Span::styled(f.impact.dots(), Style::new().fg(impact_color(f.impact))) };
                let dim = if applied { theme::muted() } else { Style::new() };
                ListItem::new(Line::from(vec![
                    dots,
                    Span::raw(" "),
                    Span::styled(format!("{:<8} ", ellipsize(short_name(f.service), 8)), theme::muted()),
                    Span::styled(format!("{:<resource_w$} ", ellipsize(&f.resource, resource_w)), dim.bold()),
                    Span::styled(format!("{:<summary_w$}", ellipsize(&f.summary, summary_w)), dim),
                    Span::raw("  "),
                    Span::styled(format!("{:>reduction_w$}", ellipsize(&f.reduction, reduction_w)), theme::accent()),
                ]))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect) {
        let Some(found) = self.selected() else {
            f.render_widget(widgets::placeholder(Line::styled("", theme::muted())).block(widgets::panel(" Finding ", false)), area);
            return;
        };
        let kw = 11;
        let color = impact_color(found.impact);
        let mut lines = vec![Line::from(vec![
            Span::styled(format!("{} {} impact", found.impact.dots(), found.impact.label()), Style::new().fg(color).bold()),
            Span::styled(format!(" · {}", found.service), theme::muted()),
        ])];
        lines.push(widgets::kv("Found", found.summary.clone(), kw));
        lines.push(widgets::kv("Change", Span::styled(found.recommendation.clone(), theme::bold()), kw));
        lines.push(widgets::kv("Reduction", Span::styled(found.reduction.clone(), theme::accent()), kw));
        if !found.evidence.is_empty() {
            lines.push(Line::raw(""));
            lines.push(widgets::section("Evidence"));
            lines.extend(found.evidence.iter().map(|e| Line::from(vec![Span::styled("· ", theme::muted()), Span::raw(e.clone())])));
        }
        if !found.caveats.is_empty() {
            lines.push(Line::raw(""));
            lines.push(widgets::section("Caveats"));
            lines.extend(found.caveats.iter().map(|c| Line::from(vec![Span::styled("! ", Style::new().fg(theme::WARN)), Span::raw(c.clone())])));
        }
        lines.push(Line::raw(""));
        let mut actions = Vec::new();
        if !found.target.is_empty() {
            actions.push(format!("Enter opens it in {}", found.module));
        }
        if self.applied.contains(&key_of(found)) {
            actions.push("✓ applied".to_string());
        } else if found.fix.is_some() {
            actions.push("a applies the change".to_string());
        }
        lines.push(Line::styled(actions.join(" · "), theme::muted()));
        let block = widgets::panel(Line::from(format!(" {} ", found.resource)).bold(), false);
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
    }

    fn render_dialog(&self, f: &mut Frame, area: Rect, view: &View) {
        match &self.dialog {
            Some(Dialog::Fix { form, busy, .. }) => {
                let color = if is_production(&self.env_name) { theme::ERR } else { theme::ACCENT };
                form.render(f, area, color);
                if *busy {
                    let body = Text::from(vec![
                        Line::raw(""),
                        Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw("Applying…")]),
                    ]);
                    widgets::popup(f, widgets::popup_area(area, 30, 5), "Savings", theme::ACCENT, body);
                }
            }
            Some(Dialog::Memory { form, .. }) => form.render(f, area, theme::ACCENT),
            None => {}
        }
    }
}

impl Module for Savings {
    fn title(&self) -> &'static str {
        "Savings"
    }

    fn description(&self) -> &'static str {
        "What could cost less · unused or oversized resources, in the units AWS bills"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Savings::new();
        self.env_name = ctx.env().name.clone();
        self.region = ctx.env().region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if self.scanned_at.is_none() && !self.scanning {
            self.scan(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        match &mut self.dialog {
            Some(Dialog::Fix { form, busy, .. }) => {
                if !*busy {
                    match form.handle(key) {
                        FormEvent::Submit => self.submit_fix(ctx),
                        FormEvent::Cancel => self.dialog = None,
                        FormEvent::Pending => {}
                    }
                }
                return Handled::Yes;
            }
            Some(Dialog::Memory { estimate, form }) => {
                match form.handle(key) {
                    FormEvent::Submit => self.submit_memory(ctx),
                    FormEvent::Cancel => {
                        self.dialog = None;
                        self.memory = MemoryState::Idle;
                    }
                    FormEvent::Pending => update_memory_form(form, estimate),
                }
                return Handled::Yes;
            }
            None => {}
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
            KeyCode::Up | KeyCode::Char('k') => {
                self.touched = true;
                self.select(selected.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.touched = true;
                self.select(selected + 1);
            }
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() || self.service.is_some() || self.min_impact != Impact::Low => {
                self.filter.clear();
                self.service = None;
                self.min_impact = Impact::Low;
                self.rebuild();
            }
            KeyCode::Char('s') => self.next_service(),
            KeyCode::Char('i') => self.next_impact(),
            KeyCode::Enter | KeyCode::Char('g') => {
                if let Some(found) = self.selected() {
                    if found.target.is_empty() {
                        ctx.toast(Level::Info, "This one names several resources: see the evidence");
                    } else {
                        ctx.jump(found.module, found.target.clone());
                    }
                }
            }
            KeyCode::Char('a') => self.open_fix(ctx),
            KeyCode::Char('m') => self.start_estimate(ctx),
            KeyCode::Char('c') => self.copy_report(ctx),
            KeyCode::Char('y') => self.copy_finding(ctx),
            KeyCode::Char('r') => self.scan(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        let pos = (event.column, event.row).into();
        if self.dialog.is_some() || !self.list_area.contains(pos) {
            return Handled::No;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                self.touched = true;
                // The filter line, then the list's border.
                let row = event.row.saturating_sub(self.list_area.y + 2) as usize;
                self.select(self.list_state.offset() + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Checked { service, result } => {
                let check = match result {
                    Ok(found) => {
                        let n = found.len();
                        self.found.extend(found);
                        Check::Done(n)
                    }
                    Err(e) => Check::Failed(e),
                };
                if let Some(slot) = self.checks.iter_mut().find(|(s, _)| *s == service) {
                    slot.1 = check;
                }
                self.rebuild();
                if !self.touched {
                    self.select(0);
                }
            }
            Msg::Scanned => {
                self.scanning = false;
                self.scanned_at = Some(now_secs());
                if let Some((_, Check::Failed(e))) = self.checks.iter().find(|(_, c)| matches!(c, Check::Failed(e) if e.is_login_required())) {
                    ctx.toast(Level::Warn, e.short());
                }
            }
            Msg::Estimated(result) => match result {
                Ok(estimate) if estimate.functions.is_empty() => {
                    self.memory = MemoryState::Idle;
                    ctx.toast(Level::Info, "No function to analyse");
                }
                Ok(estimate) => {
                    self.memory = MemoryState::Idle;
                    let form = memory_form(&estimate, "Lambda memory analysis".into());
                    self.dialog = Some(Dialog::Memory { estimate, form });
                }
                Err(e) => {
                    self.memory = MemoryState::Failed(e.short());
                    ctx.toast(Level::Error, format!("Could not measure the logs: {}", e.short()));
                }
            },
            Msg::MemoryProgress(done, total) => {
                if matches!(self.memory, MemoryState::Running(..)) {
                    self.memory = MemoryState::Running(done, total);
                }
            }
            Msg::Memory(result) => self.on_memory(result, ctx),
            Msg::Applied { key, result } => {
                let Some(Dialog::Fix { form, busy, .. }) = &mut self.dialog else { return };
                *busy = false;
                match result {
                    Ok(message) => {
                        self.dialog = None;
                        self.applied.insert(key);
                        ctx.toast(Level::Success, message);
                    }
                    Err(e) => form.fail(e.short()),
                }
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let status = self.status_lines(view);
        let height = widgets::wrapped_height(&status, area.width).min(area.height / 3);
        let [top, body] = area.layout(&Layout::vertical([Constraint::Length(height), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }), top);
        let [list, detail] = body.layout(&Layout::vertical([Constraint::Percentage(55), Constraint::Fill(1)]));
        self.render_list(f, list, view);
        self.render_detail(f, detail);
        self.render_dialog(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.dialog {
            Some(Dialog::Fix { .. }) => return vec![("Enter", "apply"), ("Esc", "cancel")],
            Some(Dialog::Memory { .. }) => return vec![("←→", "window"), ("Enter", "analyse"), ("Esc", "cancel")],
            None => {}
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        vec![
            ("↑↓", "move"),
            ("Enter", "open resource"),
            ("a", "apply fix"),
            ("m", "Lambda memory"),
            ("s", "service"),
            ("i", "impact"),
            ("/", "search"),
            ("c", "plan for AI"),
            ("y", "copy finding"),
            ("r", "rescan"),
            ("PgUp/PgDn", "scroll"),
        ]
    }

    fn animating(&self) -> bool {
        self.scanning
            || matches!(self.memory, MemoryState::Estimating | MemoryState::Running(..))
            || matches!(self.dialog, Some(Dialog::Fix { busy: true, .. }))
    }

    fn capturing(&self) -> bool {
        self.filtering || self.dialog.is_some()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if self.dialog.is_none() && self.filtering {
            self.filter.insert_str(text, false);
            self.rebuild();
        } else if let Some(Dialog::Fix { form, busy: false, .. } | Dialog::Memory { form, .. }) = &mut self.dialog {
            form.paste(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_choices_are_values_cloudwatch_accepts() {
        let accepted = [1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192, 2557, 2922, 3288, 3653];
        for choice in RETENTIONS {
            assert!(accepted.contains(&retention_days(choice)), "{choice}");
        }
        assert_eq!(leading_number("20 seconds"), 20);
        assert_eq!(leading_number("7 days"), 7);
    }
}

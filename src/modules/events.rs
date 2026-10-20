//! EventBridge explorer: event buses, their rules (pattern or schedule, state) and each
//! rule's targets, with jumps to the Lambda or Step Functions behind a target, and test
//! events checked against a rule's pattern before they are sent.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use serde_json::Value;

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::events::{self, EventBus, PutResult, Rule, Target};
use crate::aws;
use crate::config::is_production;
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::ellipsize;

const DEBOUNCE: Duration = Duration::from_millis(200);

enum Msg {
    Buses(aws::Result<Vec<EventBus>>),
    Rules { bus: String, result: aws::Result<Vec<Rule>> },
    Detail { bus: String, name: String, result: Box<aws::Result<(Rule, Vec<Target>)>> },
    /// TestEventPattern for the values identified by `signature`.
    Checked { signature: String, result: aws::Result<bool> },
    Sent { bus: String, result: aws::Result<PutResult> },
}

#[derive(PartialEq)]
enum Focus {
    List,
    Targets,
}

/// The test event form, and the rule it is checked against.
struct Sending {
    form: Form,
    bus: String,
    /// (rule name, pattern, account) when a rule with a pattern was selected.
    rule: Option<(String, String, String)>,
    /// Values last checked against the pattern, and the result.
    checked: Option<(String, bool)>,
    busy: bool,
}

pub struct EventBridge {
    buses: Vec<EventBus>,
    buses_load: LoadState,
    /// The open bus; `None` lists the buses.
    bus: Option<String>,
    rules: Vec<Rule>,
    rules_load: LoadState,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    list_state: ListState,
    details: HashMap<String, aws::Result<(Rule, Vec<Target>)>>,
    in_flight: HashSet<String>,
    pending: Option<(String, Instant)>,
    focus: Focus,
    target: usize,
    sending: Option<Sending>,
    env_name: String,
    region: String,
    detail_scroll: u16,
    list_area: Rect,
}

impl EventBridge {
    pub fn new() -> EventBridge {
        EventBridge {
            buses: Vec::new(),
            buses_load: LoadState::Idle,
            bus: None,
            rules: Vec::new(),
            rules_load: LoadState::Idle,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            details: HashMap::new(),
            in_flight: HashSet::new(),
            pending: None,
            focus: Focus::List,
            target: 0,
            sending: None,
            env_name: String::new(),
            region: String::new(),
            detail_scroll: 0,
            list_area: Rect::default(),
        }
    }

    fn load_buses(&mut self, ctx: &mut Ctx) {
        self.buses_load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Buses(events::list_event_buses(&aws)));
    }

    fn open_bus(&mut self, bus: String, ctx: &mut Ctx) {
        self.bus = Some(bus.clone());
        self.rules.clear();
        self.details.clear();
        self.in_flight.clear();
        self.rules_load = LoadState::Loading;
        self.focus = Focus::List;
        self.filter.clear();
        self.filtering = false;
        self.refilter();
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = events::list_rules(&aws, &bus);
            Msg::Rules { bus, result }
        });
    }

    fn back(&mut self) {
        let Some(bus) = self.bus.take() else { return };
        self.rules.clear();
        self.details.clear();
        self.focus = Focus::List;
        self.filter.clear();
        self.refilter();
        if let Some(pos) = self.visible.iter().position(|&i| self.buses[i].name == bus) {
            self.list_state.select(Some(pos));
        }
    }

    /// Indexes of the current list (buses or rules) that match the filter.
    fn refilter(&mut self) {
        let query = self.filter.value.trim().to_lowercase();
        self.visible = match &self.bus {
            None => (0..self.buses.len()).filter(|&i| matches_filter(&self.buses[i].name, &query)).collect(),
            Some(_) => (0..self.rules.len())
                .filter(|&i| {
                    let r = &self.rules[i];
                    matches_filter(&format!("{} {}", r.name, r.summary()), &query)
                })
                .collect(),
        };
        self.list_state.select(if self.visible.is_empty() { None } else { Some(0) });
        self.detail_scroll = 0;
        self.queue_detail();
    }

    fn selected_bus(&self) -> Option<&EventBus> {
        if self.bus.is_some() {
            return None;
        }
        self.buses.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn selected_rule(&self) -> Option<&Rule> {
        self.bus.as_ref()?;
        self.rules.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn targets(&self) -> &[Target] {
        match self.selected_rule().and_then(|r| self.details.get(&r.name)) {
            Some(Ok((_, targets))) => targets,
            _ => &[],
        }
    }

    fn select(&mut self, index: usize) {
        if self.visible.is_empty() {
            return;
        }
        let index = index.min(self.visible.len() - 1);
        if self.list_state.selected() != Some(index) {
            self.detail_scroll = 0;
            self.target = 0;
        }
        self.list_state.select(Some(index));
        self.queue_detail();
    }

    /// Loads the selected rule's detail and targets after a short pause (arrow-key scrolling).
    fn queue_detail(&mut self) {
        if let Some(r) = self.selected_rule()
            && !self.details.contains_key(&r.name)
        {
            self.pending = Some((r.name.clone(), Instant::now()));
        }
    }

    fn fetch_detail(&mut self, name: String, ctx: &mut Ctx) {
        let Some(bus) = self.bus.clone() else { return };
        if !self.in_flight.insert(name.clone()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = events::describe_rule(&aws, &bus, &name).and_then(|rule| Ok((rule, events::list_targets(&aws, &bus, &name)?)));
            Msg::Detail { bus, name, result: Box::new(result) }
        });
    }

    fn open_target(&mut self, ctx: &mut Ctx) {
        let Some(t) = self.targets().get(self.target).cloned() else { return };
        match t.service() {
            "lambda" => ctx.jump("Lambda", t.name()),
            "states" => ctx.jump("Step Functions", t.arn.clone()),
            "logs" => ctx.jump("Logs", t.name()),
            _ => ctx.toast(Level::Info, format!("dcli has no module for {} targets; o opens the rule in the console", t.kind())),
        }
    }

    /// The test event form, prefilled from the selected rule's pattern.
    fn open_send(&mut self) {
        let bus = match (&self.bus, self.selected_bus()) {
            (Some(b), _) => b.clone(),
            (None, Some(b)) => b.name.clone(),
            (None, None) => return,
        };
        let rule = self.selected_rule().and_then(|r| {
            let pattern = r.event_pattern.clone().filter(|p| !p.trim().is_empty())?;
            let account = events::account_of(&r.arn).unwrap_or("123456789012").to_string();
            Some((r.name.clone(), pattern, account))
        });
        let (source, detail_type, detail) = match &rule {
            Some((_, pattern, _)) => events::sample_from_pattern(pattern),
            None => (String::new(), String::new(), "{}".into()),
        };
        let mut form = Form::new(
            format!("Test event · {bus}"),
            vec![
                Field::fixed("Bus", bus.clone()),
                Field::text("Source", source).hint("e.g. orders.service"),
                Field::text("Detail type", detail_type).hint("e.g. OrderPaid"),
                Field::text("Detail", detail).hint("a JSON object; paste works"),
            ],
        );
        form.note = Some(match &rule {
            Some((name, _, _)) => format!("Prefilled from rule {name}. Enter checks it against the rule's pattern first."),
            None => "No rule with a pattern selected: Enter sends it as is.".into(),
        });
        if is_production(&self.env_name) {
            form = form.confirm_with(&self.env_name);
        }
        self.sending = Some(Sending { form, bus, rule, checked: None, busy: false });
    }

    fn submit(&mut self, ctx: &mut Ctx) {
        let Some(sending) = &mut self.sending else { return };
        let form = &mut sending.form;
        let (source, detail_type) = (form.get("Source").trim().to_string(), form.get("Detail type").trim().to_string());
        let detail_text = form.get("Detail").trim().to_string();
        if source.is_empty() || detail_type.is_empty() {
            return form.fail("Source and detail type are required");
        }
        let detail = match serde_json::from_str::<Value>(&detail_text) {
            Ok(v @ Value::Object(_)) => v,
            _ => return form.fail("Detail must be a JSON object, like {\"orderId\": \"123\"}"),
        };
        let signature = format!("{source}\u{1f}{detail_type}\u{1f}{detail}");
        let aws = ctx.env().aws.clone();
        // First check the values against the rule's pattern; the next Enter sends them.
        if let Some((_, pattern, account)) = &sending.rule
            && sending.checked.as_ref().is_none_or(|(s, _)| *s != signature)
        {
            sending.busy = true;
            let (pattern, event) = (pattern.clone(), events::envelope(&source, &detail_type, detail, account, &self.region));
            ctx.spawn(move || Msg::Checked { signature, result: events::test_event_pattern(&aws, &pattern, &event) });
            return;
        }
        sending.busy = true;
        let bus = sending.bus.clone();
        let detail = detail.to_string();
        ctx.spawn(move || {
            let result = events::put_event(&aws, &bus, &source, &detail_type, &detail);
            Msg::Sent { bus, result }
        });
    }

    fn on_checked(&mut self, signature: String, result: aws::Result<bool>) {
        let Some(sending) = &mut self.sending else { return };
        sending.busy = false;
        let rule = sending.rule.as_ref().map(|(n, _, _)| n.clone()).unwrap_or_default();
        match result {
            Ok(true) => {
                sending.form.note = Some(format!("✓ Matches rule {rule}: it will receive this event. Enter sends it."));
                sending.checked = Some((signature, true));
            }
            Ok(false) => {
                sending.form.note = None;
                sending
                    .form
                    .fail(format!("Does not match rule {rule}: it would not receive this event. Enter sends it anyway, or change the fields."));
                sending.checked = Some((signature, false));
            }
            Err(e) => {
                sending.form.note = Some(format!("Could not check against the pattern ({}). Enter sends it anyway.", e.short()));
                sending.checked = Some((signature, false));
            }
        }
    }

    fn on_sent(&mut self, bus: String, result: aws::Result<PutResult>, ctx: &mut Ctx) {
        let Some(sending) = &mut self.sending else { return };
        sending.busy = false;
        match result {
            Ok(PutResult { event_id: Some(id), .. }) => {
                self.sending = None;
                ctx.toast(Level::Success, format!("Sent event {id} to {bus}"));
            }
            Ok(r) => sending.form.fail(format!(
                "{}: {}",
                r.error_code.unwrap_or_else(|| "Failed".into()),
                r.error_message.unwrap_or_default()
            )),
            Err(e) => sending.form.fail(e.short()),
        }
    }

    fn copy_pattern(&self, ctx: &mut Ctx) {
        let Some(r) = self.selected_rule() else { return };
        let text = match (&r.event_pattern, &r.schedule_expression) {
            (Some(p), _) => events::pretty_pattern(p).unwrap_or_else(|| p.clone()),
            (None, Some(s)) => s.clone(),
            _ => return,
        };
        let ok = crate::platform::copy_to_clipboard(&text);
        ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Pattern copied" } else { "Clipboard not available" });
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let what = if self.bus.is_some() { "rules" } else { "buses" };
        let placeholder = format!("type / to search {what}");
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, &placeholder);
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let (load, title) = match &self.bus {
            None => (&self.buses_load, format!(" Event buses · {} ", self.visible.len())),
            Some(b) => (&self.rules_load, format!(" {b} · {} rules ", self.visible.len())),
        };
        let title = if matches!(load, LoadState::Loading) { format!(" {} {}", theme::spinner(view.tick), title.trim_start()) } else { title };
        let focused = self.focus == Focus::List;
        if self.visible.is_empty() {
            let text = match load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled(format!("Loading {what}…"), theme::muted()),
                _ if self.bus.is_some() => Line::styled("No rules on this bus · ← back to the buses", theme::muted()),
                _ => Line::styled("No event buses", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, focused)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = match &self.bus {
            None => self
                .visible
                .iter()
                .map(|&i| {
                    let b = &self.buses[i];
                    let kind = if b.name == "default" { "default" } else if b.name.starts_with("aws.partner/") { "partner" } else { "custom" };
                    let name = ellipsize(&b.name, width.saturating_sub(kind.len() + 2));
                    let pad = width.saturating_sub(name.chars().count() + kind.len());
                    ListItem::new(Line::from(vec![Span::raw(name), Span::raw(" ".repeat(pad)), Span::styled(kind, theme::muted())]))
                })
                .collect(),
            Some(_) => self
                .visible
                .iter()
                .map(|&i| {
                    let r = &self.rules[i];
                    let (symbol, style) = if r.enabled() { ("●", Style::new().fg(theme::OK)) } else { ("○", theme::muted()) };
                    let kind = if r.schedule_expression.as_deref().is_some_and(|s| !s.is_empty()) { "schedule" } else { "pattern" };
                    let name = ellipsize(&r.name, width.saturating_sub(kind.len() + 4));
                    let pad = width.saturating_sub(name.chars().count() + kind.len() + 2);
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{symbol} "), style),
                        Span::raw(name),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(kind, theme::muted()),
                    ]))
                })
                .collect(),
        };
        let list = List::new(items).block(widgets::panel(title, focused)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let kw = 12;
        if self.bus.is_none() {
            let Some(b) = self.selected_bus() else {
                f.render_widget(
                    widgets::placeholder(Line::styled("Select an event bus", theme::muted())).block(widgets::panel(" Event bus ", false)),
                    area,
                );
                return;
            };
            let mut lines = vec![widgets::kv("ARN", b.arn.clone(), kw)];
            if let Some(d) = b.description.as_ref().filter(|d| !d.is_empty()) {
                lines.push(widgets::kv("Description", d.clone(), kw));
            }
            lines.push(Line::raw(""));
            lines.push(widgets::hints_line(&[("Enter", "its rules"), ("t", "send a test event to it")]));
            let block = widgets::panel(Line::from(format!(" {} ", b.name)).bold(), false);
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
            return;
        }
        let Some(listed) = self.selected_rule() else {
            f.render_widget(
                widgets::placeholder(Line::styled("Select a rule", theme::muted())).block(widgets::panel(" Rule ", false)),
                area,
            );
            return;
        };
        let detail = self.details.get(&listed.name);
        let rule = match detail {
            Some(Ok((r, _))) => r,
            _ => listed,
        };
        let state_style = if rule.enabled() { Style::new().fg(theme::OK) } else { theme::muted() };
        let mut lines = vec![
            widgets::kv("State", Span::styled(rule.state.clone(), state_style), kw),
            widgets::kv("ARN", rule.arn.clone(), kw),
        ];
        if let Some(d) = rule.description.as_ref().filter(|d| !d.is_empty()) {
            lines.push(widgets::kv("Description", d.clone(), kw));
        }
        if let Some(s) = rule.schedule_expression.as_ref().filter(|s| !s.is_empty()) {
            lines.push(widgets::kv("Schedule", s.clone(), kw));
        }
        if let Some(m) = rule.managed_by.as_ref().filter(|m| !m.is_empty()) {
            lines.push(widgets::kv("Managed by", m.clone(), kw));
        }
        if let Some(c) = rule.created_by.as_ref().filter(|c| !c.is_empty()) {
            lines.push(widgets::kv("Created by", c.clone(), kw));
        }
        if let Some(r) = rule.role_arn.as_ref().filter(|r| !r.is_empty()) {
            lines.push(widgets::kv("Role", r.clone(), kw));
        }
        if let Some(p) = rule.event_pattern.as_ref().filter(|p| !p.is_empty()) {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![widgets::section("Event pattern").spans.remove(0), Span::styled("   y copy · t test event", theme::muted())]));
            let text = events::pretty_pattern(p).unwrap_or_else(|| p.clone());
            lines.extend(text.lines().map(|l| Line::styled(l.to_string(), theme::accent())));
        }
        lines.push(Line::raw(""));
        match detail {
            None => lines.push(Line::from(vec![
                Span::styled(theme::spinner(view.tick), theme::accent()),
                Span::raw(" loading targets…"),
            ])),
            Some(Err(e)) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            Some(Ok((_, targets))) => {
                let hint = if self.focus == Focus::Targets { "   ↑↓ target · Enter open · ← back" } else { "   → selects a target" };
                lines.push(Line::from(vec![
                    widgets::section(&format!("Targets ({})", targets.len())).spans.remove(0),
                    Span::styled(if targets.is_empty() { "" } else { hint }, theme::muted()),
                ]));
                if targets.is_empty() {
                    lines.push(Line::styled("No targets: matching events go nowhere.", theme::muted()));
                }
                for (i, t) in targets.iter().enumerate() {
                    let selected = self.focus == Focus::Targets && i == self.target;
                    let marker = if selected { "▶ " } else { "  " };
                    let name_style = if selected { theme::selected() } else { theme::bold() };
                    lines.push(Line::from(vec![
                        Span::styled(marker, theme::accent()),
                        Span::styled(format!("{:<15}", t.kind()), Style::new().fg(theme::APPROVAL)),
                        Span::styled(t.name(), name_style),
                    ]));
                    let mut extra = vec![format!("id {}", t.id), format!("input: {}", t.input_summary())];
                    if let Some(dlq) = t.dead_letter_config.as_ref().and_then(|d| d.arn.clone()) {
                        extra.push(format!("DLQ {}", dlq.rsplit(':').next().unwrap_or(&dlq)));
                    }
                    if let Some(r) = &t.retry_policy
                        && let Some(n) = r.maximum_retry_attempts
                    {
                        extra.push(format!("retries {n}"));
                    }
                    lines.push(Line::styled(format!("                 {}", extra.join(" · ")), theme::muted()));
                }
            }
        }
        let block = widgets::panel(Line::from(format!(" {} ", rule.name)).bold(), self.focus == Focus::Targets);
        f.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block),
            area,
        );
    }
}

impl Module for EventBridge {
    fn title(&self) -> &'static str {
        "EventBridge"
    }

    fn description(&self) -> &'static str {
        "EventBridge · buses, rules, targets, send test events"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = EventBridge::new();
        self.env_name = ctx.env().name.clone();
        self.region = ctx.env().region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.buses_load, LoadState::Idle) {
            self.load_buses(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some(sending) = &mut self.sending {
            if sending.busy {
                return Handled::Yes;
            }
            match sending.form.handle(key) {
                FormEvent::Submit => self.submit(ctx),
                FormEvent::Cancel => self.sending = None,
                FormEvent::Pending => {}
            }
            return Handled::Yes;
        }
        let selected = self.list_state.selected().unwrap_or(0);
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
        if self.focus == Focus::Targets {
            let count = self.targets().len();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.target = self.target.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.target = (self.target + 1).min(count.saturating_sub(1)),
                KeyCode::Enter | KeyCode::Right => self.open_target(ctx),
                KeyCode::Esc | KeyCode::Left => self.focus = Focus::List,
                KeyCode::Char('t') => self.open_send(),
                KeyCode::Char('y') => self.copy_pattern(ctx),
                _ => return Handled::No,
            }
            return Handled::Yes;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::Enter | KeyCode::Right => match &self.bus {
                None => {
                    if let Some(b) = self.selected_bus() {
                        let name = b.name.clone();
                        self.open_bus(name, ctx);
                    }
                }
                Some(_) if !self.targets().is_empty() => {
                    self.focus = Focus::Targets;
                    self.target = 0;
                }
                Some(_) => {}
            },
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => self.back(),
            KeyCode::Char('t') => self.open_send(),
            KeyCode::Char('y') => self.copy_pattern(ctx),
            KeyCode::Char('o') => {
                let (bus, rule) = match (&self.bus, self.selected_rule(), self.selected_bus()) {
                    (Some(b), Some(r), _) => (b.clone(), r.name.clone()),
                    _ => return Handled::Yes,
                };
                let url = events::console_url(&self.region, &bus, &rule);
                if !crate::platform::open_url(&url) {
                    ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                }
            }
            KeyCode::Char('r') => match self.bus.clone() {
                Some(bus) => self.open_bus(bus, ctx),
                None => self.load_buses(ctx),
            },
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if self.sending.is_some() || !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = event.row.saturating_sub(self.list_area.y + 2) as usize;
                self.focus = Focus::List;
                self.select(self.list_state.offset() + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Buses(result) => {
                match result {
                    Ok(mut buses) => {
                        // The default bus first, then by name.
                        buses.sort_by(|a, b| (a.name != "default", &a.name).cmp(&(b.name != "default", &b.name)));
                        self.buses = buses;
                        self.buses_load = LoadState::Loaded;
                    }
                    Err(e) => self.buses_load = LoadState::Failed(e),
                }
                if self.bus.is_none() {
                    self.refilter();
                }
            }
            Msg::Rules { bus, result } => {
                if self.bus.as_deref() != Some(bus.as_str()) {
                    return;
                }
                match result {
                    Ok(mut rules) => {
                        rules.sort_by(|a, b| a.name.cmp(&b.name));
                        self.rules = rules;
                        self.rules_load = LoadState::Loaded;
                    }
                    Err(e) => self.rules_load = LoadState::Failed(e),
                }
                self.refilter();
            }
            Msg::Detail { bus, name, result } => {
                self.in_flight.remove(&name);
                if self.bus.as_deref() == Some(bus.as_str()) {
                    self.details.insert(name, *result);
                }
            }
            Msg::Checked { signature, result } => self.on_checked(signature, result),
            Msg::Sent { bus, result } => self.on_sent(bus, result, ctx),
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, _active: bool) {
        if let Some((name, at)) = &self.pending
            && at.elapsed() >= DEBOUNCE
        {
            let name = name.clone();
            self.pending = None;
            if self.selected_rule().is_some_and(|r| r.name == name) && !self.details.contains_key(&name) {
                self.fetch_detail(name, ctx);
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [left, right] = if area.width < 90 {
            area.layout(&Layout::vertical([Constraint::Percentage(40), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(38), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        self.render_detail(f, right, view);
        if let Some(sending) = &self.sending {
            let color = if is_production(&self.env_name) { theme::ERR } else { theme::ACCENT };
            sending.form.render(f, area, color);
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.sending.is_some() {
            return vec![("Tab", "next field"), ("Enter", "check / send"), ("Esc", "cancel")];
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        if self.focus == Focus::Targets {
            return vec![("↑↓", "target"), ("Enter", "open"), ("t", "test event"), ("←", "back to rules")];
        }
        match self.bus {
            None => vec![("↑↓", "bus"), ("Enter", "rules"), ("/", "search"), ("t", "test event"), ("r", "reload")],
            Some(_) => vec![
                ("↑↓", "rule"),
                ("→", "targets"),
                ("t", "test event"),
                ("y", "copy pattern"),
                ("/", "search"),
                ("o", "console"),
                ("←", "buses"),
                ("r", "reload"),
            ],
        }
    }

    fn animating(&self) -> bool {
        matches!(self.buses_load, LoadState::Loading)
            || matches!(self.rules_load, LoadState::Loading)
            || !self.in_flight.is_empty()
            || self.pending.is_some()
            || self.sending.as_ref().is_some_and(|s| s.busy)
    }

    fn capturing(&self) -> bool {
        self.filtering || self.sending.is_some()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if let Some(sending) = &mut self.sending {
            sending.form.paste(text);
        } else if self.filtering {
            self.filter.insert_str(text, false);
            self.refilter();
        }
    }
}

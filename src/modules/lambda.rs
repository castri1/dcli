//! Lambda explorer: list functions, search, and inspect configuration,
//! environment variables (masked until revealed), layers, VPC, tags.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::{self, AwsError, lambda};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, fmt_bytes, parse_rfc3339};

const DEBOUNCE: Duration = Duration::from_millis(200);

enum Msg {
    Page(Vec<lambda::FunctionConfiguration>),
    Done,
    Failed(AwsError),
    Detail { name: String, result: Box<aws::Result<lambda::FunctionDetail>> },
}

pub struct Lambda {
    functions: Vec<lambda::FunctionConfiguration>,
    load: LoadState,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    list_state: ListState,
    details: HashMap<String, aws::Result<lambda::FunctionDetail>>,
    in_flight: HashSet<String>,
    pending: Option<(String, Instant)>,
    reveal_env: bool,
    detail_scroll: u16,
    region: String,
    list_area: Rect,
}

impl Lambda {
    pub fn new() -> Lambda {
        Lambda {
            functions: Vec::new(),
            load: LoadState::Idle,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            details: HashMap::new(),
            in_flight: HashSet::new(),
            pending: None,
            reveal_env: false,
            detail_scroll: 0,
            region: String::new(),
            list_area: Rect::default(),
        }
    }

    fn load(&mut self, ctx: &mut Ctx) {
        let region = ctx.env().region.clone();
        *self = Lambda::new();
        self.region = region;
        self.load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let mut marker: Option<String> = None;
            loop {
                match lambda::list_functions(&aws, marker.as_deref()) {
                    Ok((page, next)) => {
                        if !em.emit(Msg::Page(page)) {
                            return;
                        }
                        match next {
                            Some(m) => marker = Some(m),
                            None => break,
                        }
                    }
                    Err(e) => {
                        em.emit(Msg::Failed(e));
                        return;
                    }
                }
            }
            em.emit(Msg::Done);
        });
    }

    fn selected(&self) -> Option<&lambda::FunctionConfiguration> {
        self.functions.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn rebuild(&mut self) {
        let keep = self.selected().map(|f| f.function_name.clone());
        let query = self.filter.value.trim().to_lowercase();
        self.visible = (0..self.functions.len())
            .filter(|&i| {
                let f = &self.functions[i];
                query.is_empty()
                    || matches_filter(&f.function_name, &query)
                    || f.runtime.as_deref().is_some_and(|r| matches_filter(r, &query))
            })
            .collect();
        self.visible.sort_by(|&a, &b| self.functions[a].function_name.cmp(&self.functions[b].function_name));
        let index = keep
            .and_then(|n| self.visible.iter().position(|&i| self.functions[i].function_name == n))
            .unwrap_or(0);
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
        if let Some(f) = self.selected()
            && !self.details.contains_key(&f.function_name)
        {
            self.pending = Some((f.function_name.clone(), Instant::now()));
        }
    }

    fn fetch(&mut self, name: String, ctx: &mut Ctx) {
        if !self.in_flight.insert(name.clone()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = Box::new(lambda::get_function(&aws, &name));
            Msg::Detail { name, result }
        });
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|&i| {
                let func = &self.functions[i];
                let runtime = func.runtime.clone().unwrap_or_else(|| func.package_type.clone().unwrap_or_default());
                let right = format!("{:>9}  {:>6}", ellipsize(&runtime, 12), func.memory_size.map(|m| format!("{m}MB")).unwrap_or_default());
                let name = ellipsize(&func.function_name, width.saturating_sub(right.chars().count() + 2));
                let pad = width.saturating_sub(name.chars().count() + right.chars().count());
                ListItem::new(Line::from(vec![
                    Span::raw(name),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(right, theme::muted()),
                ]))
            })
            .collect();
        let title = match &self.load {
            LoadState::Loading => format!(" {} Loading… {} ", theme::spinner(view.tick), self.functions.len()),
            _ if !self.filter.value.is_empty() => format!(" {} of {} functions ", self.visible.len(), self.functions.len()),
            _ => format!(" {} functions ", self.functions.len()),
        };
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search functions or runtimes");
        f.render_widget(Paragraph::new(line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        if self.visible.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading functions…", theme::muted()),
                _ => Line::styled("No functions", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(func) = self.selected() else {
            f.render_widget(
                widgets::placeholder(Line::styled("Select a function", theme::muted())).block(widgets::panel(" Function ", false)),
                area,
            );
            return;
        };
        let detail = self.details.get(&func.function_name);
        // GetFunction has the full configuration; fall back to the list entry meanwhile.
        let (c, tags, concurrency) = match detail {
            Some(Ok(d)) => (&d.configuration, Some(&d.tags), d.concurrency.as_ref()),
            _ => (func, None, None),
        };
        let kw = 14;
        let mut lines = vec![widgets::section("General")];
        if let Some(d) = c.description.as_ref().filter(|d| !d.is_empty()) {
            lines.push(widgets::kv("Description", d.clone(), kw));
        }
        lines.push(widgets::kv("ARN", Span::styled(c.function_arn.clone(), theme::muted()), kw));
        lines.push(widgets::kv("Runtime", c.runtime.clone().unwrap_or_else(|| "—".into()), kw));
        if let Some(h) = &c.handler {
            lines.push(widgets::kv("Handler", h.clone(), kw));
        }
        lines.push(widgets::kv("Architecture", c.architectures.join(", "), kw));
        lines.push(widgets::kv("Package", c.package_type.clone().unwrap_or_default(), kw));
        lines.push(widgets::kv(
            "Memory",
            Span::styled(c.memory_size.map(|m| format!("{m} MB")).unwrap_or_default(), theme::bold()),
            kw,
        ));
        lines.push(widgets::kv("Timeout", c.timeout.map(|t| format!("{t} s")).unwrap_or_default(), kw));
        if let Some(e) = &c.ephemeral_storage {
            lines.push(widgets::kv("Ephemeral /tmp", format!("{} MB", e.size), kw));
        }
        lines.push(widgets::kv("Code size", fmt_bytes(c.code_size), kw));
        if let Some(state) = &c.state {
            let color = match state.as_str() {
                "Active" => theme::OK,
                "Failed" => theme::ERR,
                _ => theme::WARN,
            };
            let mut value = state.clone();
            if let Some(r) = &c.state_reason {
                value.push_str(&format!(" — {r}"));
            }
            lines.push(widgets::kv("State", Span::styled(value, Style::new().fg(color)), kw));
        }
        if let Some(u) = &c.last_update_status {
            lines.push(widgets::kv("Last update", u.clone(), kw));
        }
        if let Some(m) = &c.last_modified {
            let ago = parse_rfc3339(m).map(fmt_ago).unwrap_or_default();
            lines.push(widgets::kv("Modified", format!("{ago} · {m}"), kw));
        }
        if let Some(conc) = concurrency {
            let value = conc
                .reserved_concurrent_executions
                .map(|n| format!("{n} reserved"))
                .unwrap_or_else(|| "unreserved".into());
            lines.push(widgets::kv("Concurrency", value, kw));
        }

        lines.push(Line::raw(""));
        lines.push(widgets::section("Execution"));
        if let Some(role) = &c.role {
            lines.push(widgets::kv("Role", role.rsplit('/').next().unwrap_or(role).to_string(), kw));
        }
        let log_group = c
            .logging_config
            .as_ref()
            .and_then(|l| l.log_group.clone())
            .unwrap_or_else(|| format!("/aws/lambda/{}", c.function_name));
        lines.push(widgets::kv("Log group", log_group, kw));
        if let Some(fmt) = c.logging_config.as_ref().and_then(|l| l.log_format.clone()) {
            lines.push(widgets::kv("Log format", fmt, kw));
        }
        if let Some(mode) = c.tracing_config.as_ref().and_then(|t| t.mode.clone()) {
            lines.push(widgets::kv("Tracing", mode, kw));
        }
        if let Some(vpc) = c.vpc_config.as_ref().filter(|v| v.vpc_id.as_deref().is_some_and(|id| !id.is_empty())) {
            lines.push(widgets::kv(
                "VPC",
                format!(
                    "{} · {} subnets · {} security groups",
                    vpc.vpc_id.clone().unwrap_or_default(),
                    vpc.subnet_ids.len(),
                    vpc.security_group_ids.len()
                ),
                kw,
            ));
        }
        for layer in &c.layers {
            let name = layer.arn.rsplit(":layer:").next().unwrap_or(&layer.arn);
            lines.push(widgets::kv("Layer", format!("{name} ({})", fmt_bytes(layer.code_size)), kw));
        }

        let vars = c.environment.as_ref().map(|e| &e.variables);
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            widgets::section(&format!("Environment ({})", vars.map_or(0, |v| v.len()))).spans.remove(0),
            Span::styled(if self.reveal_env { "   v hide" } else { "   v reveal" }, theme::muted()),
        ]));
        match vars {
            Some(vars) if !vars.is_empty() => {
                let key_w = vars.keys().map(|k| k.len()).max().unwrap_or(0).min(32);
                for (k, v) in vars {
                    let value = if self.reveal_env {
                        Span::raw(v.clone())
                    } else {
                        Span::styled("••••••••", theme::muted())
                    };
                    lines.push(Line::from(vec![Span::styled(format!("{k:<key_w$}  "), theme::accent()), value]));
                }
            }
            _ => lines.push(Line::styled("none", theme::muted())),
        }

        lines.push(Line::raw(""));
        match (detail, tags) {
            (None, _) => lines.push(Line::from(vec![
                Span::styled(theme::spinner(view.tick), theme::accent()),
                Span::raw(" loading details…"),
            ])),
            (Some(Err(e)), _) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            (Some(Ok(_)), Some(tags)) => {
                lines.push(widgets::section(&format!("Tags ({})", tags.len())));
                let key_w = tags.keys().map(|k| k.len()).max().unwrap_or(0).min(32);
                for (k, v) in tags {
                    lines.push(Line::from(vec![Span::styled(format!("{k:<key_w$}  "), theme::accent()), Span::raw(v.clone())]));
                }
            }
            _ => {}
        }

        let block = widgets::panel(Line::from(format!(" {} ", c.function_name)).bold(), false);
        f.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block),
            area,
        );
    }
}

impl Module for Lambda {
    fn title(&self) -> &'static str {
        "Lambda"
    }

    fn description(&self) -> &'static str {
        "Lambda functions · configuration, environment, tags"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Lambda::new();
        self.region = ctx.env().region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
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
            KeyCode::Char('v') => self.reveal_env = !self.reveal_env,
            KeyCode::Char('c') => {
                if let Some(func) = self.selected() {
                    let ok = crate::platform::copy_to_clipboard(&func.function_arn);
                    ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "ARN copied" } else { "Clipboard not available" });
                }
            }
            KeyCode::Char('o') => {
                if let Some(func) = self.selected() {
                    let url = aws::console_url(
                        &self.region,
                        &format!("lambda/home?region={}#/functions/{}", self.region, func.function_name),
                    );
                    if !crate::platform::open_url(&url) {
                        ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                    }
                }
            }
            KeyCode::Char('r') => {
                if let Some(func) = self.selected() {
                    let name = func.function_name.clone();
                    self.details.remove(&name);
                    self.fetch(name, ctx);
                } else {
                    self.load(ctx);
                }
            }
            KeyCode::Char('R') => self.load(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        let pos = (event.column, event.row).into();
        if !self.list_area.contains(pos) {
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

    fn task(&mut self, payload: Payload, _ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Page(page) => {
                let first = self.functions.is_empty();
                self.functions.extend(page);
                self.rebuild();
                if first {
                    self.select(0);
                }
            }
            Msg::Done => self.load = LoadState::Loaded,
            Msg::Failed(e) => self.load = LoadState::Failed(e),
            Msg::Detail { name, result } => {
                self.in_flight.remove(&name);
                self.details.insert(name, *result);
            }
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, _active: bool) {
        if let Some((name, at)) = &self.pending
            && at.elapsed() >= DEBOUNCE
        {
            let name = name.clone();
            self.pending = None;
            if self.selected().is_some_and(|f| f.function_name == name) && !self.details.contains_key(&name) {
                self.fetch(name, ctx);
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [left, right] = if area.width < 90 {
            area.layout(&Layout::vertical([Constraint::Percentage(45), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(42), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        self.render_detail(f, right, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        vec![
            ("↑↓", "move"),
            ("/", "search"),
            ("v", "env values"),
            ("c", "copy ARN"),
            ("o", "console"),
            ("r", "refresh"),
            ("PgUp/PgDn", "scroll"),
        ]
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading) || self.pending.is_some() || !self.in_flight.is_empty()
    }

    fn capturing(&self) -> bool {
        self.filtering
    }
}

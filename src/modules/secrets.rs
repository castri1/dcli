//! Secrets Manager explorer: list and search secrets, see their metadata, reveal and copy
//! values on demand, create secrets and store new values.

use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::{self, AwsError, secrets};
use crate::config::is_production;
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, fmt_utc, pretty_json};

enum Msg {
    Page(Vec<secrets::Secret>),
    Done,
    Failed(AwsError),
    Value { name: String, result: aws::Result<secrets::SecretValue> },
    Saved { name: String, created: bool, result: aws::Result<()> },
}

/// The form being filled: a new secret, or a new value for an existing one.
struct Editing {
    form: Form,
    existing: Option<String>,
    busy: bool,
}

pub struct Secrets {
    all: Vec<secrets::Secret>,
    load: LoadState,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    list_state: ListState,
    values: HashMap<String, aws::Result<secrets::SecretValue>>,
    revealed: HashSet<String>,
    in_flight: HashSet<String>,
    copy_after_fetch: Option<String>,
    editing: Option<Editing>,
    /// Secret to select once the list reloads (after creating or updating it).
    reselect: Option<String>,
    env_name: String,
    region: String,
    detail_scroll: u16,
    list_area: Rect,
}

impl Secrets {
    pub fn new() -> Secrets {
        Secrets {
            all: Vec::new(),
            load: LoadState::Idle,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            values: HashMap::new(),
            revealed: HashSet::new(),
            in_flight: HashSet::new(),
            copy_after_fetch: None,
            editing: None,
            reselect: None,
            env_name: String::new(),
            region: String::new(),
            detail_scroll: 0,
            list_area: Rect::default(),
        }
    }

    fn load(&mut self, ctx: &mut Ctx) {
        self.all.clear();
        self.visible.clear();
        self.load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let mut token: Option<String> = None;
            loop {
                match secrets::list_secrets(&aws, token.as_deref()) {
                    Ok((page, next)) => {
                        if !em.emit(Msg::Page(page)) {
                            return;
                        }
                        match next {
                            Some(t) => token = Some(t),
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

    fn refilter(&mut self) {
        let keep = self.selected().map(|s| s.name.clone());
        let query = self.filter.value.trim().to_lowercase();
        let mut visible: Vec<usize> = (0..self.all.len())
            .filter(|&i| {
                let s = &self.all[i];
                matches_filter(&format!("{} {}", s.name, s.description.as_deref().unwrap_or("")), &query)
            })
            .collect();
        visible.sort_by(|&a, &b| self.all[a].name.cmp(&self.all[b].name));
        self.visible = visible;
        let index = keep.and_then(|k| self.visible.iter().position(|&i| self.all[i].name == k)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    fn selected(&self) -> Option<&secrets::Secret> {
        self.all.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn select(&mut self, index: usize) {
        if !self.visible.is_empty() {
            let index = index.min(self.visible.len() - 1);
            if self.list_state.selected() != Some(index) {
                self.detail_scroll = 0;
            }
            self.list_state.select(Some(index));
        }
    }

    fn fetch(&mut self, name: String, ctx: &mut Ctx) {
        if !self.in_flight.insert(name.clone()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = secrets::get_secret_value(&aws, &name);
            Msg::Value { name, result }
        });
    }

    fn copy_value(&mut self, ctx: &mut Ctx) {
        let Some(name) = self.selected().map(|s| s.name.clone()) else { return };
        match self.values.get(&name) {
            Some(Ok(v)) => copy(ctx, v.secret_string.as_deref().unwrap_or(""), "Secret copied"),
            Some(Err(e)) => ctx.toast(Level::Error, e.short()),
            None => {
                // Read it just for the copy, without showing it.
                self.copy_after_fetch = Some(name.clone());
                self.fetch(name, ctx);
            }
        }
    }

    fn open_form(&mut self, update: bool) {
        let editing = if update {
            let Some(s) = self.selected() else { return };
            let current = match self.values.get(&s.name) {
                Some(Ok(v)) => v.secret_string.clone().unwrap_or_default(),
                _ => String::new(),
            };
            let mut form = Form::new(
                format!("New value · {}", s.name),
                vec![Field::fixed("Secret", s.name.clone()), Field::secret("Value", current)],
            );
            form.note = Some("Stored as a new version (AWSCURRENT); the previous one stays as AWSPREVIOUS.".into());
            Editing { form, existing: Some(s.name.clone()), busy: false }
        } else {
            let form = Form::new(
                format!("New secret · {}", self.env_name),
                vec![
                    Field::text("Name", "").hint("e.g. prod/payments/stripe"),
                    Field::secret("Value", "").hint("text or JSON; paste works"),
                    Field::text("Description", "").optional("optional"),
                ],
            );
            Editing { form, existing: None, busy: false }
        };
        let form = if is_production(&self.env_name) { editing.form.confirm_with(&self.env_name) } else { editing.form };
        self.editing = Some(Editing { form, ..editing });
    }

    fn submit(&mut self, ctx: &mut Ctx) {
        let Some(editing) = &mut self.editing else { return };
        let form = &mut editing.form;
        let (name, value, description) =
            (form.get("Name").trim().to_string(), form.get("Value").to_string(), form.get("Description").trim().to_string());
        let aws = ctx.env().aws.clone();
        match editing.existing.clone() {
            Some(existing) => {
                if value.is_empty() {
                    return form.fail("Value is required");
                }
                editing.busy = true;
                ctx.spawn(move || {
                    let result = secrets::put_secret_value(&aws, &existing, &value).map(|_| ());
                    Msg::Saved { name: existing, created: false, result }
                });
            }
            None => {
                if name.is_empty() || value.is_empty() {
                    return form.fail("Name and value are required");
                }
                editing.busy = true;
                ctx.spawn(move || {
                    let description = Some(description.as_str()).filter(|d| !d.is_empty());
                    let result = secrets::create_secret(&aws, &name, &value, description).map(|_| ());
                    Msg::Saved { name, created: true, result }
                });
            }
        }
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search secrets");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = match &self.load {
            LoadState::Loading => format!(" {} Secrets · {} ", theme::spinner(view.tick), self.all.len()),
            _ => format!(" Secrets · {} ", self.visible.len()),
        };
        if self.visible.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading secrets…", theme::muted()),
                _ => Line::styled("No secrets · n creates one", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|&i| {
                let s = &self.all[i];
                let when = s.last_changed_date.map(fmt_ago).unwrap_or_default();
                let name = ellipsize(&s.name, width.saturating_sub(when.len() + 2));
                let pad = width.saturating_sub(name.chars().count() + when.len());
                ListItem::new(Line::from(vec![Span::raw(name), Span::raw(" ".repeat(pad)), Span::styled(when, theme::muted())]))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(s) = self.selected() else {
            f.render_widget(
                widgets::placeholder(Line::styled("Select a secret · n creates one", theme::muted()))
                    .block(widgets::panel(" Secret ", false)),
                area,
            );
            return;
        };
        let kw = 13;
        let date = |t: Option<f64>| t.map(|t| format!("{} · {}", fmt_ago(t), fmt_utc(t))).unwrap_or_else(|| "—".into());
        let mut lines = vec![widgets::kv("ARN", s.arn.clone(), kw)];
        if let Some(d) = s.description.as_ref().filter(|d| !d.is_empty()) {
            lines.push(widgets::kv("Description", d.clone(), kw));
        }
        lines.push(widgets::kv("KMS key", s.kms_key_id.clone().unwrap_or_else(|| "aws/secretsmanager".into()), kw));
        lines.push(widgets::kv(
            "Rotation",
            if s.rotation_enabled == Some(true) { format!("on · last {}", date(s.last_rotated_date)) } else { "off".into() },
            kw,
        ));
        lines.push(widgets::kv("Changed", date(s.last_changed_date), kw));
        lines.push(widgets::kv("Accessed", date(s.last_accessed_date), kw));
        lines.push(widgets::kv("Created", date(s.created_date), kw));
        if let Some(o) = &s.owning_service {
            lines.push(widgets::kv("Managed by", o.clone(), kw));
        }
        if !s.tags.is_empty() {
            lines.push(Line::raw(""));
            lines.push(widgets::section("Tags"));
            let tw = s.tags.iter().map(|t| t.key.chars().count()).max().unwrap_or(0).min(30);
            for t in &s.tags {
                lines.push(widgets::kv(&t.key, t.value.clone(), tw));
            }
        }
        lines.push(Line::raw(""));
        let revealed = self.revealed.contains(&s.name);
        lines.push(Line::from(vec![
            widgets::section("Value").spans.remove(0),
            Span::styled(if revealed { "   v hide · c copy · u new value" } else { "   v reveal · c copy · u new value" }, theme::muted()),
        ]));
        match (revealed, self.values.get(&s.name)) {
            (false, _) => lines.push(Line::styled("•••••••••••• (hidden)", theme::muted())),
            (true, Some(Ok(v))) => match (&v.secret_string, &v.secret_binary) {
                (Some(text), _) => {
                    let text = pretty_json(text).unwrap_or_else(|| text.clone());
                    lines.extend(text.lines().map(|l| Line::styled(l.to_string(), Style::new().fg(theme::APPROVAL))));
                }
                (None, Some(b)) => lines.push(Line::styled(format!("(binary secret, {} base64 characters)", b.len()), theme::muted())),
                _ => lines.push(Line::styled("(empty)", theme::muted())),
            },
            (true, Some(Err(e))) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            (true, None) => lines.push(Line::from(vec![
                Span::styled(theme::spinner(view.tick), theme::accent()),
                Span::raw(" loading…"),
            ])),
        }
        let block = widgets::panel(Line::from(format!(" {} ", s.name)).bold(), false);
        f.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block),
            area,
        );
    }
}

fn copy(ctx: &mut Ctx, text: &str, done: &str) {
    let ok = crate::platform::copy_to_clipboard(text);
    ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { done } else { "Clipboard not available" });
}

impl Module for Secrets {
    fn title(&self) -> &'static str {
        "Secrets"
    }

    fn description(&self) -> &'static str {
        "Secrets Manager · search, reveal and copy values, create secrets, store new values"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Secrets::new();
        self.env_name = ctx.env().name.clone();
        self.region = ctx.env().region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load(ctx);
        }
    }

    /// Selects secret `target`, now or once it is listed.
    fn open(&mut self, target: &str, _ctx: &mut Ctx) {
        self.filter.clear();
        self.filtering = false;
        self.refilter();
        match self.visible.iter().position(|&i| self.all[i].name == target) {
            Some(pos) => self.select(pos),
            None => self.reselect = Some(target.to_string()),
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some(editing) = &mut self.editing {
            if editing.busy {
                return Handled::Yes;
            }
            match editing.form.handle(key) {
                FormEvent::Submit => self.submit(ctx),
                FormEvent::Cancel => self.editing = None,
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
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Char('v') => {
                if let Some(name) = self.selected().map(|s| s.name.clone())
                    && !self.revealed.remove(&name)
                {
                    self.revealed.insert(name.clone());
                    if !self.values.contains_key(&name) {
                        self.fetch(name, ctx);
                    }
                }
            }
            KeyCode::Char('c') => self.copy_value(ctx),
            KeyCode::Char('y') => {
                if let Some(name) = self.selected().map(|s| s.name.clone()) {
                    copy(ctx, &name, "Name copied");
                }
            }
            KeyCode::Char('n') => self.open_form(false),
            KeyCode::Char('u') => self.open_form(true),
            KeyCode::Char('o') => {
                if let Some(s) = self.selected() {
                    let url = secrets::console_url(&self.region, &s.name);
                    if !crate::platform::open_url(&url) {
                        ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                    }
                }
            }
            KeyCode::Char('r') => self.load(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if self.editing.is_some() || !self.list_area.contains((event.column, event.row).into()) {
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

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Page(page) => {
                let first = self.all.is_empty();
                self.all.extend(page);
                self.refilter();
                if let Some(pos) =
                    self.reselect.as_ref().and_then(|n| self.visible.iter().position(|&i| self.all[i].name == *n))
                {
                    self.reselect = None;
                    self.select(pos);
                } else if first {
                    self.select(0);
                }
            }
            Msg::Done => self.load = LoadState::Loaded,
            Msg::Failed(e) => self.load = LoadState::Failed(e),
            Msg::Value { name, result } => {
                self.in_flight.remove(&name);
                if self.copy_after_fetch.as_deref() == Some(name.as_str()) {
                    self.copy_after_fetch = None;
                    match &result {
                        Ok(v) => copy(ctx, v.secret_string.as_deref().unwrap_or(""), "Secret copied"),
                        Err(e) => ctx.toast(Level::Error, e.short()),
                    }
                }
                self.values.insert(name, result);
            }
            Msg::Saved { name, created, result } => match result {
                Ok(()) => {
                    self.editing = None;
                    self.values.remove(&name);
                    ctx.toast(Level::Success, if created { format!("Created {name}") } else { format!("Stored a new value for {name}") });
                    // Reload the list (new secret, new "changed" date) and keep it selected.
                    self.reselect = Some(name);
                    self.load(ctx);
                }
                Err(e) => {
                    if let Some(editing) = &mut self.editing {
                        editing.busy = false;
                        editing.form.fail(e.short());
                    }
                }
            },
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [left, right] = if area.width < 90 {
            area.layout(&Layout::vertical([Constraint::Percentage(45), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(40), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        self.render_detail(f, right, view);
        if let Some(editing) = &self.editing {
            let color = if is_production(&self.env_name) { theme::ERR } else { theme::ACCENT };
            editing.form.render(f, area, color);
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.editing.is_some() {
            return vec![("Tab", "next field"), ("Enter", "save"), ("Esc", "cancel")];
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        vec![
            ("↑↓", "move"),
            ("/", "search"),
            ("v", "reveal"),
            ("c", "copy value"),
            ("y", "copy name"),
            ("n", "new"),
            ("u", "new value"),
            ("o", "console"),
            ("r", "reload"),
        ]
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading) || !self.in_flight.is_empty() || self.editing.as_ref().is_some_and(|e| e.busy)
    }

    fn capturing(&self) -> bool {
        self.filtering || self.editing.is_some()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if let Some(editing) = &mut self.editing {
            editing.form.paste(text);
        } else if self.filtering {
            self.filter.insert_str(text, false);
            self.refilter();
        }
    }
}

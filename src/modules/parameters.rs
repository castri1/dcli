//! Parameter Store explorer: browse parameters as folders by path, search across all
//! names, view values (SecureString only on demand) and copy them.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::{self, AwsError, ssm};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, fmt_utc, pretty_json};

const DEBOUNCE: Duration = Duration::from_millis(200);

enum Entry {
    Up,
    Folder { path: String, count: usize },
    Param(usize),
}

enum Msg {
    Page(Vec<ssm::ParameterMetadata>),
    Done,
    Failed(AwsError),
    Value { name: String, decrypt: bool, result: aws::Result<ssm::Parameter> },
}

pub struct Parameters {
    all: Vec<ssm::ParameterMetadata>,
    load: LoadState,
    prefix: String,
    filter: TextInput,
    filtering: bool,
    entries: Vec<Entry>,
    list_state: ListState,
    values: HashMap<String, aws::Result<ssm::Parameter>>,
    decrypted: HashMap<String, aws::Result<ssm::Parameter>>,
    revealed: HashSet<String>,
    in_flight: HashSet<(String, bool)>,
    pending: Option<(String, Instant)>,
    copy_after_decrypt: Option<String>,
    detail_scroll: u16,
    list_area: Rect,
}

impl Parameters {
    pub fn new() -> Parameters {
        Parameters {
            all: Vec::new(),
            load: LoadState::Idle,
            prefix: String::new(),
            filter: TextInput::default(),
            filtering: false,
            entries: Vec::new(),
            list_state: ListState::default(),
            values: HashMap::new(),
            decrypted: HashMap::new(),
            revealed: HashSet::new(),
            in_flight: HashSet::new(),
            pending: None,
            copy_after_decrypt: None,
            detail_scroll: 0,
            list_area: Rect::default(),
        }
    }

    fn load(&mut self, ctx: &mut Ctx) {
        *self = Parameters::new();
        self.load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let mut token: Option<String> = None;
            loop {
                match ssm::describe_parameters(&aws, token.as_deref()) {
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

    fn selected_entry(&self) -> Option<&Entry> {
        self.entries.get(self.list_state.selected()?)
    }

    fn selected_param(&self) -> Option<&ssm::ParameterMetadata> {
        match self.selected_entry()? {
            Entry::Param(i) => self.all.get(*i),
            _ => None,
        }
    }

    /// Rebuilds the visible entries: folders + parameters under `prefix`, or a flat
    /// search across every name while a filter is set.
    fn rebuild(&mut self) {
        let keep = self.selected_param().map(|p| p.name.clone());
        self.entries.clear();
        let query = self.filter.value.trim().to_lowercase();
        if !query.is_empty() {
            let mut hits: Vec<usize> =
                (0..self.all.len()).filter(|&i| matches_filter(&self.all[i].name, &query)).collect();
            hits.sort_by(|&a, &b| self.all[a].name.cmp(&self.all[b].name));
            self.entries.extend(hits.into_iter().map(Entry::Param));
        } else {
            if !self.prefix.is_empty() {
                self.entries.push(Entry::Up);
            }
            let mut folders: Vec<(String, usize)> = Vec::new();
            let mut params: Vec<usize> = Vec::new();
            for (i, p) in self.all.iter().enumerate() {
                let Some(rest) = p.name.strip_prefix(&self.prefix) else { continue };
                if rest.is_empty() {
                    continue;
                }
                // Skip a leading '/' so "/app/x" at the root groups under "/app/".
                match rest.char_indices().skip(1).find(|(_, c)| *c == '/') {
                    Some((slash, _)) => {
                        let folder = format!("{}{}", self.prefix, &rest[..=slash]);
                        match folders.iter_mut().find(|(f, _)| *f == folder) {
                            Some((_, n)) => *n += 1,
                            None => folders.push((folder, 1)),
                        }
                    }
                    None => params.push(i),
                }
            }
            folders.sort();
            params.sort_by(|&a, &b| self.all[a].name.cmp(&self.all[b].name));
            self.entries.extend(folders.into_iter().map(|(path, count)| Entry::Folder { path, count }));
            self.entries.extend(params.into_iter().map(Entry::Param));
        }
        let index = keep
            .and_then(|name| self.entries.iter().position(|e| matches!(e, Entry::Param(i) if self.all[*i].name == name)))
            .unwrap_or(0);
        self.list_state.select(if self.entries.is_empty() { None } else { Some(index) });
    }

    fn select(&mut self, index: usize) {
        if self.entries.is_empty() {
            return;
        }
        let index = index.min(self.entries.len() - 1);
        if self.list_state.selected() != Some(index) {
            self.detail_scroll = 0;
        }
        self.list_state.select(Some(index));
        if let Some(p) = self.selected_param()
            && p.kind != "SecureString"
            && !self.values.contains_key(&p.name)
        {
            self.pending = Some((p.name.clone(), Instant::now()));
        }
    }

    fn enter(&mut self) {
        match self.selected_entry() {
            Some(Entry::Folder { path, .. }) => {
                self.prefix = path.clone();
                self.rebuild();
                self.select(0);
            }
            Some(Entry::Up) => self.up(),
            _ => {}
        }
    }

    fn up(&mut self) {
        if self.prefix.is_empty() {
            return;
        }
        let left = self.prefix.clone();
        let trimmed = &self.prefix[..self.prefix.len() - 1];
        self.prefix = match trimmed.rfind('/') {
            Some(i) => trimmed[..=i].to_string(),
            None => String::new(),
        };
        // An absolute path's root is "/": go straight to the top level instead.
        if self.prefix == "/" {
            self.prefix.clear();
        }
        self.rebuild();
        let back = self.entries.iter().position(|e| matches!(e, Entry::Folder { path, .. } if *path == left));
        self.select(back.unwrap_or(0));
    }

    fn fetch(&mut self, name: String, decrypt: bool, ctx: &mut Ctx) {
        if !self.in_flight.insert((name.clone(), decrypt)) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = ssm::get_parameter(&aws, &name, decrypt);
            Msg::Value { name, decrypt, result }
        });
    }

    /// The value to show/copy: decrypted when revealed, plain for non-secure types.
    fn value_of(&self, p: &ssm::ParameterMetadata) -> Option<&aws::Result<ssm::Parameter>> {
        if p.kind == "SecureString" {
            if self.revealed.contains(&p.name) { self.decrypted.get(&p.name) } else { None }
        } else {
            self.values.get(&p.name)
        }
    }

    fn toggle_reveal(&mut self, ctx: &mut Ctx) {
        let Some(p) = self.selected_param() else { return };
        if p.kind != "SecureString" {
            return;
        }
        let name = p.name.clone();
        if !self.revealed.remove(&name) {
            self.revealed.insert(name.clone());
            if !self.decrypted.contains_key(&name) {
                self.fetch(name, true, ctx);
            }
        }
    }

    fn copy_value(&mut self, ctx: &mut Ctx) {
        let Some(p) = self.selected_param() else { return };
        match self.value_of(p) {
            Some(Ok(v)) => {
                let ok = crate::platform::copy_to_clipboard(&v.value);
                ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Value copied" } else { "Clipboard not available" });
            }
            Some(Err(e)) => ctx.toast(Level::Error, e.short()),
            None if p.kind == "SecureString" => {
                // Decrypt just for the copy, without showing it.
                let name = p.name.clone();
                self.copy_after_decrypt = Some(name.clone());
                if let Some(Ok(v)) = self.decrypted.get(&name) {
                    let ok = crate::platform::copy_to_clipboard(&v.value);
                    self.copy_after_decrypt = None;
                    ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Secret copied" } else { "Clipboard not available" });
                } else {
                    self.fetch(name, true, ctx);
                }
            }
            None => ctx.toast(Level::Info, "Value still loading"),
        }
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| match e {
                Entry::Up => ListItem::new(Line::styled("‹ ..", theme::muted())),
                Entry::Folder { path, count } => {
                    let label = path.strip_prefix(&self.prefix).unwrap_or(path);
                    ListItem::new(Line::from(vec![
                        Span::styled("▸ ", theme::accent()),
                        Span::styled(label.to_string(), theme::accent().bold()),
                        Span::styled(format!("  {count}"), theme::muted()),
                    ]))
                }
                Entry::Param(i) => {
                    let p = &self.all[*i];
                    let label = if self.filter.value.is_empty() {
                        p.name.strip_prefix(&self.prefix).unwrap_or(&p.name)
                    } else {
                        &p.name
                    };
                    let (badge, color) = match p.kind.as_str() {
                        "SecureString" => ("secure", theme::APPROVAL),
                        "StringList" => ("list", theme::WARN),
                        _ => ("", theme::MUTED),
                    };
                    let name = ellipsize(label, width.saturating_sub(badge.len() + 3));
                    let pad = width.saturating_sub(name.chars().count() + badge.len() + 2);
                    ListItem::new(Line::from(vec![
                        Span::raw("  "),
                        Span::raw(name),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(badge, Style::new().fg(color)),
                    ]))
                }
            })
            .collect();

        let title = match &self.load {
            LoadState::Loading => format!(" {} Loading… {} ", theme::spinner(view.tick), self.all.len()),
            _ if !self.filter.value.is_empty() => format!(" {} matches ", self.entries.len()),
            _ => format!(" {} ", if self.prefix.is_empty() { "/" } else { &self.prefix }),
        };
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search all parameters");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        if self.entries.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading parameters…", theme::muted()),
                _ => Line::styled("No parameters", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let entry = self.selected_entry();
        let Some(p) = self.selected_param() else {
            let text = match entry {
                Some(Entry::Folder { path, count }) => format!("{path} · {count} parameters — Enter to open"),
                _ => "Select a parameter".into(),
            };
            f.render_widget(
                widgets::placeholder(Line::styled(text, theme::muted())).block(widgets::panel(" Parameter ", false)),
                area,
            );
            return;
        };
        let kw = 12;
        let mut lines = vec![
            widgets::kv("Type", Span::styled(p.kind.clone(), theme::bold()), kw),
            widgets::kv("Version", p.version.map(|v| v.to_string()).unwrap_or_default(), kw),
            widgets::kv("Tier", p.tier.clone().unwrap_or_default(), kw),
        ];
        if let Some(t) = p.last_modified_date {
            lines.push(widgets::kv("Modified", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
        }
        if let Some(u) = &p.last_modified_user {
            lines.push(widgets::kv("By", u.rsplit('/').next().unwrap_or(u).to_string(), kw));
        }
        if let Some(d) = &p.data_type {
            lines.push(widgets::kv("Data type", d.clone(), kw));
        }
        if let Some(k) = &p.key_id {
            lines.push(widgets::kv("KMS key", k.clone(), kw));
        }
        if let Some(d) = p.description.as_ref().filter(|d| !d.is_empty()) {
            lines.push(widgets::kv("Description", d.clone(), kw));
        }
        lines.push(Line::raw(""));
        let secure = p.kind == "SecureString";
        lines.push(Line::from(vec![
            widgets::section("Value").spans.remove(0),
            Span::styled(
                match (secure, self.revealed.contains(&p.name)) {
                    (true, true) => "   v hide · c copy",
                    (true, false) => "   v reveal · c copy",
                    _ => "   c copy",
                },
                theme::muted(),
            ),
        ]));
        match self.value_of(p) {
            Some(Ok(v)) => {
                let text = pretty_json(&v.value).unwrap_or_else(|| v.value.clone());
                let style = if secure { Style::new().fg(theme::APPROVAL) } else { Style::new() };
                lines.extend(text.lines().map(|l| Line::styled(l.to_string(), style)));
            }
            Some(Err(e)) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            None if secure && !self.revealed.contains(&p.name) => {
                lines.push(Line::styled("•••••••••••• (hidden)", theme::muted()))
            }
            None => lines.push(Line::from(vec![
                Span::styled(theme::spinner(view.tick), theme::accent()),
                Span::raw(" loading…"),
            ])),
        }
        let block = widgets::panel(Line::from(format!(" {} ", p.name)).bold(), false);
        f.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block),
            area,
        );
    }
}

impl Module for Parameters {
    fn title(&self) -> &'static str {
        "Parameters"
    }

    fn description(&self) -> &'static str {
        "SSM Parameter Store · browse folders, search, reveal and copy values"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {
        *self = Parameters::new();
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
                InputEvent::Changed => self.rebuild(),
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
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.enter(),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.up(),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.rebuild();
            }
            KeyCode::Esc => self.up(),
            KeyCode::Char('v') => self.toggle_reveal(ctx),
            KeyCode::Char('c') => self.copy_value(ctx),
            KeyCode::Char('y') => {
                if let Some(p) = self.selected_param() {
                    let ok = crate::platform::copy_to_clipboard(&p.name);
                    ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Name copied" } else { "Clipboard not available" });
                }
            }
            KeyCode::Char('r') => self.load(ctx),
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
                // List starts below the filter bar and the panel border.
                let row = event.row.saturating_sub(self.list_area.y + 2) as usize;
                let index = self.list_state.offset() + row;
                if self.list_state.selected() == Some(index) {
                    self.enter();
                } else {
                    self.select(index);
                }
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
                self.rebuild();
                if first {
                    self.select(0);
                }
            }
            Msg::Done => self.load = LoadState::Loaded,
            Msg::Failed(e) => self.load = LoadState::Failed(e),
            Msg::Value { name, decrypt, result } => {
                self.in_flight.remove(&(name.clone(), decrypt));
                if decrypt {
                    if self.copy_after_decrypt.as_deref() == Some(name.as_str()) {
                        self.copy_after_decrypt = None;
                        if let Ok(v) = &result {
                            let ok = crate::platform::copy_to_clipboard(&v.value);
                            ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Secret copied" } else { "Clipboard not available" });
                        }
                    }
                    self.decrypted.insert(name, result);
                } else {
                    self.values.insert(name, result);
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
            let still_selected = self.selected_param().is_some_and(|p| p.name == name);
            if still_selected && !self.values.contains_key(&name) {
                self.fetch(name, false, ctx);
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [left, right] = if area.width < 90 {
            area.layout(&Layout::vertical([Constraint::Percentage(50), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(45), Constraint::Fill(1)]))
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
            ("Enter/←", "open/up"),
            ("/", "search"),
            ("v", "reveal"),
            ("c", "copy value"),
            ("y", "copy name"),
            ("r", "reload"),
        ]
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading) || self.pending.is_some() || !self.in_flight.is_empty()
    }

    fn capturing(&self) -> bool {
        self.filtering
    }
}

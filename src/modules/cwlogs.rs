//! CloudWatch Logs: list log groups, open one to read the events of every stream for a
//! time range (with a CloudWatch filter pattern), keep following new events, and read
//! them full screen. Other modules jump here (Lambda → its log group).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::{self, AwsError, logs};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_bytes, now_secs};

/// Time ranges `t` cycles through: (label, seconds).
const RANGES: [(&str, i64); 6] =
    [("5m", 300), ("15m", 900), ("1h", 3600), ("6h", 21_600), ("24h", 86_400), ("7d", 604_800)];
/// Events read at most when opening a group.
const MAX_EVENTS: usize = 10_000;
const FOLLOW_EVERY: Duration = Duration::from_secs(3);

enum Msg {
    Page(Vec<logs::LogGroup>),
    Done,
    Failed(AwsError),
    /// Events of the open group; `seq` drops results of a group or range left meanwhile.
    Events { seq: u64, result: aws::Result<Vec<logs::FilteredEvent>>, done: bool, truncated: bool },
}

enum Feed {
    Loading,
    Live,
    Paused,
    Failed(String),
}

struct Open {
    group: String,
    seq: u64,
    view: LogView,
    feed: Feed,
    /// Newest event timestamp read (ms) and the ids read at that millisecond, to continue
    /// following without duplicates.
    last_ms: i64,
    ids_at_last: HashSet<String>,
    in_flight: bool,
    last_fetch: Instant,
    truncated: bool,
}

pub struct CwLogs {
    groups: Vec<logs::LogGroup>,
    load: LoadState,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    list_state: ListState,
    open: Option<Open>,
    seq: u64,
    range: usize,
    pattern: String,
    editing_pattern: Option<TextInput>,
    region: String,
    list_area: Rect,
    log_area: Rect,
}

impl CwLogs {
    pub fn new() -> CwLogs {
        CwLogs {
            groups: Vec::new(),
            load: LoadState::Idle,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            open: None,
            seq: 0,
            range: 1,
            pattern: String::new(),
            editing_pattern: None,
            region: String::new(),
            list_area: Rect::default(),
            log_area: Rect::default(),
        }
    }

    fn load(&mut self, ctx: &mut Ctx) {
        self.groups.clear();
        self.visible.clear();
        self.load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let mut token: Option<String> = None;
            loop {
                match logs::describe_log_groups(&aws, None, token.as_deref()) {
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
        let keep = self.selected().map(|g| g.log_group_name.clone());
        let query = self.filter.value.trim().to_lowercase();
        self.visible = (0..self.groups.len()).filter(|&i| matches_filter(&self.groups[i].log_group_name, &query)).collect();
        let index = keep.and_then(|k| self.visible.iter().position(|&i| self.groups[i].log_group_name == k)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    fn selected(&self) -> Option<&logs::LogGroup> {
        self.groups.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn select(&mut self, index: usize) {
        if !self.visible.is_empty() {
            self.list_state.select(Some(index.min(self.visible.len() - 1)));
        }
    }

    /// Opens `group` (or reopens it with the current range and pattern).
    fn open_group(&mut self, group: String, ctx: &mut Ctx) {
        self.seq += 1;
        let zoomed = self.open.as_ref().is_some_and(|o| o.view.zoomed && o.group == group);
        let mut view = LogView::default();
        view.zoomed = zoomed;
        self.open = Some(Open {
            group: group.clone(),
            seq: self.seq,
            view,
            feed: Feed::Loading,
            last_ms: 0,
            ids_at_last: HashSet::new(),
            in_flight: true,
            last_fetch: Instant::now(),
            truncated: false,
        });
        let start_ms = ((now_secs() as i64) - RANGES[self.range].1) * 1000;
        let (aws, pattern, seq) = (ctx.env().aws.clone(), self.pattern.clone(), self.seq);
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let mut token: Option<String> = None;
            let mut read = 0;
            // FilterLogEvents can return empty pages while it scans streams; bound the pages.
            for _ in 0..50 {
                match logs::filter_log_events(&aws, &group, start_ms, &pattern, token.as_deref()) {
                    Ok((events, next)) => {
                        read += events.len();
                        let truncated = read >= MAX_EVENTS && next.is_some();
                        let done = next.is_none() || truncated;
                        if !em.emit(Msg::Events { seq, result: Ok(events), done, truncated }) || done {
                            return;
                        }
                        token = next;
                    }
                    Err(e) => {
                        em.emit(Msg::Events { seq, result: Err(e), done: true, truncated: false });
                        return;
                    }
                }
            }
            em.emit(Msg::Events { seq, result: Ok(Vec::new()), done: true, truncated: true });
        });
    }

    /// New events since the newest one read.
    fn follow(&mut self, ctx: &mut Ctx) {
        let Some(open) = &mut self.open else { return };
        open.in_flight = true;
        open.last_fetch = Instant::now();
        let (aws, pattern, seq) = (ctx.env().aws.clone(), self.pattern.clone(), open.seq);
        let (group, start_ms) = (open.group.clone(), open.last_ms.max(((now_secs() as i64) - 60) * 1000));
        ctx.spawn(move || {
            let mut all = Vec::new();
            let mut token: Option<String> = None;
            for _ in 0..10 {
                match logs::filter_log_events(&aws, &group, start_ms, &pattern, token.as_deref()) {
                    Ok((events, next)) => {
                        all.extend(events);
                        match next {
                            Some(t) => token = Some(t),
                            None => break,
                        }
                    }
                    Err(e) => return Msg::Events { seq, result: Err(e), done: true, truncated: false },
                }
            }
            Msg::Events { seq, result: Ok(all), done: true, truncated: false }
        });
    }

    fn on_events(&mut self, result: aws::Result<Vec<logs::FilteredEvent>>, done: bool, truncated: bool, ctx: &mut Ctx) {
        let Some(open) = &mut self.open else { return };
        match result {
            Ok(events) => {
                for e in events {
                    if e.timestamp < open.last_ms || (e.timestamp == open.last_ms && open.ids_at_last.contains(&e.event_id)) {
                        continue;
                    }
                    if e.timestamp > open.last_ms {
                        open.last_ms = e.timestamp;
                        open.ids_at_last.clear();
                    }
                    open.ids_at_last.insert(e.event_id.clone());
                    open.view.extend(logs::event_lines(&e, RANGES[self.range].1 > 86_400));
                }
                open.truncated |= truncated;
                if done {
                    open.in_flight = false;
                    if matches!(open.feed, Feed::Loading) {
                        open.feed = Feed::Live;
                    }
                }
            }
            Err(e) if e.is_login_required() => {
                open.in_flight = false;
                open.feed = Feed::Failed(e.short());
                ctx.toast(Level::Warn, e.short());
            }
            Err(e) => {
                open.in_flight = false;
                open.feed = Feed::Failed(e.short());
            }
        }
    }

    fn zoomed(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.view.zoomed)
    }

    fn copy_lines(&self, ctx: &mut Ctx) {
        let Some(open) = &self.open else { return };
        let ok = crate::platform::copy_to_clipboard(&open.view.lines().join("\n"));
        let n = open.view.lines().len();
        ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { format!("Copied {n} lines") } else { "Clipboard not available".into() });
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search log groups");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = match &self.load {
            LoadState::Loading => format!(" {} Log groups · {} ", theme::spinner(view.tick), self.groups.len()),
            _ => format!(" Log groups · {} ", self.visible.len()),
        };
        if self.visible.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading log groups…", theme::muted()),
                _ => Line::styled("No log groups", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let open = self.open.as_ref().map(|o| o.group.as_str());
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|&i| {
                let g = &self.groups[i];
                let size = g.stored_bytes.map(fmt_bytes).unwrap_or_default();
                let name = ellipsize(&g.log_group_name, width.saturating_sub(size.len() + 3));
                let pad = width.saturating_sub(name.chars().count() + size.len() + 2);
                let style = if open == Some(g.log_group_name.as_str()) { theme::accent().bold() } else { Style::new() };
                ListItem::new(Line::from(vec![
                    Span::styled(if open == Some(g.log_group_name.as_str()) { "▸ " } else { "  " }, theme::accent()),
                    Span::styled(name, style),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(size, theme::muted()),
                ]))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, self.open.is_none())).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_events(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.log_area = area;
        let range = RANGES[self.range].0;
        let Some(open) = &mut self.open else {
            let text = vec![
                Line::raw(""),
                Line::styled("Select a log group and press Enter", theme::muted()).centered(),
                Line::raw(""),
                Line::styled(format!("Shows the last {range} of every stream · t changes the range · f filters"), theme::muted())
                    .centered(),
            ];
            f.render_widget(Paragraph::new(text).block(widgets::panel(" Events ", false)), area);
            return;
        };
        let (state, style) = match &open.feed {
            Feed::Loading => (format!("{} loading", theme::spinner(view.tick)), theme::accent()),
            Feed::Live if !open.view.following() => {
                (format!("↑ {} lines · End to follow", open.view.lines_above_end().unwrap_or(0)), theme::muted())
            }
            Feed::Live => (format!("{} live", theme::spinner(view.tick)), Style::new().fg(theme::RUNNING)),
            Feed::Paused => ("paused · p resumes".to_string(), theme::muted()),
            Feed::Failed(e) => (ellipsize(e, 50), Style::new().fg(theme::ERR)),
        };
        let mut title = format!(" {} · last {range} UTC", open.group);
        if !self.pattern.is_empty() {
            title.push_str(&format!(" · filter: {}", self.pattern));
        }
        if open.truncated {
            title.push_str(&format!(" · first {MAX_EVENTS} events"));
        }
        let block = widgets::panel(format!("{title} "), true).title_top(Line::styled(format!(" {state} "), style).right_aligned());
        let empty = match open.feed {
            Feed::Loading => "Reading events…",
            _ => "No events in this range. t widens it; f changes the filter.",
        };
        open.view.render_panel(f, area, block, empty);
    }

    fn render_pattern(&self, f: &mut Frame, area: Rect) {
        let Some(input) = &self.editing_pattern else { return };
        let (line, col) = input.line("filter › ", "words, \"exact phrase\", ?ERROR ?WARN, { $.level = \"error\" }");
        let lines = vec![
            Line::raw(""),
            line,
            Line::raw(""),
            Line::styled("CloudWatch filter syntax; empty shows everything.", theme::muted()),
            Line::raw(""),
            widgets::hints_line(&[("Enter", "apply"), ("Esc", "cancel")]),
        ];
        let rect = widgets::popup_area(area, 80, lines.len() as u16 + 2);
        widgets::popup(f, rect, "Filter events", theme::ACCENT, ratatui::text::Text::from(lines));
        f.set_cursor_position((rect.x + 2 + col, rect.y + 2));
    }
}

impl Module for CwLogs {
    fn title(&self) -> &'static str {
        "Logs"
    }

    fn description(&self) -> &'static str {
        "CloudWatch Logs · log groups, live events of every stream, filters"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = CwLogs::new();
        self.region = ctx.env().region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load(ctx);
        }
    }

    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        if let Some(pos) = self.visible.iter().position(|&i| self.groups[i].log_group_name == target) {
            self.select(pos);
        }
        self.open_group(target.to_string(), ctx);
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some(input) = &mut self.editing_pattern {
            match input.handle(key) {
                InputEvent::Submit => {
                    self.pattern = input.value.trim().to_string();
                    self.editing_pattern = None;
                    if let Some(group) = self.open.as_ref().map(|o| o.group.clone()) {
                        self.open_group(group, ctx);
                    }
                }
                InputEvent::Cancel => self.editing_pattern = None,
                _ => {}
            }
            return Handled::Yes;
        }
        if let Some(open) = self.open.as_mut().filter(|o| o.view.zoomed) {
            if let LogKey::Copy = open.view.key(key) {
                self.copy_lines(ctx);
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
            KeyCode::Enter => {
                if let Some(g) = self.selected() {
                    let group = g.log_group_name.clone();
                    self.open_group(group, ctx);
                }
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Esc => self.open = None,
            KeyCode::Char('t') => {
                self.range = (self.range + 1) % RANGES.len();
                if let Some(group) = self.open.as_ref().map(|o| o.group.clone()) {
                    self.open_group(group, ctx);
                }
            }
            KeyCode::Char('f') => self.editing_pattern = Some(TextInput::with_value(self.pattern.clone())),
            KeyCode::Char('p') => {
                if let Some(open) = &mut self.open {
                    open.feed = match open.feed {
                        Feed::Paused => Feed::Live,
                        Feed::Live => Feed::Paused,
                        _ => return Handled::Yes,
                    };
                }
            }
            KeyCode::Char('z') => {
                if self.open.is_none()
                    && let Some(g) = self.selected()
                {
                    let group = g.log_group_name.clone();
                    self.open_group(group, ctx);
                }
                if let Some(open) = &mut self.open {
                    open.view.zoomed = true;
                }
            }
            KeyCode::Char('y') => self.copy_lines(ctx),
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::End => {
                if let Some(open) = &mut self.open {
                    open.view.key(key);
                }
            }
            KeyCode::Char('o') => {
                let Some(group) = self.open.as_ref().map(|o| o.group.clone()).or_else(|| self.selected().map(|g| g.log_group_name.clone()))
                else {
                    return Handled::Yes;
                };
                let url = logs::console_url(&self.region, &group);
                if !crate::platform::open_url(&url) {
                    ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                }
            }
            KeyCode::Char('r') => self.load(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        let pos = (event.column, event.row).into();
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp if self.log_area.contains(pos) => {
                if let Some(open) = &mut self.open {
                    open.view.scroll(-3);
                }
            }
            MouseEventKind::ScrollDown if self.log_area.contains(pos) => {
                if let Some(open) = &mut self.open {
                    open.view.scroll(3);
                }
            }
            MouseEventKind::ScrollUp if self.list_area.contains(pos) => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown if self.list_area.contains(pos) => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) if self.list_area.contains(pos) => {
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
                let first = self.groups.is_empty();
                self.groups.extend(page);
                self.refilter();
                if first {
                    self.select(0);
                }
            }
            Msg::Done => self.load = LoadState::Loaded,
            Msg::Failed(e) => self.load = LoadState::Failed(e),
            Msg::Events { seq, result, done, truncated } => {
                if self.open.as_ref().is_some_and(|o| o.seq == seq) {
                    self.on_events(result, done, truncated, ctx);
                }
            }
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        let due = self.open.as_ref().is_some_and(|o| {
            matches!(o.feed, Feed::Live) && !o.in_flight && o.last_fetch.elapsed() >= FOLLOW_EVERY
        });
        if active && due {
            self.follow(ctx);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some(open) = self.open.as_mut().filter(|o| o.view.zoomed) {
            let title = format!("{} · last {} UTC", open.group, RANGES[self.range].0);
            open.view.render_zoomed(f, area, &title);
            return;
        }
        let [left, right] = if area.width < 100 {
            area.layout(&Layout::vertical([Constraint::Percentage(35), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(30), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        self.render_events(f, right, view);
        self.render_pattern(f, area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.editing_pattern.is_some() {
            return vec![("Enter", "apply filter"), ("Esc", "cancel")];
        }
        if self.zoomed() {
            return vec![("/", "search"), ("e", "next error"), ("y", "copy all"), ("Esc", "back")];
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        let mut h = vec![("↑↓", "group"), ("Enter", "open"), ("/", "search"), ("t", "range"), ("f", "filter")];
        if self.open.is_some() {
            h.extend([("z", "full screen"), ("p", "pause"), ("y", "copy"), ("PgUp/PgDn", "scroll")]);
        }
        h.extend([("o", "console"), ("r", "reload")]);
        h
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || self.open.as_ref().is_some_and(|o| matches!(o.feed, Feed::Loading | Feed::Live))
    }

    fn capturing(&self) -> bool {
        self.filtering || self.editing_pattern.is_some() || self.zoomed()
    }

    fn wants_mouse(&self) -> bool {
        !self.zoomed()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if let Some(input) = &mut self.editing_pattern {
            input.insert_str(text, false);
        } else if self.filtering {
            self.filter.insert_str(text, false);
            self.refilter();
        }
    }
}

//! Logs Insights, inside the Logs module: a query editor over one or more log groups, saved
//! and recent queries, the results as a table (with bars for `stats … by …`), whole
//! records full screen, and copies as Markdown, CSV or a report for an AI agent.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::RANGES;
use crate::aws::{self, logs};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Handled, Level, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::{theme, widgets};
use crate::util::{ellipsize, fmt_duration, now_secs, pretty_json, sanitize};

const DEFAULT_QUERY: &str = "fields @timestamp, @message, @logStream\n| sort @timestamp desc\n| limit 100";
const POLL_EVERY: Duration = Duration::from_secs(1);
/// Rows in the report for an AI agent, at most.
const REPORT_ROWS: usize = 200;
/// The widest a column gets; the last one shown takes the room left.
const COLUMN_MAX: usize = 48;
/// Queries of this session kept for the picker.
const HISTORY: usize = 20;
/// Insights commands, highlighted in the editor.
const COMMANDS: [&str; 12] =
    ["fields", "filter", "stats", "sort", "limit", "parse", "display", "dedup", "unmask", "pattern", "diff", "anomaly"];

pub(super) enum Msg {
    Started { seq: u64, result: aws::Result<String> },
    Results { seq: u64, result: aws::Result<logs::QueryResults> },
    Stopped { seq: u64, result: aws::Result<()> },
    Saved(aws::Result<Vec<logs::QueryDefinition>>),
    Record { seq: u64, title: String, result: aws::Result<Vec<(String, String)>> },
}

fn wrap(msg: Msg) -> super::Msg {
    super::Msg::Insights(msg)
}

// ---- the query editor -------------------------------------------------------------------

enum Edit {
    Run,
    Leave,
    Done,
}

/// A small multi-line editor: Enter runs, Alt+Enter (or Ctrl+J) breaks the line.
struct Editor {
    lines: Vec<String>,
    /// Cursor: line, and character in it.
    row: usize,
    col: usize,
}

impl Editor {
    fn new(text: &str) -> Editor {
        let mut editor = Editor { lines: vec![String::new()], row: 0, col: 0 };
        editor.insert(text);
        editor
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn byte(&self, col: usize) -> usize {
        let line = &self.lines[self.row];
        line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
    }

    fn len(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    /// Inserts typed or pasted text at the cursor, keeping its line breaks.
    fn insert(&mut self, text: &str) {
        let text = sanitize(&text.replace("\r\n", "\n").replace('\r', "\n")).replace('\t', "  ");
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            let at = self.byte(self.col);
            self.lines[self.row].insert_str(at, part);
            self.col += part.chars().count();
        }
    }

    fn newline(&mut self) {
        let at = self.byte(self.col);
        let rest = self.lines[self.row].split_off(at);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    fn handle(&mut self, key: KeyEvent) -> Edit {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => self.newline(),
            KeyCode::Char('j') if ctrl => self.newline(),
            KeyCode::Enter => return Edit::Run,
            KeyCode::Esc | KeyCode::Tab => return Edit::Leave,
            KeyCode::Char('u') if ctrl => *self = Editor::new(""),
            KeyCode::Char(c) if !ctrl => self.insert(&c.to_string()),
            KeyCode::Backspace if self.col > 0 => {
                self.col -= 1;
                let at = self.byte(self.col);
                self.lines[self.row].remove(at);
            }
            KeyCode::Backspace if self.row > 0 => {
                let line = self.lines.remove(self.row);
                self.row -= 1;
                self.col = self.len(self.row);
                self.lines[self.row].push_str(&line);
            }
            KeyCode::Delete if self.col < self.len(self.row) => {
                let at = self.byte(self.col);
                self.lines[self.row].remove(at);
            }
            KeyCode::Delete if self.row + 1 < self.lines.len() => {
                let next = self.lines.remove(self.row + 1);
                self.lines[self.row].push_str(&next);
            }
            KeyCode::Left if self.col > 0 => self.col -= 1,
            KeyCode::Left if self.row > 0 => {
                self.row -= 1;
                self.col = self.len(self.row);
            }
            KeyCode::Right if self.col < self.len(self.row) => self.col += 1,
            KeyCode::Right if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.col = 0;
            }
            KeyCode::Up if self.row > 0 => {
                self.row -= 1;
                self.col = self.col.min(self.len(self.row));
            }
            KeyCode::Down if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.col = self.col.min(self.len(self.row));
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.len(self.row),
            _ => {}
        }
        Edit::Done
    }

    /// The lines that fit `width` × `height` around the cursor, commands highlighted, and
    /// where the cursor is in them.
    fn render(&self, width: usize, height: usize) -> (Vec<Line<'static>>, (u16, u16)) {
        let top = self.row.saturating_sub(height.saturating_sub(1));
        let mut cursor = (0, 0);
        let lines = self.lines[top..(top + height).min(self.lines.len())]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let row = top + i;
                // The cursor's line scrolls sideways to keep the cursor in view.
                let skip = if row == self.row { (self.col + 1).saturating_sub(width) } else { 0 };
                if row == self.row {
                    cursor = ((self.col - skip) as u16, i as u16);
                }
                highlight(line, skip, width)
            })
            .collect();
        (lines, cursor)
    }
}

/// A query line with `|` and the command after it highlighted, `width` characters from `skip`.
fn highlight(line: &str, skip: usize, width: usize) -> Line<'static> {
    let chars: Vec<char> = line.chars().collect();
    let mut styles = vec![Style::new(); chars.len()];
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '|' {
            styles[i] = theme::accent();
        }
        // A command starts the line or follows a pipe.
        let starts = chars[..i].iter().rev().find(|c| !c.is_whitespace()).is_none_or(|c| *c == '|');
        if chars[i].is_alphabetic() && starts {
            let end = (i..chars.len()).find(|&j| !chars[j].is_alphanumeric()).unwrap_or(chars.len());
            let word: String = chars[i..end].iter().collect();
            if COMMANDS.contains(&word.to_lowercase().as_str()) {
                styles[i..end].fill(theme::accent().bold());
            }
            i = end;
            continue;
        }
        i += 1;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (c, style) in chars.iter().zip(&styles).skip(skip).take(width) {
        match spans.last_mut() {
            Some(last) if last.style == *style => last.content.to_mut().push(*c),
            _ => spans.push(Span::styled(c.to_string(), *style)),
        }
    }
    Line::from(spans)
}

// ---- the screen -------------------------------------------------------------------------

/// The query running or last run.
struct Run {
    id: Option<String>,
    query: String,
    groups: Vec<String>,
    /// `last 1h · 2026-10-21 13:46 → 14:46 UTC`
    range: String,
    /// `1h`
    span: &'static str,
    started: Instant,
    took: Option<Duration>,
    in_flight: bool,
    last_poll: Instant,
    stopping: bool,
}

enum Saved {
    Loading,
    Loaded(Vec<logs::QueryDefinition>),
    Failed(String),
}

#[derive(Clone)]
struct Recent {
    query: String,
    groups: Vec<String>,
    language: Option<String>,
}

enum Pick {
    Saved(usize),
    Recent(usize),
}

enum Choosing {
    Queries(Picker, Vec<Pick>),
    Groups(Picker, Vec<String>),
}

pub(super) struct Insights {
    pub(super) groups: Vec<String>,
    range: usize,
    editor: Editor,
    editing: bool,
    /// The query language of a saved query picked (SQL, PPL); `None` for Logs Insights.
    language: Option<String>,
    seq: u64,
    run: Option<Run>,
    results: Option<logs::QueryResults>,
    table: logs::Table,
    error: Option<String>,
    row: usize,
    offset: usize,
    /// First column shown.
    col: usize,
    saved: Saved,
    /// Open the saved queries once they load.
    pick_when_loaded: bool,
    recent: Vec<Recent>,
    choosing: Option<Choosing>,
    record: Option<(String, LogView)>,
    table_area: Rect,
}

impl Insights {
    pub(super) fn new(groups: Vec<String>, range: usize, ctx: &mut Ctx) -> Insights {
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || wrap(Msg::Saved(logs::query_definitions(&aws))));
        Insights {
            groups,
            range,
            editor: Editor::new(DEFAULT_QUERY),
            editing: true,
            language: None,
            seq: 0,
            run: None,
            results: None,
            table: logs::Table::default(),
            error: None,
            row: 0,
            offset: 0,
            col: 0,
            saved: Saved::Loading,
            pick_when_loaded: false,
            recent: Vec::new(),
            choosing: None,
            record: None,
            table_area: Rect::default(),
        }
    }

    fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.took.is_none())
    }

    pub(super) fn capturing(&self) -> bool {
        self.editing || self.choosing.is_some() || self.record.is_some()
    }

    pub(super) fn animating(&self) -> bool {
        self.running() || matches!(self.saved, Saved::Loading)
    }

    pub(super) fn wants_mouse(&self) -> bool {
        self.record.is_none()
    }

    fn run_query(&mut self, ctx: &mut Ctx) {
        let query = self.editor.text().trim().to_string();
        if query.is_empty() {
            self.error = Some("Write a query first".into());
            return;
        }
        if self.groups.is_empty() {
            self.error = Some("No log group to query: g picks them".into());
            return;
        }
        self.stop(ctx);
        self.seq += 1;
        let end = now_secs() as i64;
        let start = end - RANGES[self.range].1;
        let recent = Recent { query: query.clone(), groups: self.groups.clone(), language: self.language.clone() };
        self.recent.retain(|r| r.query != recent.query || r.groups != recent.groups);
        self.recent.insert(0, recent);
        self.recent.truncate(HISTORY);
        self.run = Some(Run {
            id: None,
            query: query.clone(),
            groups: self.groups.clone(),
            range: logs::range_label(RANGES[self.range].0, start, end),
            span: RANGES[self.range].0,
            started: Instant::now(),
            took: None,
            in_flight: false,
            last_poll: Instant::now(),
            stopping: false,
        });
        (self.results, self.table, self.error) = (None, logs::Table::default(), None);
        (self.row, self.offset, self.col) = (0, 0, 0);
        self.editing = false;
        let (aws, seq, groups, language) = (ctx.env().aws.clone(), self.seq, self.groups.clone(), self.language.clone());
        ctx.spawn(move || {
            let result = logs::start_query(&aws, &groups, (start, end), &query, language.as_deref(), None);
            wrap(Msg::Started { seq, result })
        });
    }

    /// Stops the running query (Insights bills what it scans until then).
    fn stop(&mut self, ctx: &mut Ctx) {
        let Some(run) = self.run.as_mut().filter(|r| r.took.is_none() && !r.stopping) else { return };
        let Some(id) = run.id.clone() else { return };
        run.stopping = true;
        let (aws, seq) = (ctx.env().aws.clone(), self.seq);
        ctx.spawn(move || wrap(Msg::Stopped { seq, result: logs::stop_query(&aws, &id) }));
    }

    pub(super) fn tick(&mut self, ctx: &mut Ctx) {
        let Some(run) = self.run.as_mut() else { return };
        let Some(id) = run.id.clone().filter(|_| run.took.is_none() && !run.in_flight && run.last_poll.elapsed() >= POLL_EVERY) else {
            return;
        };
        run.in_flight = true;
        run.last_poll = Instant::now();
        let (aws, seq) = (ctx.env().aws.clone(), self.seq);
        ctx.spawn(move || wrap(Msg::Results { seq, result: logs::get_query_results(&aws, &id) }));
    }

    pub(super) fn task(&mut self, msg: Msg, ctx: &mut Ctx) {
        match msg {
            Msg::Saved(result) => {
                self.saved = match result {
                    Ok(defs) => Saved::Loaded(defs),
                    Err(e) => Saved::Failed(e.short()),
                };
                if std::mem::take(&mut self.pick_when_loaded) {
                    self.pick_query(ctx);
                }
            }
            Msg::Started { seq, result } if seq == self.seq => {
                let Some(run) = self.run.as_mut() else { return };
                match result {
                    Ok(id) => {
                        run.id = Some(id);
                        // The first look comes sooner: small queries finish in well under a second.
                        run.last_poll = Instant::now().checked_sub(POLL_EVERY / 2).unwrap_or_else(Instant::now);
                    }
                    Err(e) => {
                        run.took = Some(run.started.elapsed());
                        self.error = Some(e.short());
                    }
                }
            }
            Msg::Results { seq, result } if seq == self.seq => {
                let Some(run) = self.run.as_mut() else { return };
                run.in_flight = false;
                match result {
                    Ok(r) => {
                        if !r.running() {
                            run.took = Some(run.started.elapsed());
                        }
                        self.table = logs::Table::new(&r.results);
                        self.row = self.row.min(self.table.rows.len().saturating_sub(1));
                        self.results = Some(r);
                    }
                    Err(e) => {
                        run.took = Some(run.started.elapsed());
                        self.error = Some(e.short());
                    }
                }
            }
            Msg::Stopped { seq, result } if seq == self.seq => match result {
                Ok(()) => {
                    if let Some(run) = self.run.as_mut() {
                        run.took = Some(run.started.elapsed());
                    }
                    if let Some(r) = self.results.as_mut() {
                        r.status = "Cancelled".into();
                    } else {
                        self.results = Some(logs::QueryResults { status: "Cancelled".into(), ..Default::default() });
                    }
                }
                Err(e) => {
                    if let Some(run) = self.run.as_mut() {
                        run.stopping = false;
                    }
                    ctx.toast(Level::Warn, format!("Could not stop the query: {}", e.short()));
                }
            },
            Msg::Record { seq, title, result } if seq == self.seq => match result {
                Ok(fields) => self.show_record(title, &fields),
                Err(e) => ctx.toast(Level::Error, e.short()),
            },
            _ => {}
        }
    }

    fn show_record(&mut self, title: String, fields: &[(String, String)]) {
        let width = fields.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0).min(20);
        let mut lines = Vec::new();
        for (key, value) in fields {
            let value = pretty_json(value).unwrap_or_else(|| value.clone());
            for (i, line) in value.lines().enumerate() {
                let key = if i == 0 { key.as_str() } else { "" };
                lines.push(format!("{key:<width$}  {line}"));
            }
            if value.is_empty() {
                lines.push(key.clone());
            }
        }
        let mut view = LogView::default();
        view.extend(lines);
        view.zoomed = true;
        // Start at the top, not following the end like a log.
        view.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        self.record = Some((title, view));
    }

    /// The selected row's whole record: read through its `@ptr`, or the row itself when it has
    /// none (results of `stats`).
    fn open_record(&mut self, ctx: &mut Ctx) {
        let Some(values) = self.table.rows.get(self.row) else { return };
        let title = format!("Record {} of {}", self.row + 1, self.table.rows.len());
        match self.table.ptrs.get(self.row).cloned().flatten() {
            Some(ptr) => {
                let (aws, seq) = (ctx.env().aws.clone(), self.seq);
                ctx.spawn(move || wrap(Msg::Record { seq, title, result: logs::get_log_record(&aws, &ptr) }));
            }
            None => {
                let fields: Vec<(String, String)> = self.table.columns.iter().cloned().zip(values.iter().cloned()).collect();
                self.show_record(title, &fields);
            }
        }
    }

    fn pick_query(&mut self, ctx: &mut Ctx) {
        let defs: &[logs::QueryDefinition] = match &self.saved {
            Saved::Loaded(defs) => defs,
            Saved::Loading => {
                self.pick_when_loaded = true;
                return ctx.toast(Level::Info, "Loading the saved queries…");
            }
            Saved::Failed(e) => {
                ctx.toast(Level::Warn, format!("Saved queries unavailable: {e}"));
                &[]
            }
        };
        // Saved queries for these log groups first, then those for any group, then the rest.
        let rank = |d: &logs::QueryDefinition| match d.log_group_names.is_empty() {
            true => 1,
            false if d.log_group_names.iter().any(|g| self.groups.contains(g)) => 0,
            false => 2,
        };
        let mut order: Vec<usize> = (0..defs.len()).collect();
        order.sort_by_key(|&i| (rank(&defs[i]), defs[i].name.to_lowercase()));
        let one_line = |q: &str| q.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut items = Vec::new();
        let mut picks = Vec::new();
        for i in order {
            let d = &defs[i];
            let groups = match d.log_group_names.len() {
                0 => String::new(),
                1 => format!(" · {}", d.log_group_names[0]),
                n => format!(" · {n} log groups"),
            };
            items.push(Item::new(sanitize(&d.name), sanitize(&format!("{}{groups}", ellipsize(&one_line(&d.query_string), 60)))));
            picks.push(Pick::Saved(i));
        }
        for (i, r) in self.recent.iter().enumerate() {
            let groups = match r.groups.as_slice() {
                [one] => one.clone(),
                many => format!("{} log groups", many.len()),
            };
            items.push(Item::new("recent", format!("{} · {groups}", one_line(&r.query))).color(theme::MUTED));
            picks.push(Pick::Recent(i));
        }
        if items.is_empty() {
            return ctx.toast(Level::Info, "No saved queries in this account, and none run yet");
        }
        self.choosing = Some(Choosing::Queries(Picker::new("Saved and recent queries", "use", items, false), picks));
    }

    fn pick_groups(&mut self, all: &[String], ctx: &mut Ctx) {
        if all.is_empty() {
            return ctx.toast(Level::Info, "The log groups are still loading");
        }
        let items: Vec<Item> = all.iter().map(|g| Item::new(g.clone(), "").current(self.groups.contains(g))).collect();
        let first = all.iter().position(|g| self.groups.contains(g)).unwrap_or(0);
        let picker = Picker::new("Log groups to query (Space marks several)", "use", items, true).with_cursor(first);
        self.choosing = Some(Choosing::Groups(picker, all.to_vec()));
    }

    fn chose(&mut self, choosing: Choosing, picked: Vec<usize>) {
        match choosing {
            Choosing::Queries(_, picks) => {
                let Some(pick) = picked.first().and_then(|&i| picks.get(i)) else { return };
                let (query, groups, language) = match (pick, &self.saved) {
                    (Pick::Saved(i), Saved::Loaded(defs)) => {
                        let d = &defs[*i];
                        (d.query_string.clone(), d.log_group_names.clone(), d.query_language.clone())
                    }
                    (Pick::Recent(i), _) => {
                        let r = self.recent[*i].clone();
                        (r.query, r.groups, r.language)
                    }
                    _ => return,
                };
                self.editor = Editor::new(&query);
                if !groups.is_empty() {
                    self.groups = groups;
                }
                self.language = language;
                self.editing = true;
            }
            Choosing::Groups(_, names) => {
                let groups: Vec<String> = picked.into_iter().filter_map(|i| names.get(i).cloned()).collect();
                if !groups.is_empty() {
                    self.groups = groups;
                }
            }
        }
    }

    fn copy(&self, what: &str, ctx: &mut Ctx) {
        let Some(results) = &self.results else {
            return ctx.toast(Level::Info, "Run a query first");
        };
        let text = match what {
            "csv" => self.table.csv(),
            "markdown" => self.table.markdown(usize::MAX),
            _ => {
                let Some(run) = &self.run else { return };
                let env = ctx.env();
                let report = logs::Report { env: &env.name, region: &env.region, groups: &run.groups, range: &run.range, query: &run.query };
                report.markdown(results, &self.table, REPORT_ROWS)
            }
        };
        let rows = logs::rows(self.table.rows.len());
        let done = match what {
            "csv" => format!("Copied {rows} as CSV"),
            "markdown" => format!("Copied {rows} as a Markdown table"),
            _ => "Copied the query and its results. Paste them into your AI agent.".into(),
        };
        if crate::platform::copy_to_clipboard(&text) {
            ctx.toast(Level::Success, done);
        } else {
            ctx.toast(Level::Warn, "Clipboard not available");
        }
    }

    fn select(&mut self, row: usize) {
        self.row = row.min(self.table.rows.len().saturating_sub(1));
    }

    /// Handles a key; false closes Insights.
    pub(super) fn key(&mut self, key: KeyEvent, all_groups: &[String], ctx: &mut Ctx) -> (Handled, bool) {
        if let Some((_, view)) = &mut self.record {
            if let LogKey::Copy = view.key(key) {
                let ok = crate::platform::copy_to_clipboard(&view.lines().join("\n"));
                ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Copied the record" } else { "Clipboard not available" });
            }
            if !view.zoomed {
                self.record = None;
            }
            return (Handled::Yes, true);
        }
        if let Some(choosing) = &mut self.choosing {
            let picker = match choosing {
                Choosing::Queries(p, _) | Choosing::Groups(p, _) => p,
            };
            match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.choosing = None,
                Outcome::Done(picked) => {
                    if let Some(choosing) = self.choosing.take() {
                        self.chose(choosing, picked);
                    }
                }
            }
            return (Handled::Yes, true);
        }
        if self.editing {
            match self.editor.handle(key) {
                Edit::Run => self.run_query(ctx),
                Edit::Leave => self.editing = false,
                Edit::Done => self.error = None,
            }
            return (Handled::Yes, true);
        }
        let page = self.table_area.height.saturating_sub(3).max(1) as usize;
        match key.code {
            KeyCode::Esc if self.running() => self.stop(ctx),
            KeyCode::Esc | KeyCode::Backspace => return (Handled::Yes, false),
            KeyCode::Up | KeyCode::Char('k') => self.select(self.row.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(self.row + 1),
            KeyCode::PageUp => self.select(self.row.saturating_sub(page)),
            KeyCode::PageDown => self.select(self.row + page),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(usize::MAX),
            KeyCode::Left | KeyCode::Char('h') => self.col = self.col.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => self.col = (self.col + 1).min(self.table.columns.len().saturating_sub(1)),
            KeyCode::Enter if !self.table.rows.is_empty() => self.open_record(ctx),
            KeyCode::Enter | KeyCode::Char('i') => self.editing = true,
            KeyCode::Char('r') => self.run_query(ctx),
            KeyCode::Char('t') => {
                // A query already run runs again over the new range (a running one is stopped).
                self.range = (self.range + 1) % RANGES.len();
                if self.run.is_some() {
                    self.run_query(ctx);
                }
            }
            KeyCode::Char('g') => self.pick_groups(all_groups, ctx),
            KeyCode::Char('s') => self.pick_query(ctx),
            KeyCode::Char('y') => self.copy("markdown", ctx),
            KeyCode::Char('Y') => self.copy("csv", ctx),
            KeyCode::Char('c') => self.copy("report", ctx),
            KeyCode::Char('o') => {
                let url = logs::insights_console_url(&ctx.env().region);
                if !crate::platform::open_url(&url) {
                    ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                }
            }
            _ => return (Handled::No, true),
        }
        (Handled::Yes, true)
    }

    pub(super) fn mouse(&mut self, event: MouseEvent) -> Handled {
        if self.capturing() || !self.table_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.select(self.row.saturating_sub(3)),
            MouseEventKind::ScrollDown => self.select(self.row + 3),
            // Rows start below the border and the header.
            MouseEventKind::Down(MouseButton::Left) => match event.row.checked_sub(self.table_area.y + 2) {
                Some(r) => self.select(self.offset + r as usize),
                None => return Handled::No,
            },
            _ => return Handled::No,
        }
        Handled::Yes
    }

    pub(super) fn paste(&mut self, text: &str) {
        match &mut self.choosing {
            Some(Choosing::Queries(p, _) | Choosing::Groups(p, _)) => p.paste(text),
            None if self.editing => self.editor.insert(text),
            None => {}
        }
    }

    pub(super) fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.record.is_some() {
            return vec![("/", "search"), ("y", "copy"), ("Esc", "back")];
        }
        match &self.choosing {
            Some(Choosing::Groups(..)) => return vec![("Space", "mark"), ("Enter", "use"), ("Esc", "cancel")],
            Some(Choosing::Queries(..)) => return vec![("↑↓", "move"), ("Enter", "use"), ("Esc", "cancel")],
            None => {}
        }
        if self.editing {
            return vec![("Enter", "run"), ("Alt+Enter", "new line"), ("^U", "clear"), ("Esc", "results")];
        }
        let mut h = vec![("↑↓", "row"), ("←→", "columns"), ("Enter", "record"), ("i", "edit query"), ("r", "run again")];
        h.extend([("t", "range"), ("g", "log groups"), ("s", "saved queries"), ("y/Y", "copy table/CSV"), ("c", "copy for AI")]);
        h.extend([("o", "console"), ("Esc", if self.running() { "stop" } else { "back" })]);
        h
    }

    // ---- rendering -------------------------------------------------------------------------

    pub(super) fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some((title, record)) = &mut self.record {
            let title = title.clone();
            record.render_zoomed(f, area, &title);
            return;
        }
        let rows = self.editor.lines.len().clamp(3, 8) as u16;
        let [editor, results] = area.layout(&Layout::vertical([Constraint::Length(rows + 2), Constraint::Fill(1)]));
        self.render_editor(f, editor);
        self.render_results(f, results, view);
        if let Some(choosing) = &mut self.choosing {
            let picker = match choosing {
                Choosing::Queries(p, _) | Choosing::Groups(p, _) => p,
            };
            let rect = widgets::popup_area(area, 96, picker.height(area.height.saturating_sub(4)));
            picker.render(f, rect, theme::ACCENT);
        }
    }

    fn render_editor(&self, f: &mut Frame, area: Rect) {
        let groups = match self.groups.as_slice() {
            [] => "no log group (g)".to_string(),
            [one] => one.clone(),
            [first, rest @ ..] => format!("{first} +{}", rest.len()),
        };
        let language = self.language.as_deref().filter(|l| *l != "CWLI").map(|l| format!(" · {l}")).unwrap_or_default();
        let title = format!(" Logs Insights · {groups} · last {}{language} ", RANGES[self.range].0);
        let hint = if self.editing { " Enter run · Alt+Enter new line · Esc results " } else { " i edits the query " };
        let block = widgets::panel(Line::from(title).bold(), self.editing).title_bottom(Line::styled(hint, theme::muted()).right_aligned());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let (lines, (col, row)) = self.editor.render(inner.width as usize, inner.height as usize);
        f.render_widget(Paragraph::new(lines), inner);
        if self.editing {
            f.set_cursor_position((inner.x + col, inner.y + row));
        }
    }

    /// The query's state for the results title; `compact` for narrow terminals.
    fn status(&self, view: &View, compact: bool) -> Line<'static> {
        let Some(run) = &self.run else { return Line::raw("") };
        let summary = self.results.as_ref().map(|r| {
            if compact {
                let s = &r.statistics;
                format!("{} matched · {}", logs::count(s.records_matched), crate::util::fmt_bytes(s.bytes_scanned as i64))
            } else {
                r.summary()
            }
        });
        match (run.took, &self.results) {
            (None, _) => {
                let elapsed = fmt_duration(run.started.elapsed().as_secs_f64());
                let what = if run.stopping { "stopping" } else { "running" };
                let text = match summary {
                    Some(s) => format!(" {} {what} · {s} · {elapsed} ", theme::spinner(view.tick)),
                    None => format!(" {} {what} · {elapsed} ", theme::spinner(view.tick)),
                };
                Line::styled(text, Style::new().fg(theme::RUNNING))
            }
            (Some(took), Some(r)) => {
                let (symbol, color) = match r.status.as_str() {
                    "Complete" => ("✓", theme::OK),
                    "Cancelled" => ("■", theme::WARN),
                    _ => ("✗", theme::ERR),
                };
                let text = format!(" {symbol} {} · {} · {} ", r.status.to_lowercase(), summary.unwrap_or_default(), fmt_duration(took.as_secs_f64()));
                Line::styled(text, Style::new().fg(color))
            }
            (Some(_), None) => Line::styled(" ✗ failed ", Style::new().fg(theme::ERR)),
        }
    }

    fn render_results(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.table_area = area;
        let rows = self.table.rows.len();
        let mut title = " Results ".to_string();
        if let Some(run) = &self.run {
            if rows > 0 {
                title.push_str(&format!("· {} ", logs::rows(rows)));
            }
            title.push_str(&format!("· last {} ", run.span));
        }
        let block = widgets::panel(title, !self.editing).title_top(self.status(view, area.width < 120).right_aligned());
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut notes: Vec<Line> = Vec::new();
        if let Some(e) = &self.error {
            notes.push(Line::styled(format!("✗ {e}"), Style::new().fg(theme::ERR)));
        }
        if self.table.columns.is_empty() {
            match &self.run {
                None => {
                    let n = self.groups.len();
                    notes.push(Line::styled(
                        format!("Enter runs the query over the last {} of {n} log group{}.", RANGES[self.range].0, if n == 1 { "" } else { "s" }),
                        theme::muted(),
                    ));
                    notes.push(Line::styled(
                        "Insights bills the data it scans: on big groups, narrow the range (t) or the groups (g).",
                        theme::muted(),
                    ));
                    notes.push(Line::styled("Out of the editor (Esc): s saved and recent queries · g log groups · t range.", theme::muted()));
                }
                Some(run) if run.took.is_none() => notes.push(Line::styled("Waiting for the first results…", theme::muted())),
                Some(_) if self.results.as_ref().is_some_and(|r| r.status == "Cancelled") => {
                    notes.push(Line::styled("Stopped before any result. r runs it again.", theme::muted()))
                }
                Some(_) if self.error.is_none() => notes.push(Line::styled("No results.", theme::muted())),
                Some(_) => {}
            }
            f.render_widget(Paragraph::new(notes), inner);
            return;
        }
        let [header, body] = inner.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let height = body.height as usize;
        if self.row < self.offset {
            self.offset = self.row;
        } else if self.row >= self.offset + height {
            self.offset = self.row + 1 - height;
        }
        let layout = columns(&self.table, self.col, inner.width as usize);
        f.render_widget(Paragraph::new(layout.line(&self.table.columns, None, theme::bold())), header);
        let lines: Vec<Line> = self
            .table
            .rows
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(height)
            .map(|(i, row)| {
                let line = layout.line(row, layout.bar(row), Style::new());
                if i == self.row { line.patch_style(theme::selected()) } else { line }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), body);
    }
}

/// Which columns fit from `first`, how wide each is, and the bars of a `stats … by …` result.
struct Columns {
    shown: Vec<(usize, usize)>,
    /// The column drawn as bars after its number: column, largest value, width of the
    /// numbers, width of the bars.
    chart: Option<(usize, f64, usize, usize)>,
    hidden_left: bool,
    hidden_right: bool,
}

fn columns(table: &logs::Table, first: usize, width: usize) -> Columns {
    let full: Vec<usize> = (0..table.columns.len())
        .map(|c| table.rows.iter().map(|r| r[c].chars().count()).chain([table.columns[c].chars().count(), 1]).max().unwrap_or(1))
        .collect();
    let first = first.min(table.columns.len().saturating_sub(1));
    // `‹ ` marks columns scrolled out on the left.
    let width = if first > 0 { width.saturating_sub(2) } else { width };
    let mut shown: Vec<(usize, usize)> = Vec::new();
    let mut used = 0;
    for (c, &w) in full.iter().enumerate().skip(first) {
        let w = w.min(COLUMN_MAX);
        let gap = if shown.is_empty() { 0 } else { 2 };
        if used + gap + w.min(12) > width && !shown.is_empty() {
            break;
        }
        shown.push((c, w));
        used += gap + w;
    }
    // Room left goes to the columns cut short, @message first; the last column shown takes
    // the rest (or is cut to fit).
    let mut room = width.saturating_sub(used);
    let mut order: Vec<usize> = (0..shown.len()).collect();
    order.sort_by_key(|&i| table.columns[shown[i].0] != "@message");
    for i in order {
        let grow = room.min(full[shown[i].0] - shown[i].1);
        shown[i].1 += grow;
        room -= grow;
    }
    let before = shown_width(&shown) - shown.last().map_or(0, |(_, w)| *w);
    if let Some(last) = shown.last_mut() {
        last.1 = width.saturating_sub(before).max(1);
    }
    let chart = table.chart_column().and_then(|c| {
        let (_, w) = shown.iter().find(|(col, _)| *col == c)?;
        let max = table.rows.iter().filter_map(|r| r[c].trim().parse::<f64>().ok()).fold(0.0, f64::max);
        let number = table.rows.iter().map(|r| r[c].trim().chars().count()).max().unwrap_or(1);
        (*w > number + 4).then_some((c, max, number, w - number - 1))
    });
    Columns {
        hidden_left: first > 0,
        hidden_right: shown.last().is_some_and(|(c, _)| *c + 1 < table.columns.len()),
        shown,
        chart,
    }
}

/// Columns and the gaps between them.
fn shown_width(shown: &[(usize, usize)]) -> usize {
    shown.iter().map(|(_, w)| w).sum::<usize>() + 2 * shown.len().saturating_sub(1)
}

impl Columns {
    fn bar(&self, row: &[String]) -> Option<String> {
        let (c, max, _, width) = self.chart?;
        let value = row.get(c)?.trim().parse::<f64>().ok()?;
        let eighths = if max > 0.0 { (value / max * width as f64 * 8.0).round() as usize } else { 0 };
        let mut bar = "█".repeat(eighths / 8);
        if eighths % 8 > 0 {
            bar.push(['▏', '▎', '▍', '▌', '▋', '▊', '▉'][eighths % 8 - 1]);
        }
        Some(bar)
    }

    fn line(&self, values: &[String], bar: Option<String>, style: Style) -> Line<'static> {
        let mut spans = Vec::new();
        if self.hidden_left {
            spans.push(Span::styled("‹ ", theme::muted()));
        }
        for (i, &(c, w)) in self.shown.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            let value = values.get(c).map(String::as_str).unwrap_or_default();
            match (&bar, self.chart) {
                (Some(bar), Some((chart, _, number, _))) if chart == c => {
                    spans.push(Span::styled(format!("{:>number$} ", value.trim()), style));
                    spans.push(Span::styled(ellipsize(bar, w.saturating_sub(number + 1)), theme::accent()));
                }
                _ => {
                    // The last column is not padded, unless `›` must show at the edge.
                    let last = i + 1 == self.shown.len();
                    let w = if last && self.hidden_right { w.saturating_sub(2) } else { w };
                    let text = ellipsize(value, w);
                    spans.push(Span::styled(if last && !self.hidden_right { text } else { format!("{text:<w$}") }, style));
                }
            }
        }
        if self.hidden_right {
            spans.push(Span::styled(" ›", theme::muted()));
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn edits_multi_line_queries() {
        let mut e = Editor::new("fields @message");
        e.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        for c in "| limit 5".chars() {
            e.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(e.text(), "fields @message\n| limit 5");
        assert!(matches!(e.handle(key(KeyCode::Enter)), Edit::Run));
        e.handle(key(KeyCode::Home));
        e.handle(key(KeyCode::Backspace));
        assert_eq!(e.text(), "fields @message| limit 5");
        e.insert("\n| sort @timestamp desc\r\n");
        assert_eq!(e.lines.len(), 3);
        assert_eq!((e.row, e.col), (2, 0));
        e.handle(key(KeyCode::Up));
        e.handle(key(KeyCode::End));
        assert_eq!(e.col, "| sort @timestamp desc".chars().count());
    }

    #[test]
    fn highlights_commands() {
        let line = highlight("fields @m | filter x | stats count(*)", 0, 80);
        let accented: Vec<&str> = line.spans.iter().filter(|s| s.style == theme::accent().bold()).map(|s| s.content.as_ref()).collect();
        assert_eq!(accented, ["fields", "filter", "stats"]);
    }

    #[test]
    fn fits_columns_and_draws_bars() {
        let table = logs::Table {
            columns: vec!["bin(5m)".into(), "count(*)".into()],
            rows: vec![vec!["2026-10-21 10:00:00.000".into(), "8".into()], vec!["2026-10-21 10:05:00.000".into(), "2".into()]],
            ptrs: vec![None, None],
        };
        let layout = columns(&table, 0, 60);
        // The count column takes the rest of the line: its number, then a 33-cell bar.
        assert_eq!(layout.shown, [(0, 23), (1, 35)]);
        assert_eq!(layout.bar(&table.rows[0]).map(|b| b.chars().count()), Some(33));
        assert_eq!(layout.bar(&table.rows[1]).as_deref(), Some("████████▎"));
        let scrolled = columns(&table, 1, 60);
        assert!(scrolled.hidden_left && !scrolled.hidden_right);
        assert_eq!(scrolled.shown, [(1, 58)]);
    }
}

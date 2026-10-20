//! DynamoDB explorer: tables with their keys and indexes; items read by key (Query, or
//! GetItem for a whole primary key) or by Scan, a page at a time, shown as plain JSON.
//! Read-only.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use serde_json::Value;

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::dynamodb::{self as ddb, KeyQuery, Page, SortCondition, TableDescription};
use crate::aws::{self, AwsError};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, fmt_bytes, fmt_utc};

/// Items per page, for Query and Scan.
const PAGE: u32 = 50;
const DEBOUNCE: Duration = Duration::from_millis(200);
/// The sort key choice of the query form: no condition, then [`SORT_OPERATORS`].
const SORT_CHOICES: [&str; 8] = ["any", "=", "begins_with", "between", "<", "<=", ">", ">="];

enum Msg {
    Tables(Vec<String>),
    TablesDone,
    TablesFailed(AwsError),
    Described { name: String, result: Box<aws::Result<TableDescription>> },
    /// A page of the open table; `seq` drops pages of a query or scan left meanwhile.
    Page { seq: u64, result: aws::Result<Page> },
}

enum Mode {
    /// Opened for a query: nothing read yet.
    Idle,
    Scan,
    Query(KeyQuery),
}

struct Open {
    desc: TableDescription,
    mode: Mode,
    seq: u64,
    items: Vec<Value>,
    next: Option<Value>,
    pages: usize,
    scanned: i64,
    loading: bool,
    error: Option<String>,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    list_state: ListState,
    detail_scroll: u16,
    zoom: Option<LogView>,
    last_query: Option<KeyQuery>,
}

impl Open {
    fn new(desc: TableDescription) -> Open {
        Open {
            desc,
            mode: Mode::Idle,
            seq: 0,
            items: Vec::new(),
            next: None,
            pages: 0,
            scanned: 0,
            loading: false,
            error: None,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            detail_scroll: 0,
            zoom: None,
            last_query: None,
        }
    }

    fn refilter(&mut self) {
        let query = self.filter.value.trim().to_lowercase();
        let keep = self.selected_index();
        self.visible = (0..self.items.len())
            .filter(|&i| query.is_empty() || matches_filter(&self.items[i].to_string(), &query))
            .collect();
        let pos = keep.and_then(|k| self.visible.iter().position(|&i| i == k)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(pos) });
    }

    fn selected_index(&self) -> Option<usize> {
        self.visible.get(self.list_state.selected()?).copied()
    }

    fn selected(&self) -> Option<&Value> {
        self.items.get(self.selected_index()?)
    }

    fn select(&mut self, pos: usize) {
        if !self.visible.is_empty() {
            let pos = pos.min(self.visible.len() - 1);
            if self.list_state.selected() != Some(pos) {
                self.detail_scroll = 0;
            }
            self.list_state.select(Some(pos));
        }
    }

    /// Key columns first (the table's, then the queried index's), then the most common
    /// other attributes.
    fn columns(&self) -> Vec<String> {
        let mut cols: Vec<String> = Vec::new();
        let index = match &self.mode {
            Mode::Query(q) => q.index.clone(),
            _ => None,
        };
        for keys in [self.desc.keys(None), index.as_deref().and_then(|i| self.desc.keys(Some(i)))].into_iter().flatten() {
            for name in std::iter::once(keys.partition).chain(keys.sort) {
                if !cols.contains(&name) {
                    cols.push(name);
                }
            }
        }
        // (count, first seen) per attribute: the most common first, then in item order.
        let mut seen: HashMap<&str, (usize, usize)> = HashMap::new();
        for item in &self.items {
            if let Some(m) = item.as_object() {
                for k in m.keys().filter(|k| !cols.contains(k)) {
                    let next = seen.len();
                    seen.entry(k.as_str()).or_insert((0, next)).0 += 1;
                }
            }
        }
        let mut others: Vec<(&str, (usize, usize))> = seen.into_iter().collect();
        others.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.1.1.cmp(&b.1.1)));
        cols.extend(others.into_iter().map(|(k, _)| k.to_string()));
        cols
    }

    /// What is being read, for the header line.
    fn describe_mode(&self) -> String {
        match &self.mode {
            Mode::Idle => "Nothing read yet · f finds items by key · s scans".into(),
            Mode::Scan => "Scan · reads the whole table, 50 items per page".into(),
            Mode::Query(q) if q.is_get(&self.desc) => format!("GetItem · {}", q.describe(&self.desc)),
            Mode::Query(q) => format!("Query · {}", q.describe(&self.desc)),
        }
    }
}

enum Dialog {
    /// Which key to query: the table's (`None`) or an index's.
    Index(Picker, Vec<Option<String>>),
    Query { form: Form, index: Option<String> },
}

pub struct DynamoDb {
    tables: Vec<String>,
    load: LoadState,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    list_state: ListState,
    descriptions: HashMap<String, aws::Result<TableDescription>>,
    describing: HashSet<String>,
    pending: Option<(String, Instant)>,
    /// A table to open once its description arrives (`true`: for a query, without scanning).
    opening: Option<(String, bool)>,
    open: Option<Open>,
    dialog: Option<Dialog>,
    seq: u64,
    region: String,
    list_area: Rect,
    detail_area: Rect,
}

impl DynamoDb {
    pub fn new() -> DynamoDb {
        DynamoDb {
            tables: Vec::new(),
            load: LoadState::Idle,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            descriptions: HashMap::new(),
            describing: HashSet::new(),
            pending: None,
            opening: None,
            open: None,
            dialog: None,
            seq: 0,
            region: String::new(),
            list_area: Rect::default(),
            detail_area: Rect::default(),
        }
    }

    fn load(&mut self, ctx: &mut Ctx) {
        self.tables.clear();
        self.visible.clear();
        self.descriptions.clear();
        self.load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let mut start: Option<String> = None;
            loop {
                match ddb::list_tables(&aws, start.as_deref()) {
                    Ok((page, next)) => {
                        if !em.emit(Msg::Tables(page)) {
                            return;
                        }
                        match next {
                            Some(t) => start = Some(t),
                            None => break,
                        }
                    }
                    Err(e) => {
                        em.emit(Msg::TablesFailed(e));
                        return;
                    }
                }
            }
            em.emit(Msg::TablesDone);
        });
    }

    fn refilter(&mut self) {
        let keep = self.selected_table().map(str::to_string);
        let query = self.filter.value.trim().to_lowercase();
        self.visible = (0..self.tables.len()).filter(|&i| matches_filter(&self.tables[i], &query)).collect();
        let pos = keep.and_then(|k| self.visible.iter().position(|&i| self.tables[i] == k)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(pos) });
        self.queue_describe();
    }

    fn selected_table(&self) -> Option<&str> {
        self.tables.get(*self.visible.get(self.list_state.selected()?)?).map(String::as_str)
    }

    fn select(&mut self, pos: usize) {
        if !self.visible.is_empty() {
            self.list_state.select(Some(pos.min(self.visible.len() - 1)));
            self.queue_describe();
        }
    }

    /// Describes the selected table after a short pause (scrolling does not fire calls).
    fn queue_describe(&mut self) {
        if let Some(name) = self.selected_table()
            && !self.descriptions.contains_key(name)
        {
            self.pending = Some((name.to_string(), Instant::now()));
        }
    }

    fn describe(&mut self, name: String, ctx: &mut Ctx) {
        if !self.describing.insert(name.clone()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = Box::new(ddb::describe_table(&aws, &name));
            Msg::Described { name, result }
        });
    }

    /// Opens the selected table: scans its first page, or (`for_query`) asks for a key.
    fn open_selected(&mut self, for_query: bool, ctx: &mut Ctx) {
        let Some(name) = self.selected_table().map(str::to_string) else { return };
        match self.descriptions.get(&name) {
            Some(Ok(desc)) => {
                let desc = desc.clone();
                self.open_table(desc, for_query, ctx);
            }
            Some(Err(e)) => ctx.toast(Level::Error, e.short()),
            None => {
                self.opening = Some((name.clone(), for_query));
                self.describe(name, ctx);
            }
        }
    }

    fn open_table(&mut self, desc: TableDescription, for_query: bool, ctx: &mut Ctx) {
        self.open = Some(Open::new(desc));
        if for_query {
            self.ask_query();
        } else {
            self.start(Mode::Scan, ctx);
        }
    }

    /// Starts reading with `mode` from the first page.
    fn start(&mut self, mode: Mode, ctx: &mut Ctx) {
        let Some(open) = &mut self.open else { return };
        self.seq += 1;
        if let Mode::Query(q) = &mode {
            open.last_query = Some(q.clone());
        }
        open.mode = mode;
        open.seq = self.seq;
        open.items.clear();
        open.visible.clear();
        open.list_state.select(None);
        open.next = None;
        open.pages = 0;
        open.scanned = 0;
        open.error = None;
        open.detail_scroll = 0;
        self.fetch(None, ctx);
    }

    /// Reads the next page of the current mode (from `start`).
    fn fetch(&mut self, start: Option<Value>, ctx: &mut Ctx) {
        let Some(open) = &mut self.open else { return };
        let query = match &open.mode {
            Mode::Idle => return,
            Mode::Scan => None,
            Mode::Query(q) => Some(q.clone()),
        };
        open.loading = true;
        let (aws, desc, seq) = (ctx.env().aws.clone(), open.desc.clone(), open.seq);
        ctx.spawn(move || {
            let result = match &query {
                Some(q) => ddb::query(&aws, &desc, q, PAGE, start.as_ref()),
                None => ddb::scan(&aws, &desc.table_name, PAGE, start.as_ref()),
            };
            Msg::Page { seq, result }
        });
    }

    fn more(&mut self, ctx: &mut Ctx) {
        let Some(open) = &self.open else { return };
        if open.loading {
            return;
        }
        match open.next.clone() {
            Some(next) => self.fetch(Some(next), ctx),
            None => ctx.toast(Level::Info, "No more items"),
        }
    }

    /// `f`: pick the table or an index (when there are indexes), then the key form.
    fn ask_query(&mut self) {
        let Some(open) = &self.open else { return };
        let desc = &open.desc;
        if desc.indexes().next().is_none() {
            self.query_form(None);
            return;
        }
        let mut items = vec![Item::new(format!("{} (table)", desc.table_name), desc.key_label(None))];
        let mut choices = vec![None];
        for (kind, index) in desc.indexes() {
            let projection = index.projection.as_ref().and_then(|p| p.projection_type.clone()).unwrap_or_default();
            items.push(Item::new(format!("{} ({kind})", index.index_name), format!("{} · {projection}", desc.key_label(Some(&index.index_name)))));
            choices.push(Some(index.index_name.clone()));
        }
        let current = match &open.last_query {
            Some(q) => choices.iter().position(|c| *c == q.index).unwrap_or(0),
            None => 0,
        };
        self.dialog = Some(Dialog::Index(Picker::new("Query by the key of", "choose", items, false).with_cursor(current), choices));
    }

    fn query_form(&mut self, index: Option<String>) {
        let Some(open) = &self.open else { return };
        let desc = &open.desc;
        let Some(keys) = desc.keys(index.as_deref()) else { return };
        // Start from the last query on the same key.
        let last = open.last_query.as_ref().filter(|q| q.index == index);
        let partition = last.map(|q| q.partition.clone()).unwrap_or_default();
        let (op, v1, v2) = match last.and_then(|q| q.sort.as_ref()) {
            Some(SortCondition::Between(a, b)) => ("between", a.clone(), b.clone()),
            Some(SortCondition::Eq(v)) => ("=", v.clone(), String::new()),
            Some(SortCondition::BeginsWith(v)) => ("begins_with", v.clone(), String::new()),
            Some(SortCondition::Lt(v)) => ("<", v.clone(), String::new()),
            Some(SortCondition::Le(v)) => ("<=", v.clone(), String::new()),
            Some(SortCondition::Gt(v)) => (">", v.clone(), String::new()),
            Some(SortCondition::Ge(v)) => (">=", v.clone(), String::new()),
            None => ("any", String::new(), String::new()),
        };
        let target = match &index {
            Some(i) => format!("index {i}"),
            None => format!("table {}", desc.table_name),
        };
        let mut fields = vec![
            Field::fixed("Key of", target),
            Field::text("Partition key", partition).hint("value of the partition key"),
        ];
        let mut note = format!("Partition key: {} ({})", keys.partition, desc.attr_type(&keys.partition));
        if let Some(sort) = &keys.sort {
            fields.push(Field::choice("Sort key", &SORT_CHOICES, op));
            fields.push(Field::text("Value", v1).optional("sort key value"));
            fields.push(Field::text("And", v2).optional("second value, for between"));
            note.push_str(&format!(" · sort key: {sort} ({})", desc.attr_type(sort)));
        }
        if index.is_none() {
            note.push_str(if keys.sort.is_some() {
                ". With = on the sort key it reads that one item (GetItem)."
            } else {
                ". The partition key is the whole key: it reads that one item (GetItem)."
            });
        }
        let mut form = Form::new(format!("Query · {}", desc.table_name), fields);
        form.note = Some(note);
        self.dialog = Some(Dialog::Query { form, index });
    }

    fn submit_query(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Query { form, index }) = &mut self.dialog else { return };
        let Some(open) = &self.open else { return };
        let desc = &open.desc;
        let Some(keys) = desc.keys(index.as_deref()) else { return };
        let partition = form.get("Partition key").to_string();
        if let Err(e) = ddb::typed(&partition, desc.attr_type(&keys.partition)) {
            return form.fail(format!("Partition key: {e}"));
        }
        let mut sort = None;
        if let Some(sort_name) = &keys.sort {
            let op = form.get("Sort key");
            let (v1, v2) = (form.get("Value").to_string(), form.get("And").to_string());
            if op != "any" {
                if v1.is_empty() || (op == "between" && v2.is_empty()) {
                    return form.fail(if op == "between" { "between needs both values" } else { "Value is required for that condition" });
                }
                let kind = desc.attr_type(sort_name);
                for v in [&v1, &v2].into_iter().filter(|v| !v.is_empty()) {
                    if let Err(e) = ddb::typed(v, kind) {
                        return form.fail(format!("Sort key: {e}"));
                    }
                }
                match SortCondition::new(op, &v1, &v2) {
                    Ok(c) => sort = Some(c),
                    Err(e) => return form.fail(e),
                }
            }
        }
        let query = KeyQuery { index: index.clone(), partition, sort };
        self.dialog = None;
        self.start(Mode::Query(query), ctx);
    }

    fn copy_item(&self, ctx: &mut Ctx) {
        let Some(item) = self.open.as_ref().and_then(Open::selected) else { return };
        let text = serde_json::to_string_pretty(item).unwrap_or_default();
        let ok = crate::platform::copy_to_clipboard(&text);
        ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Item copied as JSON" } else { "Clipboard not available" });
    }

    fn console(&self, ctx: &mut Ctx) {
        let table = match &self.open {
            Some(o) => o.desc.table_name.clone(),
            None => match self.selected_table() {
                Some(t) => t.to_string(),
                None => return,
            },
        };
        let url = aws::console_url(&self.region, &format!("dynamodbv2/home?region={}#table?name={table}", self.region));
        if !crate::platform::open_url(&url) {
            ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
        }
    }

    fn zoomed(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.zoom.as_ref().is_some_and(|z| z.zoomed))
    }

    // ---- rendering ----------------------------------------------------------------

    fn render_tables(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search tables");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = match &self.load {
            LoadState::Loading => format!(" {} Tables · {} ", theme::spinner(view.tick), self.tables.len()),
            _ => format!(" Tables · {} ", self.visible.len()),
        };
        if self.visible.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading tables…", theme::muted()),
                _ => Line::styled("No tables", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|&i| {
                let name = &self.tables[i];
                let info = match self.descriptions.get(name) {
                    Some(Ok(d)) => d.item_count.map(|n| format!("{} items", thousands(n))).unwrap_or_default(),
                    _ => String::new(),
                };
                let label = ellipsize(name, width.saturating_sub(info.len() + 2));
                let pad = width.saturating_sub(label.chars().count() + info.len());
                ListItem::new(Line::from(vec![Span::raw(label), Span::raw(" ".repeat(pad)), Span::styled(info, theme::muted())]))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_table_info(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(name) = self.selected_table() else {
            f.render_widget(widgets::placeholder(Line::styled("Select a table", theme::muted())).block(widgets::panel(" Table ", false)), area);
            return;
        };
        let block = widgets::panel(Line::from(Span::styled(format!(" {name} "), theme::bold())), false);
        let desc = match self.descriptions.get(name) {
            Some(Ok(d)) => d,
            Some(Err(e)) => {
                f.render_widget(widgets::placeholder(Line::styled(e.short(), Style::new().fg(theme::ERR))).block(block), area);
                return;
            }
            None => {
                let text = Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" loading…")]);
                f.render_widget(Paragraph::new(text).block(block), area);
                return;
            }
        };
        let kw = 12;
        let mut lines = vec![
            widgets::kv("Keys", Span::styled(desc.key_label(None), theme::bold()), kw),
            widgets::kv(
                "Items",
                desc.item_count.map(|n| format!("{} (DynamoDB updates this about every 6 hours)", thousands(n))).unwrap_or_else(|| "—".into()),
                kw,
            ),
            widgets::kv("Size", desc.table_size_bytes.map(fmt_bytes).unwrap_or_else(|| "—".into()), kw),
            widgets::kv("Billing", desc.billing(), kw),
            widgets::kv("Status", desc.table_status.clone().unwrap_or_default(), kw),
        ];
        let stream = match &desc.stream_specification {
            Some(s) if s.stream_enabled == Some(true) => s.stream_view_type.clone().unwrap_or_else(|| "on".into()),
            _ => "off".into(),
        };
        lines.push(widgets::kv("Stream", stream, kw));
        lines.push(widgets::kv(
            "Protection",
            if desc.deletion_protection_enabled == Some(true) { "deletion protection on" } else { "deletion protection off" },
            kw,
        ));
        if let Some(t) = desc.creation_date_time {
            lines.push(widgets::kv("Created", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
        }
        if desc.indexes().next().is_some() {
            lines.push(Line::raw(""));
            lines.push(widgets::section("Indexes"));
            for (kind, i) in desc.indexes() {
                let projection = i.projection.as_ref().and_then(|p| p.projection_type.clone()).unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(format!("{kind} "), theme::muted()),
                    Span::styled(i.index_name.clone(), theme::bold()),
                    Span::raw(format!("  {}", desc.key_label(Some(&i.index_name)))),
                    Span::styled(format!("  · {projection}"), theme::muted()),
                ]));
            }
        }
        lines.push(Line::raw(""));
        lines.push(widgets::hints_line(&[("Enter", "open (scans the first 50 items)"), ("f", "find by key")]));
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block), area);
    }

    fn render_items(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let Some(open) = &mut self.open else { return };
        let [head, bar, body] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Fill(1)]));

        let status = if open.loading {
            format!("{} reading…", theme::spinner(view.tick))
        } else if let Some(e) = &open.error {
            e.clone()
        } else if matches!(open.mode, Mode::Idle) {
            String::new()
        } else {
            let pages = if open.pages == 1 { "1 page".to_string() } else { format!("{} pages", open.pages) };
            let more = if open.next.is_some() { " · m loads more" } else { " · all read" };
            let items = if open.items.len() == 1 { "1 item".to_string() } else { format!("{} items", thousands(open.items.len() as i64)) };
            format!("{items} · {pages} · {} read{more}", thousands(open.scanned))
        };
        let status_style = if open.error.is_some() { Style::new().fg(theme::ERR) } else { theme::muted() };
        let line = Line::from(vec![
            Span::styled(format!(" {} ", open.desc.table_name), theme::accent().add_modifier(Modifier::BOLD)),
            Span::raw(open.describe_mode()),
            Span::styled(format!("   {status}"), status_style),
        ]);
        f.render_widget(Paragraph::new(line), head);
        let (bar_line, cursor) = filter_bar(&open.filter, open.filtering, "type / to filter the items read");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }

        let [list_area, detail_area] = if body.width < 110 {
            body.layout(&Layout::vertical([Constraint::Percentage(50), Constraint::Fill(1)]))
        } else {
            body.layout(&Layout::horizontal([Constraint::Percentage(55), Constraint::Fill(1)]))
        };
        self.list_area = list_area;
        self.detail_area = detail_area;

        // Columns that fit: keys first, then common attributes.
        let inner = list_area.width.saturating_sub(4) as usize;
        let columns = open.columns();
        let mut shown: Vec<(String, usize)> = Vec::new();
        let mut used = 0;
        for (i, name) in columns.iter().enumerate() {
            let longest = open
                .visible
                .iter()
                .filter_map(|&r| open.items[r].get(name))
                .map(|v| ddb::cell(v).chars().count())
                .chain([name.chars().count()])
                .max()
                .unwrap_or(4);
            let width = longest.clamp(4, if i < 2 { 28 } else { 20 }) + 2;
            if used + width > inner && !shown.is_empty() {
                break;
            }
            used += width;
            shown.push((name.clone(), width));
        }
        let header: Vec<Span> = shown.iter().map(|(n, w)| Span::styled(format!("{:<w$}", ellipsize(n, w - 2)), theme::muted().add_modifier(Modifier::BOLD))).collect();
        let more_cols = columns.len().saturating_sub(shown.len());
        let title = if more_cols > 0 { format!(" Items · {} · +{more_cols} attributes in the detail ", open.visible.len()) } else { format!(" Items · {} ", open.visible.len()) };
        let block = widgets::panel(title, open.zoom.is_none() && self.dialog.is_none());
        let inner_area = block.inner(list_area);
        f.render_widget(block, list_area);
        let [header_area, rows_area] = inner_area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(Line::from(header)), header_area);

        if open.visible.is_empty() {
            let text = match (&open.mode, open.loading) {
                (_, true) => "Reading…",
                (Mode::Idle, _) => "f finds items by key · s scans the table",
                _ if !open.items.is_empty() => "No item matches the filter",
                _ => "No items",
            };
            f.render_widget(widgets::placeholder(Line::styled(text, theme::muted())), rows_area);
        } else {
            let items: Vec<ListItem> = open
                .visible
                .iter()
                .map(|&r| {
                    let item = &open.items[r];
                    let spans: Vec<Span> = shown
                        .iter()
                        .enumerate()
                        .map(|(i, (name, w))| match item.get(name) {
                            Some(v) => Span::styled(
                                format!("{:<w$}", ellipsize(&ddb::cell(v), w - 2)),
                                if i == 0 { theme::bold() } else { Style::new() },
                            ),
                            None => Span::styled(format!("{:<w$}", "—"), theme::muted()),
                        })
                        .collect();
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let list = List::new(items).highlight_style(theme::selected());
            f.render_stateful_widget(list, rows_area, &mut open.list_state);
        }

        let detail_block = widgets::panel(" Item ", false);
        match open.selected() {
            Some(item) => {
                let text = serde_json::to_string_pretty(item).unwrap_or_default();
                let lines: Vec<Line> = text.lines().map(json_line).collect();
                f.render_widget(Paragraph::new(lines).scroll((open.detail_scroll, 0)).block(detail_block), detail_area);
            }
            None => f.render_widget(widgets::placeholder(Line::styled("Select an item", theme::muted())).block(detail_block), detail_area),
        }
    }
}

/// A line of pretty JSON with attribute names highlighted.
fn json_line(line: &str) -> Line<'static> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix('"')
        && let Some(end) = rest.find("\": ")
    {
        let indent = &line[..line.len() - trimmed.len()];
        return Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled(format!("\"{}\"", &rest[..end]), theme::accent()),
            Span::raw(rest[end + 1..].to_string()),
        ]);
    }
    Line::raw(line.to_string())
}

/// `12,345`
fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

impl Module for DynamoDb {
    fn title(&self) -> &'static str {
        "DynamoDB"
    }

    fn description(&self) -> &'static str {
        "DynamoDB · tables, query by key, scan, items as JSON"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = DynamoDb::new();
        self.region = ctx.env().region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        // Dialogs first: the index picker, then the query form.
        if let Some(dialog) = &mut self.dialog {
            match dialog {
                Dialog::Index(picker, choices) => match picker.handle(key) {
                    Outcome::Pending => {}
                    Outcome::Cancel => self.dialog = None,
                    Outcome::Done(chosen) => {
                        let index = chosen.first().and_then(|&i| choices.get(i).cloned()).flatten();
                        self.dialog = None;
                        self.query_form(index);
                    }
                },
                Dialog::Query { form, .. } => match form.handle(key) {
                    FormEvent::Pending => {}
                    FormEvent::Cancel => self.dialog = None,
                    FormEvent::Submit => self.submit_query(ctx),
                },
            }
            return Handled::Yes;
        }
        if let Some(open) = &mut self.open {
            if let Some(zoom) = open.zoom.as_mut().filter(|z| z.zoomed) {
                if let LogKey::Copy = zoom.key(key) {
                    let ok = crate::platform::copy_to_clipboard(&zoom.lines().join("\n"));
                    ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Item copied as JSON" } else { "Clipboard not available" });
                }
                if !zoom.zoomed {
                    open.zoom = None;
                }
                return Handled::Yes;
            }
            let selected = open.list_state.selected().unwrap_or(0);
            if open.filtering {
                match open.filter.handle(key) {
                    InputEvent::Changed => open.refilter(),
                    InputEvent::Submit => open.filtering = false,
                    InputEvent::Cancel => {
                        open.filtering = false;
                        open.filter.clear();
                        open.refilter();
                    }
                    InputEvent::Ignored => match key.code {
                        KeyCode::Up => open.select(selected.saturating_sub(1)),
                        KeyCode::Down => open.select(selected + 1),
                        _ => {}
                    },
                    InputEvent::Moved => {}
                }
                return Handled::Yes;
            }
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => open.select(selected.saturating_sub(1)),
                KeyCode::Down | KeyCode::Char('j') => open.select(selected + 1),
                KeyCode::PageUp => open.detail_scroll = open.detail_scroll.saturating_sub(10),
                KeyCode::PageDown => open.detail_scroll = open.detail_scroll.saturating_add(10),
                KeyCode::Char('/') => open.filtering = true,
                KeyCode::Esc if !open.filter.value.is_empty() => {
                    open.filter.clear();
                    open.refilter();
                }
                KeyCode::Esc => self.open = None,
                KeyCode::Char('f') => self.ask_query(),
                KeyCode::Char('s') => self.start(Mode::Scan, ctx),
                KeyCode::Char('m') => self.more(ctx),
                KeyCode::Char('r') => {
                    let mode = match &open.mode {
                        Mode::Query(q) => Mode::Query(q.clone()),
                        Mode::Scan => Mode::Scan,
                        Mode::Idle => return Handled::Yes,
                    };
                    self.start(mode, ctx);
                }
                KeyCode::Char('y') => self.copy_item(ctx),
                KeyCode::Char('z') => {
                    if let Some(item) = open.selected() {
                        let mut zoom = LogView::default();
                        zoom.extend(serde_json::to_string_pretty(item).unwrap_or_default().lines().map(str::to_string));
                        zoom.scroll(-(zoom.lines().len() as isize));
                        zoom.zoomed = true;
                        open.zoom = Some(zoom);
                    }
                }
                KeyCode::Char('o') => self.console(ctx),
                _ => return Handled::No,
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
            KeyCode::Enter => self.open_selected(false, ctx),
            KeyCode::Char('f') => self.open_selected(true, ctx),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Char('o') => self.console(ctx),
            KeyCode::Char('r') => self.load(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() {
            return Handled::No;
        }
        let pos = (event.column, event.row).into();
        if let Some(open) = &mut self.open {
            let selected = open.list_state.selected().unwrap_or(0);
            match event.kind {
                MouseEventKind::ScrollUp if self.detail_area.contains(pos) => open.detail_scroll = open.detail_scroll.saturating_sub(3),
                MouseEventKind::ScrollDown if self.detail_area.contains(pos) => open.detail_scroll = open.detail_scroll.saturating_add(3),
                MouseEventKind::ScrollUp if self.list_area.contains(pos) => open.select(selected.saturating_sub(1)),
                MouseEventKind::ScrollDown if self.list_area.contains(pos) => open.select(selected + 1),
                MouseEventKind::Down(MouseButton::Left) if self.list_area.contains(pos) => {
                    // Border and the column header row.
                    let row = event.row.saturating_sub(self.list_area.y + 2) as usize;
                    open.select(open.list_state.offset() + row);
                }
                _ => return Handled::No,
            }
            return Handled::Yes;
        }
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

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Tables(page) => {
                let first = self.tables.is_empty();
                self.tables.extend(page);
                self.refilter();
                if first {
                    self.select(0);
                }
            }
            Msg::TablesDone => self.load = LoadState::Loaded,
            Msg::TablesFailed(e) => self.load = LoadState::Failed(e),
            Msg::Described { name, result } => {
                let result = *result;
                self.describing.remove(&name);
                let opening = self.opening.as_ref().filter(|(n, _)| *n == name).map(|(_, q)| *q);
                if let Some(for_query) = opening {
                    self.opening = None;
                    match &result {
                        Ok(desc) => {
                            let desc = desc.clone();
                            self.descriptions.insert(name, result);
                            self.open_table(desc, for_query, ctx);
                            return;
                        }
                        Err(e) => ctx.toast(Level::Error, e.short()),
                    }
                }
                self.descriptions.insert(name, result);
            }
            Msg::Page { seq, result } => {
                let Some(open) = self.open.as_mut().filter(|o| o.seq == seq) else { return };
                open.loading = false;
                match result {
                    Ok(page) => {
                        let first = open.items.is_empty();
                        open.items.extend(page.items);
                        open.next = page.next;
                        open.pages += 1;
                        open.scanned += page.scanned;
                        open.refilter();
                        if first {
                            open.select(0);
                        }
                    }
                    Err(e) => open.error = Some(e.short()),
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
            if self.selected_table() == Some(name.as_str()) && !self.descriptions.contains_key(&name) {
                self.describe(name, ctx);
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some(open) = &mut self.open {
            if let Some(zoom) = open.zoom.as_mut().filter(|z| z.zoomed) {
                zoom.render_zoomed(f, area, &format!("{} · item", open.desc.table_name));
                return;
            }
            self.render_items(f, area, view);
        } else {
            let [left, right] = if area.width < 90 {
                area.layout(&Layout::vertical([Constraint::Percentage(45), Constraint::Fill(1)]))
            } else {
                area.layout(&Layout::horizontal([Constraint::Percentage(35), Constraint::Fill(1)]))
            };
            self.render_tables(f, left, view);
            self.render_table_info(f, right, view);
        }
        match &mut self.dialog {
            Some(Dialog::Index(picker, _)) => {
                let rect = widgets::popup_area(area, 84, picker.height(area.height.saturating_sub(4)));
                picker.render(f, rect, theme::ACCENT);
            }
            Some(Dialog::Query { form, .. }) => form.render(f, area, theme::ACCENT),
            None => {}
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.dialog {
            Some(Dialog::Index(..)) => return vec![("↑↓", "move"), ("Enter", "choose"), ("Esc", "cancel")],
            Some(Dialog::Query { .. }) => return vec![("Tab", "next field"), ("←→", "condition"), ("Enter", "run"), ("Esc", "cancel")],
            None => {}
        }
        if let Some(open) = &self.open {
            if self.zoomed() {
                return vec![("/", "search"), ("y", "copy"), ("Esc", "back")];
            }
            if open.filtering {
                return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
            }
            let mut h = vec![("↑↓", "item"), ("f", "find by key"), ("s", "scan")];
            if open.next.is_some() {
                h.push(("m", "more"));
            }
            h.extend([("/", "filter"), ("y", "copy"), ("z", "full screen"), ("o", "console"), ("Esc", "tables")]);
            return h;
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        vec![("↑↓", "move"), ("Enter", "open"), ("f", "find by key"), ("/", "search"), ("o", "console"), ("r", "reload")]
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || self.pending.is_some()
            || !self.describing.is_empty()
            || self.open.as_ref().is_some_and(|o| o.loading)
    }

    fn capturing(&self) -> bool {
        self.dialog.is_some()
            || self.filtering
            || self.open.as_ref().is_some_and(|o| o.filtering || o.zoom.is_some())
    }

    fn wants_mouse(&self) -> bool {
        !self.zoomed()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.dialog {
            Some(Dialog::Index(picker, _)) => picker.paste(text),
            Some(Dialog::Query { form, .. }) => form.paste(text),
            None => {
                if let Some(open) = self.open.as_mut().filter(|o| o.filtering) {
                    open.filter.insert_str(text, false);
                    open.refilter();
                } else if self.filtering {
                    self.filter.insert_str(text, false);
                    self.refilter();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1234), "1,234");
        assert_eq!(thousands(-1_234_567), "-1,234,567");
    }

    #[test]
    fn sort_choices_follow_the_operators() {
        assert_eq!(&SORT_CHOICES[1..], &crate::aws::dynamodb::SORT_OPERATORS[..]);
    }

    #[test]
    fn highlights_json_keys() {
        let line = json_line(r#"  "name": "Ana","#);
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].content, "\"name\"");
        assert_eq!(line.spans[2].content, ": \"Ana\",");
        assert_eq!(json_line("  }").spans.len(), 1);
    }
}

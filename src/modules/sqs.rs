//! SQS: queues with their depth (visible, in flight, delayed) and dead-letter
//! relationships; the messages of dead-letter queues, and redrive back to their source
//! queues. Messages are only read in dead-letter queues: a receive counts toward a queue's
//! maxReceiveCount, so looking into a normal queue could push its messages to its DLQ.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::sqs::{self, Message, MoveTask, Queue};
use crate::aws;
use crate::config::is_production;
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, fmt_bytes, fmt_duration, fmt_utc};

const DEBOUNCE: Duration = Duration::from_millis(200);
/// How often the queue counts refresh while the module is open.
const LIST_REFRESH: Duration = Duration::from_secs(30);
/// How often a running redrive is checked.
const MOVE_REFRESH: Duration = Duration::from_secs(2);

/// What is read when a queue is selected.
struct Detail {
    /// URLs of the queues that use it as their dead-letter queue.
    sources: Vec<String>,
    /// Its recent redrives, newest first (dead-letter queues).
    moves: Vec<MoveTask>,
    /// Age of its oldest message in seconds (CloudWatch), when known.
    oldest: Option<f64>,
}

enum Msg {
    Queues(aws::Result<Vec<Queue>>),
    Detail { url: String, result: aws::Result<Detail> },
    Messages { url: String, result: aws::Result<Vec<Message>> },
    /// A redrive's progress, with the dead-letter queue's fresh counts.
    Moves { url: String, result: aws::Result<(Vec<MoveTask>, Queue)> },
    Acted { url: String, result: aws::Result<String> },
}

#[derive(PartialEq)]
enum Focus {
    List,
    /// The queues related to the selected one (its DLQ, or the queues that send to it).
    Links,
}

/// The messages of a dead-letter queue.
struct Messages {
    url: String,
    name: String,
    load: LoadState,
    list: Vec<Message>,
    state: ListState,
}

#[derive(Clone)]
enum Action {
    Redrive { url: String, source_arn: String, destination_arn: Option<String>, destination: String },
    Cancel { url: String, task_handle: String },
}

enum Dialog {
    /// Where a redrive sends the messages: (picker, DLQ URL, destination ARNs by item;
    /// `None` = their source queues).
    Destination(Picker, String, Vec<Option<String>>),
    Confirm { title: String, lines: Vec<String>, typed: Option<(String, TextInput)>, action: Action, busy: bool, error: Option<String> },
}

pub struct Sqs {
    queues: Vec<Queue>,
    load: LoadState,
    listed_at: Instant,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    list_state: ListState,
    details: HashMap<String, aws::Result<Detail>>,
    in_flight: HashSet<String>,
    pending: Option<(String, Instant)>,
    focus: Focus,
    link: usize,
    messages: Option<Messages>,
    zoom: Option<(String, LogView)>,
    dialog: Option<Dialog>,
    /// The dead-letter queue whose redrive is being followed, and when it was last checked.
    watching: Option<(String, Instant)>,
    /// A jump arrived before the queues loaded.
    pending_open: Option<String>,
    env_name: String,
    region: String,
    detail_scroll: u16,
    list_area: Rect,
}

impl Sqs {
    pub fn new() -> Sqs {
        Sqs {
            queues: Vec::new(),
            load: LoadState::Idle,
            listed_at: Instant::now(),
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            details: HashMap::new(),
            in_flight: HashSet::new(),
            pending: None,
            focus: Focus::List,
            link: 0,
            messages: None,
            zoom: None,
            dialog: None,
            watching: None,
            pending_open: None,
            env_name: String::new(),
            region: String::new(),
            detail_scroll: 0,
            list_area: Rect::default(),
        }
    }

    fn load_queues(&mut self, ctx: &mut Ctx) {
        self.load = LoadState::Loading;
        self.listed_at = Instant::now();
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Queues(sqs::list_queues(&aws)));
    }

    // ---- queues and their relationships -------------------------------------------------

    fn by_arn(&self, arn: &str) -> Option<&Queue> {
        self.queues.iter().find(|q| q.arn() == arn)
    }

    /// Names of the queues that send their failures to `q`: from the redrive policies
    /// listed here, and from ListDeadLetterSourceQueues once its detail is read.
    fn sources(&self, q: &Queue) -> Vec<String> {
        let mut names: Vec<String> =
            self.queues.iter().filter(|s| s.redrive().is_some_and(|r| r.dead_letter_arn == q.arn())).map(|s| s.name.clone()).collect();
        if let Some(Ok(d)) = self.details.get(&q.url) {
            for url in &d.sources {
                let name = sqs::queue_name(url).to_string();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }

    fn is_dlq(&self, q: &Queue) -> bool {
        !self.sources(q).is_empty()
    }

    /// Dead-letter queues holding messages first (what people come for), then by name.
    fn refilter(&mut self) {
        let keep = self.selected().map(|q| q.url.clone());
        let query = self.filter.value.trim().to_lowercase();
        let mut visible: Vec<usize> = (0..self.queues.len()).filter(|&i| matches_filter(&self.queues[i].name, &query)).collect();
        visible.sort_by_key(|&i| {
            let q = &self.queues[i];
            (!(self.is_dlq(q) && q.visible() > 0), q.name.clone())
        });
        self.visible = visible;
        let index = keep.and_then(|k| self.visible.iter().position(|&i| self.queues[i].url == k)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(index) });
        self.queue_detail();
    }

    fn selected(&self) -> Option<&Queue> {
        self.queues.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn select(&mut self, index: usize) {
        if self.visible.is_empty() {
            return;
        }
        let index = index.min(self.visible.len() - 1);
        if self.list_state.selected() != Some(index) {
            self.detail_scroll = 0;
            self.link = 0;
            self.focus = Focus::List;
        }
        self.list_state.select(Some(index));
        self.queue_detail();
    }

    /// Selects the queue named `name`, clearing the filter if it hides it.
    fn select_named(&mut self, name: &str) -> bool {
        if !self.visible.iter().any(|&i| self.queues[i].name == name) {
            self.filter.clear();
            self.refilter();
        }
        match self.visible.iter().position(|&i| self.queues[i].name == name) {
            Some(pos) => {
                self.select(pos);
                true
            }
            None => false,
        }
    }

    /// Reads the selected queue's detail after a short pause (arrow-key scrolling).
    fn queue_detail(&mut self) {
        if let Some(q) = self.selected()
            && !self.details.contains_key(&q.url)
        {
            self.pending = Some((q.url.clone(), Instant::now()));
        }
    }

    fn fetch_detail(&mut self, url: String, ctx: &mut Ctx) {
        let Some(q) = self.queues.iter().find(|q| q.url == url) else { return };
        if !self.in_flight.insert(url.clone()) {
            return;
        }
        let (name, arn, known_dlq) = (q.name.clone(), q.arn().to_string(), self.is_dlq(q));
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = sqs::dead_letter_sources(&aws, &url).map(|sources| {
                let moves = if known_dlq || !sources.is_empty() { sqs::list_moves(&aws, &arn).unwrap_or_default() } else { Vec::new() };
                // CloudWatch may be denied or empty: the age is a bonus.
                let oldest = sqs::oldest_age(&aws, &name).ok().flatten();
                Detail { sources, moves, oldest }
            });
            Msg::Detail { url, result }
        });
    }

    /// Related queues of the selected one: (label, queue name).
    fn links(&self) -> Vec<(String, String)> {
        let Some(q) = self.selected() else { return Vec::new() };
        let mut links = Vec::new();
        if let Some(r) = q.redrive() {
            links.push((format!("its dead-letter queue, after {} receives", r.max_receive_count), sqs::queue_name(&r.dead_letter_arn).to_string()));
        }
        for s in self.sources(q) {
            links.push(("sends its failures here".to_string(), s));
        }
        links
    }

    fn open_link(&mut self, ctx: &mut Ctx) {
        let Some((_, name)) = self.links().get(self.link).cloned() else { return };
        if self.select_named(&name) {
            self.focus = Focus::List;
        } else {
            ctx.toast(Level::Info, format!("{name} is not in this account and region"));
        }
    }

    fn latest_move(&self, url: &str) -> Option<&MoveTask> {
        match self.details.get(url) {
            Some(Ok(d)) => d.moves.first(),
            _ => None,
        }
    }

    // ---- messages ------------------------------------------------------------------------

    fn open_messages(&mut self, ctx: &mut Ctx) {
        let Some(q) = self.selected() else { return };
        if !self.is_dlq(q) {
            let reason = match (self.details.contains_key(&q.url), q.redrive()) {
                (false, _) => "Still checking whether it is a dead-letter queue…".to_string(),
                (true, Some(r)) => format!(
                    "Messages are only shown in dead-letter queues: reading one here counts as a receive and could move it to {}",
                    sqs::queue_name(&r.dead_letter_arn)
                ),
                (true, None) => "Messages are only shown in dead-letter queues: reading one counts as a receive".to_string(),
            };
            return ctx.toast(Level::Info, reason);
        }
        let (url, name) = (q.url.clone(), q.name.clone());
        self.messages = Some(Messages { url: url.clone(), name, load: LoadState::Loading, list: Vec::new(), state: ListState::default() });
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = sqs::peek(&aws, &url, sqs::PEEK_MAX);
            Msg::Messages { url, result }
        });
    }

    fn reload_messages(&mut self, ctx: &mut Ctx) {
        let Some(m) = &mut self.messages else { return };
        m.load = LoadState::Loading;
        let (aws, url) = (ctx.env().aws.clone(), m.url.clone());
        ctx.spawn(move || {
            let result = sqs::peek(&aws, &url, sqs::PEEK_MAX);
            Msg::Messages { url, result }
        });
    }

    fn selected_message(&self) -> Option<&Message> {
        let m = self.messages.as_ref()?;
        m.list.get(m.state.selected()?)
    }

    fn zoom_message(&mut self) {
        let Some(m) = self.selected_message() else { return };
        let title = format!("{} · message {}", self.messages.as_ref().map(|m| m.name.as_str()).unwrap_or_default(), m.message_id);
        let mut view = LogView::default();
        view.extend(m.pretty_body().lines().map(str::to_string));
        view.zoomed = true;
        // Start at the top, not following the end like a log.
        view.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        self.zoom = Some((title, view));
    }

    // ---- redrive -------------------------------------------------------------------------

    fn open_redrive(&mut self, ctx: &mut Ctx) {
        let Some(q) = self.selected() else { return };
        if !self.is_dlq(q) {
            return ctx.toast(Level::Info, "Redrive moves the messages of a dead-letter queue back to its source queues");
        }
        if self.latest_move(&q.url).is_some_and(MoveTask::running) {
            return ctx.toast(Level::Info, "A redrive of this queue is already running · x cancels it");
        }
        if q.visible() == 0 {
            return ctx.toast(Level::Info, format!("{} is empty: nothing to redrive", q.name));
        }
        let sources = self.sources(q);
        let mut items = vec![Item::new("Back to their source queues", sources.join(", "))];
        let mut destinations = vec![None];
        for other in self.queues.iter().filter(|o| o.url != q.url && o.fifo() == q.fifo()) {
            items.push(Item::new(other.name.clone(), format!("{} visible", other.visible())));
            destinations.push(Some(other.arn().to_string()));
        }
        let picker = Picker::new(format!("Redrive {} to", q.name), "choose", items, false);
        self.dialog = Some(Dialog::Destination(picker, q.url.clone(), destinations));
    }

    fn confirm_redrive(&mut self, url: String, destination_arn: Option<String>) {
        let Some(q) = self.queues.iter().find(|q| q.url == url) else { return };
        let destination = match &destination_arn {
            None => {
                let sources = self.sources(q);
                format!("their source queues ({})", if sources.is_empty() { "?".into() } else { sources.join(", ") })
            }
            Some(arn) => sqs::queue_name(arn).to_string(),
        };
        let lines = vec![
            format!("Move about {} messages from {} to {destination}?", q.visible(), q.name),
            "Whatever reads those queues processes them again; messages that fail again come back here.".into(),
        ];
        let action = Action::Redrive { url: url.clone(), source_arn: q.arn().to_string(), destination_arn, destination };
        self.dialog = Some(self.confirm(format!("Redrive {}", q.name), lines, action));
    }

    fn open_cancel(&mut self, ctx: &mut Ctx) {
        let Some(q) = self.selected() else { return };
        let Some(task) = self.latest_move(&q.url).filter(|t| t.status == "RUNNING") else {
            return ctx.toast(Level::Info, "No redrive running for this queue");
        };
        let Some(handle) = task.task_handle.clone() else { return };
        let lines = vec![
            format!("Stop moving messages from {}? {} moved so far stay where they went.", q.name, task.approximate_number_of_messages_moved),
        ];
        let action = Action::Cancel { url: q.url.clone(), task_handle: handle };
        self.dialog = Some(self.confirm(format!("Cancel the redrive of {}", q.name), lines, action));
    }

    fn confirm(&self, title: String, lines: Vec<String>, action: Action) -> Dialog {
        let typed = is_production(&self.env_name).then(|| (self.env_name.clone(), TextInput::default()));
        Dialog::Confirm { title, lines, typed, action, busy: false, error: None }
    }

    fn run(&mut self, action: Action, ctx: &mut Ctx) {
        let aws = ctx.env().aws.clone();
        match action {
            Action::Redrive { url, source_arn, destination_arn, destination } => ctx.spawn(move || {
                let result = sqs::start_move(&aws, &source_arn, destination_arn.as_deref()).map(|_| format!("Redrive started: messages are moving to {destination}"));
                Msg::Acted { url, result }
            }),
            Action::Cancel { url, task_handle } => ctx.spawn(move || {
                let result = sqs::cancel_move(&aws, &task_handle).map(|moved| format!("Redrive cancelled after {moved} messages"));
                Msg::Acted { url, result }
            }),
        }
    }

    fn dialog_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(dialog) = &mut self.dialog else { return };
        match dialog {
            Dialog::Destination(picker, url, destinations) => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.dialog = None,
                Outcome::Done(chosen) => {
                    let (url, destination) = (url.clone(), chosen.first().and_then(|&i| destinations.get(i).cloned()).flatten());
                    self.confirm_redrive(url, destination);
                }
            },
            Dialog::Confirm { busy: true, .. } => {}
            Dialog::Confirm { typed: Some((name, input)), action, busy, error, .. } => match input.handle(key) {
                InputEvent::Cancel => self.dialog = None,
                InputEvent::Submit if input.value.trim() == name.as_str() => {
                    *busy = true;
                    let action = action.clone();
                    self.run(action, ctx);
                }
                InputEvent::Submit => *error = Some(format!("Type '{name}' to confirm")),
                _ => {}
            },
            Dialog::Confirm { action, busy, .. } => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    *busy = true;
                    let action = action.clone();
                    self.run(action, ctx);
                }
                KeyCode::Esc | KeyCode::Char('n') => self.dialog = None,
                _ => {}
            },
        }
    }

    fn on_acted(&mut self, url: String, result: aws::Result<String>, ctx: &mut Ctx) {
        match result {
            Ok(text) => {
                self.dialog = None;
                ctx.toast(Level::Success, text);
                self.watching = Some((url.clone(), Instant::now() - MOVE_REFRESH));
            }
            Err(e) => match &mut self.dialog {
                Some(Dialog::Confirm { busy, error, .. }) => {
                    *busy = false;
                    *error = Some(e.short());
                }
                _ => ctx.toast(Level::Error, e.short()),
            },
        }
    }

    fn check_moves(&mut self, url: String, ctx: &mut Ctx) {
        let Some(q) = self.queues.iter().find(|q| q.url == url) else { return };
        let arn = q.arn().to_string();
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = sqs::list_moves(&aws, &arn).and_then(|moves| Ok((moves, sqs::queue(&aws, &url)?)));
            Msg::Moves { url, result }
        });
    }

    fn on_moves(&mut self, url: String, result: aws::Result<(Vec<MoveTask>, Queue)>, ctx: &mut Ctx) {
        let Ok((moves, fresh)) = result else {
            self.watching = None;
            return;
        };
        let running = moves.first().is_some_and(MoveTask::running);
        if let Some(task) = moves.first().filter(|_| !running && self.watching.as_ref().is_some_and(|(w, _)| *w == url)) {
            let name = sqs::queue_name(&url).to_string();
            match task.status.as_str() {
                "COMPLETED" => ctx.toast(
                    Level::Success,
                    format!("Redrive of {name} done: {} messages moved to {}", task.approximate_number_of_messages_moved, task.destination()),
                ),
                "FAILED" => ctx.toast(Level::Error, format!("Redrive of {name} failed: {}", task.failure_reason.clone().unwrap_or_default())),
                other => ctx.toast(Level::Info, format!("Redrive of {name}: {} ({} moved)", other.to_lowercase(), task.approximate_number_of_messages_moved)),
            }
            self.watching = None;
            // The source queues' counts changed too.
            self.load_queues(ctx);
        }
        if let Some(q) = self.queues.iter_mut().find(|q| q.url == url) {
            *q = fresh;
        }
        if let Some(Ok(d)) = self.details.get_mut(&url) {
            d.moves = moves;
        }
        if running && self.watching.is_none() {
            self.watching = Some((url, Instant::now()));
        }
    }

    // ---- rendering -----------------------------------------------------------------------

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search queues");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = match &self.load {
            LoadState::Loading => format!(" {} Queues ", theme::spinner(view.tick)),
            _ => format!(" Queues · {} ", self.visible.len()),
        };
        let focused = self.focus == Focus::List;
        if self.visible.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading queues…", theme::muted()),
                _ => Line::styled("No queues", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, focused)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|&i| {
                let q = &self.queues[i];
                let dlq = self.is_dlq(q);
                let (marker, marker_style) = match (dlq, q.visible() > 0) {
                    (true, true) => ("⚠ ", Style::new().fg(theme::ERR)),
                    (true, false) => ("↯ ", theme::muted()),
                    _ => ("  ", Style::new()),
                };
                let (count, count_style) = match (dlq, q.visible(), q.in_flight()) {
                    (true, n, _) if n > 0 => (format!("{n} message{}", if n == 1 { "" } else { "s" }), Style::new().fg(theme::ERR).bold()),
                    (true, _, _) => ("empty".to_string(), theme::muted()),
                    (false, n, 0) => (n.to_string(), theme::muted()),
                    (false, n, f) => (format!("{n} · {f} in flight"), theme::muted()),
                };
                let tags = match (dlq, q.fifo()) {
                    (true, true) => " dlq fifo",
                    (true, false) => " dlq",
                    (false, true) => " fifo",
                    _ => "",
                };
                let name_w = width.saturating_sub(count.chars().count() + tags.len() + 4);
                let name = ellipsize(&q.name, name_w);
                let pad = width.saturating_sub(2 + name.chars().count() + tags.len() + count.chars().count());
                ListItem::new(Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::raw(name),
                    Span::styled(tags, Style::new().fg(theme::APPROVAL)),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(count, count_style),
                ]))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, focused)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(q) = self.selected() else {
            f.render_widget(widgets::placeholder(Line::styled("Select a queue", theme::muted())).block(widgets::panel(" Queue ", false)), area);
            return;
        };
        let kw = 13;
        let dlq = self.is_dlq(q);
        let detail = self.details.get(&q.url);
        let err = Style::new().fg(theme::ERR);
        let mut counts = vec![Span::styled(format!("{} visible", q.visible()), if dlq && q.visible() > 0 { err.bold() } else { theme::bold() })];
        counts.push(Span::raw(format!(" · {} in flight · {} delayed", q.in_flight(), q.delayed())));
        let mut lines = vec![Line::from(vec![Span::styled(format!("{:<kw$} ", "Messages"), theme::muted())].into_iter().chain(counts).collect::<Vec<_>>())];
        if let Some(Ok(Detail { oldest: Some(age), .. })) = detail {
            lines.push(widgets::kv("Oldest", format!("{} old", fmt_duration(*age)), kw));
        }
        let kind = match (q.fifo(), q.attr("ContentBasedDeduplication")) {
            (true, Some("true")) => "FIFO, content-based deduplication",
            (true, _) => "FIFO",
            _ => "Standard",
        };
        lines.push(widgets::kv("Type", kind, kw));
        if let Some(v) = q.attr("MessageRetentionPeriod") {
            lines.push(widgets::kv("Retention", sqs::fmt_seconds(v), kw));
        }
        if let Some(v) = q.attr("VisibilityTimeout") {
            lines.push(widgets::kv("Visibility", format!("{} after a receive", sqs::fmt_seconds(v)), kw));
        }
        if let Some(v) = q.attr("DelaySeconds").filter(|v| *v != "0") {
            lines.push(widgets::kv("Delay", sqs::fmt_seconds(v), kw));
        }
        if let Some(v) = q.attr("ReceiveMessageWaitTimeSeconds") {
            let polling = if v == "0" { "short polling".to_string() } else { format!("long polling, {}", sqs::fmt_seconds(v)) };
            lines.push(widgets::kv("Receives", polling, kw));
        }
        if let Some(v) = q.attr("MaximumMessageSize").and_then(|v| v.parse::<i64>().ok()) {
            lines.push(widgets::kv("Max size", fmt_bytes(v), kw));
        }
        lines.push(widgets::kv("Encryption", q.encryption(), kw));
        if let Some(t) = q.attr("CreatedTimestamp").and_then(|t| t.parse::<f64>().ok()) {
            lines.push(widgets::kv("Created", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
        }
        lines.push(widgets::kv("URL", q.url.clone(), kw));

        let links = self.links();
        let link_hint = if self.focus == Focus::Links { "   ↑↓ queue · Enter open · ← back" } else { "   → / Enter to open" };
        if let Some(r) = q.redrive() {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![widgets::section("Dead-letter queue").spans.remove(0), Span::styled(link_hint, theme::muted())]));
            let dlq_name = sqs::queue_name(&r.dead_letter_arn).to_string();
            let held = self.by_arn(&r.dead_letter_arn).map(|d| d.visible());
            let mut spans = vec![self.link_marker(&links, &dlq_name), Span::styled(dlq_name.clone(), theme::bold())];
            spans.push(Span::styled(format!("  after {} receives", r.max_receive_count), theme::muted()));
            match held {
                Some(n) if n > 0 => spans.push(Span::styled(format!(" · holds {n}"), err)),
                Some(_) => spans.push(Span::styled(" · empty", theme::muted())),
                None => {}
            }
            lines.push(Line::from(spans));
        }
        if dlq {
            lines.push(Line::raw(""));
            let hint = if q.redrive().is_some() { "" } else { link_hint };
            lines.push(Line::from(vec![widgets::section("Dead-letter queue of").spans.remove(0), Span::styled(hint, theme::muted())]));
            for s in self.sources(q) {
                let max = self.queues.iter().find(|o| o.name == s).and_then(Queue::redrive).map(|r| format!("  after {} receives", r.max_receive_count));
                lines.push(Line::from(vec![
                    self.link_marker(&links, &s),
                    Span::styled(s.clone(), theme::bold()),
                    Span::styled(max.unwrap_or_default(), theme::muted()),
                ]));
            }
        }
        if let Some(allow) = q.redrive_allow() {
            lines.push(widgets::kv("Accepts", format!("failures from {allow}"), kw));
        }

        lines.push(Line::raw(""));
        lines.push(widgets::section("Messages"));
        match (dlq, detail) {
            (true, _) => {
                lines.push(widgets::hints_line(&[("m", "shows up to 50 of them"), ("R", "redrives them")]));
                if let Some(r) = q.redrive() {
                    lines.push(Line::styled(
                        format!("Reading them counts toward this queue's own limit of {} receives (then they move to its DLQ).", r.max_receive_count),
                        Style::new().fg(theme::WARN),
                    ));
                }
            }
            (false, None) => lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" checking…")])),
            (false, Some(_)) => {
                let why = match q.redrive() {
                    Some(r) => format!(" and could move it to {}", sqs::queue_name(&r.dead_letter_arn)),
                    None => String::new(),
                };
                lines.push(Line::styled(
                    format!("Only dead-letter queues show their messages: reading one here counts as a receive{why}."),
                    theme::muted(),
                ));
            }
        }
        match detail {
            Some(Err(e)) => lines.push(Line::styled(e.short(), err)),
            Some(Ok(d)) if !d.moves.is_empty() => {
                lines.push(Line::raw(""));
                lines.push(widgets::section("Redrives"));
                for (i, t) in d.moves.iter().take(3).enumerate() {
                    let (symbol, style) = match t.status.as_str() {
                        "RUNNING" | "CANCELLING" => (theme::spinner(view.tick), Style::new().fg(theme::RUNNING)),
                        "COMPLETED" => ("✓", Style::new().fg(theme::OK)),
                        "FAILED" => ("✗", err),
                        _ => ("■", theme::muted()),
                    };
                    let moved = match t.approximate_number_of_messages_to_move {
                        Some(total) => format!("{} of {total} moved", t.approximate_number_of_messages_moved),
                        None => format!("{} moved", t.approximate_number_of_messages_moved),
                    };
                    let when = t.started().map(|s| format!(" · {}", fmt_ago(s))).unwrap_or_default();
                    lines.push(Line::from(vec![
                        Span::styled(format!("{symbol} "), style),
                        Span::styled(t.status.to_lowercase(), style),
                        Span::raw(format!("  {moved} → {}", t.destination())),
                        Span::styled(when, theme::muted()),
                    ]));
                    if i == 0
                        && t.running()
                        && let Some(total) = t.approximate_number_of_messages_to_move.filter(|n| *n > 0)
                    {
                        let mut bar = vec![Span::raw("  ")];
                        bar.extend(widgets::progress_bar(t.approximate_number_of_messages_moved as f64 / total as f64, 24, theme::RUNNING));
                        bar.push(Span::styled("  x cancels", theme::muted()));
                        lines.push(Line::from(bar));
                    }
                    if let Some(reason) = &t.failure_reason {
                        lines.push(Line::styled(format!("  {reason}"), err));
                    }
                }
            }
            _ => {}
        }

        let mut block = widgets::panel(Line::from(format!(" {} ", q.name)).bold(), self.focus == Focus::Links);
        if dlq {
            let style = if q.visible() > 0 { err.bold() } else { theme::muted() };
            block = block.title_top(Line::styled(" dead-letter queue ", style).right_aligned());
        }
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
    }

    fn link_marker(&self, links: &[(String, String)], name: &str) -> Span<'static> {
        let selected = self.focus == Focus::Links && links.get(self.link).is_some_and(|(_, n)| n == name);
        Span::styled(if selected { "▶ " } else { "  " }, theme::accent())
    }

    fn render_messages(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let Some(m) = &mut self.messages else { return };
        let [left, right] = if area.width < 100 {
            area.layout(&Layout::vertical([Constraint::Percentage(45), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(45), Constraint::Fill(1)]))
        };
        self.list_area = left;
        let title = match &m.load {
            LoadState::Loading => format!(" {} {} · reading messages ", theme::spinner(view.tick), m.name),
            _ => {
                let total = self.queues.iter().find(|q| q.url == m.url).map_or(0, Queue::visible);
                format!(" {} · {} of {total} messages ", m.name, m.list.len())
            }
        };
        if m.list.is_empty() {
            let text = match &m.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Reading messages…", theme::muted()),
                _ => Line::styled("No messages came back · r tries again", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), left);
        } else {
            let width = left.width.saturating_sub(4) as usize;
            let items: Vec<ListItem> = m
                .list
                .iter()
                .map(|msg| {
                    let when = msg.sent().map(fmt_ago).unwrap_or_default();
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{when:>8} "), theme::muted()),
                        Span::styled(format!("↻{:<3} ", msg.receive_count()), Style::new().fg(theme::WARN)),
                        Span::raw(ellipsize(&msg.preview(), width.saturating_sub(14))),
                    ]))
                })
                .collect();
            let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
            f.render_stateful_widget(list, left, &mut m.state);
        }

        let Some(msg) = self.selected_message() else {
            let note = "Messages are read without being consumed: they stay in the queue.";
            f.render_widget(widgets::placeholder(Line::styled(note, theme::muted())).block(widgets::panel(" Message ", false)), right);
            return;
        };
        let kw = 14;
        let mut lines = Vec::new();
        if let Some(t) = msg.sent() {
            lines.push(widgets::kv("Sent", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
        }
        if let Some(t) = msg.first_received() {
            lines.push(widgets::kv("First received", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
        }
        lines.push(widgets::kv("Received", format!("{} times", msg.receive_count()), kw));
        if let Some(g) = msg.group() {
            lines.push(widgets::kv("Group", g.to_string(), kw));
        }
        if let Some(sender) = msg.attributes.get("SenderId") {
            lines.push(widgets::kv("Sender", sender.clone(), kw));
        }
        lines.push(widgets::kv("Message ID", msg.message_id.clone(), kw));
        if !msg.message_attributes.is_empty() {
            lines.push(Line::raw(""));
            lines.push(widgets::section("Attributes"));
            for (name, a) in &msg.message_attributes {
                let value = a.string_value.clone().or_else(|| a.binary_value.as_ref().map(|b| format!("({} bytes, base64)", b.len()))).unwrap_or_default();
                let style = if name.to_lowercase().contains("error") { Style::new().fg(theme::ERR) } else { Style::new() };
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<kw$} ", ellipsize(name, kw)), theme::muted()),
                    Span::styled(value, style),
                    Span::styled(format!("  {}", a.data_type), theme::muted()),
                ]));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![widgets::section("Body").spans.remove(0), Span::styled("   Enter full screen · y copy", theme::muted())]));
        lines.extend(msg.pretty_body().lines().map(|l| Line::raw(l.to_string())));
        let block = widgets::panel(Line::from(" Message ").bold(), false);
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), right);
    }

    fn render_dialog(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let color = if is_production(&self.env_name) { theme::ERR } else { theme::ACCENT };
        match &mut self.dialog {
            None => {}
            Some(Dialog::Destination(picker, ..)) => {
                let rect = widgets::popup_area(area, 70, picker.height(area.height.saturating_sub(4)));
                picker.render(f, rect, color);
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
                let rect = widgets::popup_area(area, 76, widgets::wrapped_height(&text, 72) + 2);
                widgets::popup(f, rect, title, color, Text::from(text));
                if let Some((row, col)) = cursor {
                    f.set_cursor_position((rect.x + 2 + col, rect.y + 1 + row));
                }
            }
        }
    }

    fn copy(&self, text: &str, what: &str, ctx: &mut Ctx) {
        let ok = crate::platform::copy_to_clipboard(text);
        ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { format!("Copied {what}") } else { "Clipboard not available".into() });
    }
}

impl Module for Sqs {
    fn title(&self) -> &'static str {
        "SQS"
    }

    fn description(&self) -> &'static str {
        "SQS queues · depth, dead-letter queues, their messages, redrive"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Sqs::new();
        self.env_name = ctx.env().name.clone();
        self.region = ctx.env().region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load_queues(ctx);
        }
    }

    /// Selects the queue `target`: a name, URL or ARN.
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        if !matches!(self.load, LoadState::Loaded) {
            self.pending_open = Some(target.to_string());
            if matches!(self.load, LoadState::Idle) {
                self.load_queues(ctx);
            }
            return;
        }
        self.zoom = None;
        self.dialog = None;
        self.messages = None;
        self.filtering = false;
        let name = sqs::queue_name(target).to_string();
        if !self.select_named(&name) {
            ctx.toast(Level::Warn, format!("Queue not found here: {name}"));
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some((_, view)) = &mut self.zoom {
            if let LogKey::Copy = view.key(key) {
                let text = view.lines().join("\n");
                self.copy(&text, "the body", ctx);
            }
            if self.zoom.as_ref().is_some_and(|(_, v)| !v.zoomed) {
                self.zoom = None;
            }
            return Handled::Yes;
        }
        if self.dialog.is_some() {
            self.dialog_key(key, ctx);
            return Handled::Yes;
        }
        if let Some(m) = &mut self.messages {
            let selected = m.state.selected().unwrap_or(0);
            let count = m.list.len();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    m.state.select(Some(selected.saturating_sub(1)));
                    self.detail_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    m.state.select(Some((selected + 1).min(count.saturating_sub(1))));
                    self.detail_scroll = 0;
                }
                KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
                KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
                KeyCode::Enter | KeyCode::Char('z') => self.zoom_message(),
                KeyCode::Char('y') => {
                    if let Some(body) = self.selected_message().map(|m| m.body.clone()) {
                        self.copy(&body, "the body", ctx);
                    }
                }
                KeyCode::Char('r') => self.reload_messages(ctx),
                KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => {
                    self.messages = None;
                    self.detail_scroll = 0;
                }
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
        if self.focus == Focus::Links {
            let count = self.links().len();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.link = self.link.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.link = (self.link + 1).min(count.saturating_sub(1)),
                KeyCode::Enter | KeyCode::Right => self.open_link(ctx),
                KeyCode::Esc | KeyCode::Left => self.focus = Focus::List,
                _ => return Handled::No,
            }
            return Handled::Yes;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::Enter | KeyCode::Right => match self.links().len() {
                0 => {}
                1 => {
                    self.link = 0;
                    self.open_link(ctx);
                }
                _ => {
                    self.focus = Focus::Links;
                    self.link = 0;
                }
            },
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Char('m') => self.open_messages(ctx),
            KeyCode::Char('R') => self.open_redrive(ctx),
            KeyCode::Char('x') => self.open_cancel(ctx),
            KeyCode::Char('y') => {
                if let Some(url) = self.selected().map(|q| q.url.clone()) {
                    self.copy(&url, "the queue URL", ctx);
                }
            }
            KeyCode::Char('o') => {
                if let Some(q) = self.selected() {
                    let url = sqs::console_url(&self.region, &q.url);
                    if !crate::platform::open_url(&url) {
                        ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                    }
                }
            }
            KeyCode::Char('r') => {
                self.details.clear();
                self.load_queues(ctx);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() || self.zoom.is_some() || !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        if let Some(m) = &mut self.messages {
            let selected = m.state.selected().unwrap_or(0);
            let index = match event.kind {
                MouseEventKind::ScrollUp => selected.saturating_sub(1),
                MouseEventKind::ScrollDown => selected + 1,
                MouseEventKind::Down(MouseButton::Left) => m.state.offset() + event.row.saturating_sub(self.list_area.y + 1) as usize,
                _ => return Handled::No,
            };
            m.state.select(Some(index.min(m.list.len().saturating_sub(1))));
            self.detail_scroll = 0;
            return Handled::Yes;
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
            Msg::Queues(result) => {
                match result {
                    Ok(queues) => {
                        self.queues = queues;
                        self.load = LoadState::Loaded;
                    }
                    Err(e) => self.load = LoadState::Failed(e),
                }
                self.refilter();
                if let Some(target) = self.pending_open.take() {
                    self.open(&target, ctx);
                }
            }
            Msg::Detail { url, result } => {
                self.in_flight.remove(&url);
                let running = result.as_ref().is_ok_and(|d| d.moves.first().is_some_and(MoveTask::running));
                self.details.insert(url.clone(), result);
                // A DLQ found through ListDeadLetterSourceQueues may move up the list.
                self.refilter();
                if running && self.watching.is_none() {
                    self.watching = Some((url, Instant::now()));
                }
            }
            Msg::Messages { url, result } => {
                let Some(m) = self.messages.as_mut().filter(|m| m.url == url) else { return };
                match result {
                    Ok(list) => {
                        m.list = list;
                        m.load = LoadState::Loaded;
                        m.state.select(if m.list.is_empty() { None } else { Some(0) });
                    }
                    Err(e) => m.load = LoadState::Failed(e),
                }
            }
            Msg::Moves { url, result } => self.on_moves(url, result, ctx),
            Msg::Acted { url, result } => self.on_acted(url, result, ctx),
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if let Some((url, at)) = &self.pending
            && at.elapsed() >= DEBOUNCE
        {
            let url = url.clone();
            self.pending = None;
            if self.selected().is_some_and(|q| q.url == url) && !self.details.contains_key(&url) {
                self.fetch_detail(url, ctx);
            }
        }
        if let Some((url, at)) = &mut self.watching
            && at.elapsed() >= MOVE_REFRESH
        {
            *at = Instant::now();
            let url = url.clone();
            self.check_moves(url, ctx);
        }
        if active && matches!(self.load, LoadState::Loaded) && self.dialog.is_none() && self.listed_at.elapsed() >= LIST_REFRESH {
            self.load_queues(ctx);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some((title, zoomed)) = &mut self.zoom {
            let title = title.clone();
            zoomed.render_zoomed(f, area, &title);
            return;
        }
        if self.messages.is_some() {
            self.render_messages(f, area, view);
        } else {
            let [left, right] = if area.width < 90 {
                area.layout(&Layout::vertical([Constraint::Percentage(40), Constraint::Fill(1)]))
            } else {
                area.layout(&Layout::horizontal([Constraint::Percentage(40), Constraint::Fill(1)]))
            };
            self.render_list(f, left, view);
            self.render_detail(f, right, view);
        }
        self.render_dialog(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.zoom.is_some() {
            return vec![("/", "search"), ("y", "copy"), ("Esc", "back")];
        }
        match &self.dialog {
            Some(Dialog::Destination(..)) => return vec![("↑↓", "move"), ("Enter", "choose"), ("Esc", "cancel")],
            Some(Dialog::Confirm { .. }) => return vec![("Enter", "confirm"), ("Esc", "cancel")],
            None => {}
        }
        if self.messages.is_some() {
            return vec![("↑↓", "message"), ("Enter", "full screen"), ("y", "copy body"), ("r", "read again"), ("Esc", "queues")];
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        if self.focus == Focus::Links {
            return vec![("↑↓", "queue"), ("Enter", "open"), ("←", "back")];
        }
        let dlq = self.selected().is_some_and(|q| self.is_dlq(q));
        let mut hints = vec![("↑↓", "move"), ("→", "related queue")];
        if dlq {
            hints.extend([("m", "messages"), ("R", "redrive")]);
            if self.selected().and_then(|q| self.latest_move(&q.url)).is_some_and(|t| t.status == "RUNNING") {
                hints.push(("x", "cancel redrive"));
            }
        }
        hints.extend([("/", "search"), ("y", "copy URL"), ("o", "console"), ("r", "reload")]);
        hints
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || !self.in_flight.is_empty()
            || self.pending.is_some()
            || self.watching.is_some()
            || self.messages.as_ref().is_some_and(|m| matches!(m.load, LoadState::Loading))
            || matches!(self.dialog, Some(Dialog::Confirm { busy: true, .. }))
    }

    fn capturing(&self) -> bool {
        self.filtering || self.dialog.is_some() || self.zoom.is_some()
    }

    fn wants_mouse(&self) -> bool {
        self.zoom.is_none()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.dialog {
            Some(Dialog::Destination(picker, ..)) => picker.paste(text),
            Some(Dialog::Confirm { typed: Some((_, input)), .. }) => input.insert_str(text, false),
            _ if self.filtering => {
                self.filter.insert_str(text, false);
                self.refilter();
            }
            _ => {}
        }
    }
}

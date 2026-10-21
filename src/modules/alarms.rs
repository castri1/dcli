//! Alarms: CloudWatch alarms (metric and composite) in every environment, those firing
//! first, with what makes each one fire, its state changes and a graph of its metric
//! against the threshold. Enter opens the alarm's resource in its module. The header
//! counts the current environment's firing alarms, refreshed in the background, and a
//! desktop notification tells when one starts firing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{filter_bar, matches_filter};
use crate::aws::alarms::{self, Alarm, Change, State, fmt_number, fmt_period};
use crate::aws::credentials::effective_profile;
use crate::aws::metrics::{self, Series};
use crate::aws::{self, Aws};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, fmt_utc, now_secs, show_invisible, uri_encode};

/// How often alarms refresh: every environment while the module is open, only the current
/// one (for the header) otherwise.
const REFRESH_EVERY: Duration = Duration::from_secs(60);
const DEBOUNCE: Duration = Duration::from_millis(200);
/// Desktop notifications per refresh for alarms that start firing; more are summed up.
const MAX_NOTIFICATIONS: usize = 3;
/// Rows of the metric graph.
const GRAPH_ROWS: usize = 6;

/// (environment, alarm name).
type Key = (String, String);

enum Msg {
    Alarms { env: usize, result: aws::Result<Vec<Alarm>> },
    Detail { key: Key, detail: Box<Detail> },
}

enum Load {
    Idle,
    Loading,
    /// The environment's AWS session expired.
    Login,
    Failed(String),
    Ready(Vec<Alarm>),
}

struct EnvAlarms {
    name: String,
    profile: String,
    aws: Arc<Aws>,
    load: Load,
    refreshed: Option<Instant>,
    in_flight: bool,
    /// Alarm states at the previous refresh, to notice the ones that start firing.
    seen: Option<HashMap<String, State>>,
}

impl EnvAlarms {
    fn alarms(&self) -> &[Alarm] {
        match &self.load {
            Load::Ready(list) => list,
            _ => &[],
        }
    }
}

/// A line of the list: an alarm (environment, index), or an environment that could not be read.
#[derive(Clone, Copy, PartialEq)]
enum Row {
    Alarm(usize, usize),
    Env(usize),
}

/// What the detail panel shows beyond the alarm itself.
struct Detail {
    history: Result<Vec<Change>, String>,
    /// The alarm's metric over `span`, when it watches a single metric.
    series: Option<Result<Series, String>>,
    span: (f64, f64),
    loaded: Instant,
}

pub struct Alarms {
    envs: Vec<EnvAlarms>,
    /// The app's environment.
    current: usize,
    /// Every environment (the default) or only the current one.
    all: bool,
    /// The module has been opened: from then on every environment refreshes while it shows.
    opened: bool,
    rows: Vec<Row>,
    list_state: ListState,
    filter: TextInput,
    filtering: bool,
    details: HashMap<Key, Detail>,
    detail_in_flight: HashSet<Key>,
    pending_detail: Option<(Key, Instant)>,
    detail_scroll: u16,
    list_area: Rect,
    /// An alarm of the current environment to select once read (from the `:` search).
    open_target: Option<String>,
}

fn look(state: State) -> (&'static str, Style, &'static str) {
    match state {
        State::Alarm => ("●", Style::new().fg(theme::ERR), "ALARM"),
        State::InsufficientData => ("◌", Style::new().fg(theme::WARN), "INSUFFICIENT DATA"),
        State::Ok => ("✓", Style::new().fg(theme::OK), "OK"),
    }
}

/// The time range of an alarm's graph: about 60 periods, between 3 hours and a week.
fn span_for(alarm: &Alarm, now: f64) -> (f64, f64) {
    let period = f64::from(alarm.period().unwrap_or(300));
    (now - (period * 60.0).clamp(3.0 * 3600.0, 7.0 * 86_400.0), now)
}

/// `12`, `1.5k`, `2.3M`, `0.25`
fn short(v: f64) -> String {
    let a = v.abs();
    let trim = |s: String| if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if a >= 1e6 {
        format!("{}M", trim(format!("{:.1}", v / 1e6)))
    } else if a >= 1e4 {
        format!("{}k", trim(format!("{:.1}", v / 1e3)))
    } else if a >= 100.0 || v.fract() == 0.0 {
        format!("{}", v.round() as i64)
    } else {
        trim(format!("{v:.2}"))
    }
}

/// `3h`, `7d`, or `1h 30m` for spans that are not whole hours.
fn span_label(secs: f64) -> String {
    match secs.round() as u64 {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        _ => crate::util::fmt_duration(secs),
    }
}

/// The last part of an SNS topic (or any) ARN.
fn arn_name(arn: &str) -> &str {
    arn.rsplit(':').next().unwrap_or(arn)
}

/// The alarm's metric over `span` as bars (a datapoint covers its period), the threshold
/// as a dotted line, and below them a strip marking when the alarm was firing and the
/// time axis. Breaching values are red.
fn graph(alarm: &Alarm, points: &[(f64, f64)], (t0, t1): (f64, f64), firing: &[(f64, f64)], width: usize) -> Vec<Line<'static>> {
    const AXIS: usize = 8;
    const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let cols = width.saturating_sub(AXIS).max(10);
    let period = f64::from(alarm.period().unwrap_or(60));
    let dt = (t1 - t0) / cols as f64;
    let values: Vec<Option<f64>> = (0..cols)
        .map(|c| {
            let (a, b) = (t0 + c as f64 * dt, t0 + (c + 1) as f64 * dt);
            points.iter().filter(|(t, _)| *t < b && t + period > a).map(|(_, v)| *v).reduce(f64::max)
        })
        .collect();
    let max = values.iter().flatten().copied().chain(alarm.threshold).fold(0.0f64, f64::max);
    let min = values.iter().flatten().copied().chain(alarm.threshold).fold(0.0f64, f64::min);
    let top = if max > min { max + (max - min) * 0.1 } else { min + 1.0 };
    let eighths = |v: f64| (((v - min) / (top - min)) * (GRAPH_ROWS * 8) as f64).round().max(0.0) as usize;
    let threshold_row = alarm.threshold.map(|t| (eighths(t) / 8).min(GRAPH_ROWS - 1));
    let line_style = Style::new().fg(theme::ERR);
    let mut lines = Vec::new();
    for row in (0..GRAPH_ROWS).rev() {
        let label = match row {
            r if r == GRAPH_ROWS - 1 => short(top),
            r if Some(r) == threshold_row => alarm.threshold.map(short).unwrap_or_default(),
            0 => short(min),
            _ => String::new(),
        };
        let label_style = if Some(row) == threshold_row { line_style } else { theme::muted() };
        let mut spans = vec![Span::styled(format!("{label:>6} ┤"), label_style)];
        let (mut run, mut run_style) = (String::new(), Style::new());
        for v in &values {
            let fill = v.map_or(0, |v| eighths(v).saturating_sub(row * 8).min(8));
            let (c, style) = match (fill, v) {
                (0, _) if Some(row) == threshold_row => ('┄', line_style),
                (0, _) => (' ', Style::new()),
                (f, Some(v)) => (BLOCKS[f], Style::new().fg(if alarm.breaches(*v) { theme::ERR } else { theme::ACCENT })),
                (_, None) => (' ', Style::new()),
            };
            if style != run_style && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), run_style));
            }
            run_style = style;
            run.push(c);
        }
        spans.push(Span::styled(run, run_style));
        lines.push(Line::from(spans));
    }
    if !firing.is_empty() {
        let strip: String = (0..cols)
            .map(|c| {
                let mid = t0 + (c as f64 + 0.5) * dt;
                if firing.iter().any(|(a, b)| mid >= *a && mid < *b) { '▀' } else { ' ' }
            })
            .collect();
        lines.push(Line::from(vec![Span::styled(format!("{:>6}  ", "alarm"), theme::muted()), Span::styled(strip, line_style)]));
    }
    let start = format!("-{}", span_label(t1 - t0));
    lines.push(Line::styled(format!("{:>8}{start:<w$}now", "", w = cols.saturating_sub(3)), theme::muted()));
    lines
}

/// A Markdown summary of an alarm, for an AI agent or a chat.
fn summary(env: &EnvAlarms, a: &Alarm, detail: Option<&Detail>) -> String {
    let mut md = format!("## CloudWatch alarm {}: {} ({}, {})\n\n", a.state.api(), show_invisible(&a.name), env.name, env.aws.region);
    if let Some(t) = a.since {
        md.push_str(&format!("- **State:** {} since {} ({})\n", a.state.api(), fmt_utc(t), fmt_ago(t)));
    }
    if let Some(r) = &a.reason {
        md.push_str(&format!("- **Reason:** {r}\n"));
    }
    md.push_str(&format!("- **Condition:** {}\n", a.condition()));
    if let Some(m) = &a.metric {
        let dims: Vec<String> = m.dimensions.iter().map(|(k, v)| format!("{k}={v}")).collect();
        md.push_str(&format!("- **Metric:** `{}` `{}` ({} per {}) {}\n", m.namespace, m.name, m.stat, fmt_period(m.period), dims.join(", ")));
    }
    for q in &a.queries {
        let what = match (&q.expression, &q.metric) {
            (Some(e), _) => format!("`{e}`"),
            (None, Some(m)) => format!("`{}` `{}` ({} per {})", m.namespace, m.name, m.stat, fmt_period(m.period)),
            _ => String::new(),
        };
        md.push_str(&format!("- **Query `{}`:** {what}{}\n", q.id, if q.return_data { " (alarmed on)" } else { "" }));
    }
    if let Some(t) = &a.treat_missing {
        md.push_str(&format!("- **Missing data:** treated as {t}\n"));
    }
    if !a.alarm_actions.is_empty() {
        let names: Vec<&str> = a.alarm_actions.iter().map(|x| arn_name(x)).collect();
        md.push_str(&format!("- **Actions on ALARM:** {}{}\n", names.join(", "), if a.actions_enabled { "" } else { " (disabled)" }));
    }
    if let Some(d) = &a.description {
        md.push_str(&format!("- **Description:** {d}\n"));
    }
    md.push_str(&format!("- **ARN:** `{}`\n", a.arn));
    if let Some(Some(Ok(series))) = detail.map(|d| &d.series) {
        let recent: Vec<String> =
            series.points.iter().rev().take(10).map(|(t, v)| format!("{} {}", &fmt_utc(*t)[11..16], fmt_number(*v))).collect();
        if !recent.is_empty() {
            md.push_str(&format!("\n**Latest datapoints (UTC, newest first):** {}\n", recent.join(" · ")));
        }
    }
    if let Some(Ok(changes)) = detail.map(|d| &d.history)
        && !changes.is_empty()
    {
        md.push_str("\n**State changes (newest first):**\n\n");
        for c in changes.iter().take(10) {
            md.push_str(&format!("- {} · {}\n", fmt_utc(c.at), c.summary));
        }
    }
    md
}

impl Alarms {
    pub fn new() -> Alarms {
        Alarms {
            envs: Vec::new(),
            current: 0,
            all: true,
            opened: false,
            rows: Vec::new(),
            list_state: ListState::default(),
            filter: TextInput::default(),
            filtering: false,
            details: HashMap::new(),
            detail_in_flight: HashSet::new(),
            pending_detail: None,
            detail_scroll: 0,
            list_area: Rect::default(),
            open_target: None,
        }
    }

    /// Selects the alarm asked for by `open`, once the current environment's are read.
    fn try_open(&mut self) {
        let Some(name) = self.open_target.clone() else { return };
        if !matches!(self.envs.get(self.current).map(|e| &e.load), Some(Load::Ready(_))) {
            return;
        }
        self.open_target = None;
        self.filter.clear();
        self.filtering = false;
        self.refilter();
        let row = self.rows.iter().position(|r| matches!(r, Row::Alarm(e, i) if *e == self.current && self.alarm(*e, *i).is_some_and(|a| a.name == name)));
        if let Some(index) = row {
            self.select(index);
        }
    }

    fn fetch(&mut self, index: usize, ctx: &mut Ctx) {
        let env = &mut self.envs[index];
        env.in_flight = true;
        if !matches!(env.load, Load::Ready(_)) {
            env.load = Load::Loading;
        }
        let aws = env.aws.clone();
        ctx.spawn(move || Msg::Alarms { env: index, result: alarms::describe_alarms(&aws) });
    }

    /// Refreshes the environments that are due: every one shown while the module is open,
    /// the current one (for the header) otherwise.
    fn refresh_due(&mut self, ctx: &mut Ctx, active: bool) {
        for i in 0..self.envs.len() {
            let shown = active && self.opened && (self.all || i == self.current);
            let env = &self.envs[i];
            if (shown || i == self.current) && !env.in_flight && env.refreshed.is_none_or(|t| t.elapsed() >= REFRESH_EVERY) {
                self.fetch(i, ctx);
            }
        }
    }

    fn alarm(&self, env: usize, index: usize) -> Option<&Alarm> {
        self.envs.get(env)?.alarms().get(index)
    }

    fn key_of(&self, row: Row) -> Option<Key> {
        match row {
            Row::Alarm(e, i) => Some((self.envs[e].name.clone(), self.alarm(e, i)?.name.clone())),
            Row::Env(e) => Some((self.envs[e].name.clone(), String::new())),
        }
    }

    fn selected(&self) -> Option<Row> {
        self.rows.get(self.list_state.selected()?).copied()
    }

    fn selected_alarm(&self) -> Option<(&EnvAlarms, &Alarm)> {
        match self.selected()? {
            Row::Alarm(e, i) => Some((&self.envs[e], self.alarm(e, i)?)),
            Row::Env(_) => None,
        }
    }

    fn firing_here(&self) -> usize {
        self.envs.get(self.current).map_or(0, |e| e.alarms().iter().filter(|a| a.state == State::Alarm).count())
    }

    /// Rebuilds the list (after a refresh, the filter or the scope changed), keeping the selection.
    fn refilter(&mut self) {
        let keep = self.selected().and_then(|r| self.key_of(r));
        let query = self.filter.value.trim().to_lowercase();
        let mut problems = Vec::new();
        let mut found: Vec<(usize, usize)> = Vec::new();
        for (e, env) in self.envs.iter().enumerate() {
            if !self.all && e != self.current {
                continue;
            }
            match &env.load {
                Load::Ready(list) => {
                    for (i, a) in list.iter().enumerate() {
                        let dims: Vec<String> = a.dimensions().iter().map(|(k, v)| format!("{k}={v}")).collect();
                        if matches_filter(&format!("{} {} {} {}", a.name, env.name, a.condition(), dims.join(" ")), &query) {
                            found.push((e, i));
                        }
                    }
                }
                Load::Login | Load::Failed(_) => problems.push(Row::Env(e)),
                Load::Idle | Load::Loading => {}
            }
        }
        // Firing first, the most recent first; then the rest by name.
        found.sort_by(|&(ea, ia), &(eb, ib)| {
            let (a, b) = (&self.envs[ea].alarms()[ia], &self.envs[eb].alarms()[ib]);
            a.state.cmp(&b.state).then_with(|| match a.state {
                State::Alarm => b.since.unwrap_or(0.0).total_cmp(&a.since.unwrap_or(0.0)),
                _ => a.name.cmp(&b.name).then(ea.cmp(&eb)),
            })
        });
        self.rows = problems.into_iter().chain(found.into_iter().map(|(e, i)| Row::Alarm(e, i))).collect();
        let index = keep.and_then(|k| self.rows.iter().position(|r| self.key_of(*r).as_ref() == Some(&k))).unwrap_or(0);
        self.list_state.select(if self.rows.is_empty() { None } else { Some(index) });
        self.queue_detail();
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
        self.queue_detail();
    }

    /// Loads the selected alarm's history and graph shortly (not while scrolling past it).
    fn queue_detail(&mut self) {
        let Some(row @ Row::Alarm(..)) = self.selected() else { return };
        let Some(key) = self.key_of(row) else { return };
        if !self.details.contains_key(&key) && !self.detail_in_flight.contains(&key) {
            self.pending_detail = Some((key, Instant::now()));
        }
    }

    fn fetch_detail(&mut self, key: Key, ctx: &mut Ctx) {
        let Some(env) = self.envs.iter().find(|e| e.name == key.0) else { return };
        let Some(alarm) = env.alarms().iter().find(|a| a.name == key.1) else { return };
        if !self.detail_in_flight.insert(key.clone()) {
            return;
        }
        let (aws, name, query, span) = (env.aws.clone(), alarm.name.clone(), alarm.query(), span_for(alarm, now_secs()));
        ctx.spawn(move || {
            let history = alarms::history(&aws, &name, 50).map_err(|e| e.short());
            let series = query.map(|q| {
                metrics::get_metric_data(&aws, &[q], span.0, span.1).map(|mut s| s.pop().unwrap_or_default()).map_err(|e| e.short())
            });
            Msg::Detail { key, detail: Box::new(Detail { history, series, span, loaded: Instant::now() }) }
        });
    }

    fn on_alarms(&mut self, index: usize, result: aws::Result<Vec<Alarm>>, ctx: &mut Ctx) {
        let Some(env) = self.envs.get_mut(index) else { return };
        env.in_flight = false;
        env.refreshed = Some(Instant::now());
        match result {
            Ok(list) => {
                // Alarms that were not firing at the previous refresh (none on the first one).
                let started: Vec<String> = match &env.seen {
                    Some(seen) => list
                        .iter()
                        .filter(|a| a.state == State::Alarm && seen.get(&a.name) != Some(&State::Alarm))
                        .map(|a| show_invisible(&a.name))
                        .collect(),
                    None => Vec::new(),
                };
                env.seen = Some(list.iter().map(|a| (a.name.clone(), a.state)).collect());
                env.load = Load::Ready(list);
                let title = format!("Alarm firing in {}", env.name);
                for name in started.iter().take(MAX_NOTIFICATIONS) {
                    ctx.notify(Level::Error, &title, name);
                }
                if started.len() > MAX_NOTIFICATIONS {
                    ctx.notify(Level::Error, &title, &format!("and {} more", started.len() - MAX_NOTIFICATIONS));
                }
            }
            Err(e) if e.is_login_required() => env.load = Load::Login,
            // A failed refresh keeps what was read before.
            Err(_) if matches!(env.load, Load::Ready(_)) => {}
            Err(e) => env.load = Load::Failed(e.short()),
        }
        self.refilter();
        self.try_open();
    }

    fn open_selected(&mut self, ctx: &mut Ctx) {
        match self.selected() {
            Some(Row::Env(e)) => match self.envs[e].load {
                Load::Login => ctx.login(self.envs[e].profile.clone()),
                _ => self.fetch(e, ctx),
            },
            Some(Row::Alarm(..)) => {
                let Some((env, alarm)) = self.selected_alarm() else { return };
                match alarm.resource() {
                    Some((module, target)) => {
                        if env.name != ctx.env().name {
                            ctx.switch_env(env.name.clone());
                        }
                        ctx.jump(module, target);
                    }
                    None if alarm.composite => ctx.toast(Level::Info, "A composite alarm: its rule names the alarms it watches"),
                    None => ctx.toast(Level::Info, "dcli has no module for this alarm's metric; o opens it in the console"),
                }
            }
            None => {}
        }
    }

    fn copy_summary(&self, ctx: &mut Ctx) {
        let Some((env, alarm)) = self.selected_alarm() else { return };
        let detail = self.details.get(&(env.name.clone(), alarm.name.clone()));
        let ok = crate::platform::copy_to_clipboard(&summary(env, alarm, detail));
        ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Copied the alarm's summary" } else { "Clipboard not available" });
    }

    fn open_console(&self, ctx: &mut Ctx) {
        let Some((env, alarm)) = self.selected_alarm() else { return };
        let region = &env.aws.region;
        let url = aws::console_url(region, &format!("cloudwatch/home?region={region}#alarmsV2:alarm/{}", uri_encode(&alarm.name)));
        if !crate::platform::open_url(&url) {
            ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
        }
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to filter alarms");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let loading = self.envs.iter().any(|e| e.in_flight);
        let firing = self.rows.iter().filter(|r| matches!(r, Row::Alarm(e, i) if self.alarm(*e, *i).is_some_and(|a| a.state == State::Alarm))).count();
        let spinner = if loading { format!("{} ", theme::spinner(view.tick)) } else { String::new() };
        let scope = if self.all { "every environment".to_string() } else { self.envs.get(self.current).map(|e| e.name.clone()).unwrap_or_default() };
        let mut right = Vec::new();
        if firing > 0 {
            right.push(Span::styled(format!(" ● {firing} firing "), Style::new().fg(theme::ERR).bold()));
        }
        right.push(Span::styled(format!(" {scope} "), theme::muted()));
        let block = widgets::panel(format!(" {spinner}Alarms · {} ", self.rows.len()), true).title_top(Line::from(right).right_aligned());
        if self.rows.is_empty() {
            let text = if loading || self.envs.iter().any(|e| matches!(e.load, Load::Idle)) {
                Line::styled("Loading alarms…", theme::muted())
            } else if self.filter.value.is_empty() {
                Line::styled("No CloudWatch alarms here", theme::muted())
            } else {
                Line::styled("No alarm matches the filter", theme::muted())
            };
            f.render_widget(widgets::placeholder(text).block(block), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let many = self.all && self.envs.len() > 1;
        let env_w = if many { self.envs.iter().map(|e| e.name.chars().count()).max().unwrap_or(3) + 2 } else { 0 };
        let name_w = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Alarm(e, i) => self.alarm(*e, *i).map(|a| show_invisible(&a.name).chars().count()),
                Row::Env(_) => None,
            })
            .max()
            .unwrap_or(10)
            .min(width * 2 / 5)
            .max(10);
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| match *row {
                Row::Env(e) => {
                    let env = &self.envs[e];
                    match &env.load {
                        Load::Login => ListItem::new(Line::styled(
                            ellipsize(&format!("! {}: the AWS session expired · Enter signs in", env.name), width),
                            Style::new().fg(theme::WARN),
                        )),
                        Load::Failed(err) => {
                            ListItem::new(Line::styled(ellipsize(&format!("✗ {}: {err}", env.name), width), Style::new().fg(theme::ERR)))
                        }
                        _ => ListItem::new(Line::raw("")),
                    }
                }
                Row::Alarm(e, i) => {
                    let (env, a) = (&self.envs[e], &self.envs[e].alarms()[i]);
                    let (symbol, style, _) = look(a.state);
                    let since = a.since.map(fmt_ago).unwrap_or_default().replace(" ago", "");
                    let name = ellipsize(&show_invisible(&a.name), name_w);
                    let used = 2 + name_w + 2 + env_w + since.chars().count() + 2;
                    let condition = ellipsize(&a.condition(), width.saturating_sub(used));
                    let pad = width.saturating_sub(used + condition.chars().count());
                    let mut spans = vec![
                        Span::styled(format!("{symbol} "), style),
                        Span::styled(format!("{name:<name_w$}  "), if a.state == State::Alarm { theme::bold() } else { Style::new() }),
                    ];
                    if many {
                        spans.push(Span::styled(format!("{:<env_w$}", env.name), Style::new().fg(theme::env_color(&env.name))));
                    }
                    spans.extend([
                        Span::styled(condition, theme::muted()),
                        Span::raw(" ".repeat(pad + 2)),
                        Span::styled(since, theme::muted()),
                    ]);
                    ListItem::new(Line::from(spans))
                }
            })
            .collect();
        let list = List::new(items).block(block).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let kw = 11;
        match self.selected() {
            None => {
                let text = Line::styled("Select an alarm", theme::muted());
                f.render_widget(widgets::placeholder(text).block(widgets::panel(" Alarm ", false)), area);
            }
            Some(Row::Env(e)) => {
                let env = &self.envs[e];
                let lines = match &env.load {
                    Load::Login => vec![
                        Line::styled(
                            format!("The AWS session of {} (profile {}) expired.", env.name, effective_profile(&env.profile)),
                            Style::new().fg(theme::WARN),
                        ),
                        Line::styled("Enter signs in; its alarms load at the next refresh (r refreshes now).", theme::muted()),
                    ],
                    Load::Failed(err) => vec![
                        Line::styled(format!("Could not read the alarms of {}:", env.name), Style::new().fg(theme::ERR)),
                        Line::raw(err.clone()),
                        Line::styled("Enter tries again.", theme::muted()),
                    ],
                    _ => Vec::new(),
                };
                let block = widgets::panel(format!(" {} ", env.name), false);
                f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
            }
            Some(Row::Alarm(e, i)) => {
                let (env, a) = (&self.envs[e], &self.envs[e].alarms()[i]);
                let (symbol, style, label) = look(a.state);
                let block = widgets::panel(Line::from(format!(" {} ", show_invisible(&a.name))).bold(), false)
                    .title_top(Line::styled(format!(" {symbol} {label} "), style.bold()).right_aligned());
                let inner_width = block.inner(area).width as usize;
                let mut lines = vec![widgets::kv("Env", format!("{} · {}", env.name, env.aws.region), kw)];
                if let Some(t) = a.since {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:<kw$} ", "State"), theme::muted()),
                        Span::styled(label, style.bold()),
                        Span::raw(format!(" since {} · {}", fmt_ago(t), fmt_utc(t))),
                    ]));
                }
                if let Some(r) = &a.reason {
                    lines.push(widgets::kv("Reason", r.clone(), kw));
                }
                lines.push(widgets::kv("Condition", Span::styled(a.condition(), theme::bold()), kw));
                if let Some(m) = &a.metric {
                    lines.push(widgets::kv("Metric", format!("{} · {} ({} per {})", m.namespace, m.name, m.stat, fmt_period(m.period)), kw));
                    for (n, (k, v)) in m.dimensions.iter().enumerate() {
                        lines.push(widgets::kv(if n == 0 { "Dimensions" } else { "" }, format!("{k} = {v}"), kw));
                    }
                }
                for (n, q) in a.queries.iter().enumerate() {
                    let what = match (&q.expression, &q.metric) {
                        (Some(e), _) => e.clone(),
                        (None, Some(m)) => format!("{} · {} ({})", m.namespace, m.name, m.stat),
                        _ => String::new(),
                    };
                    let mark = if q.return_data { "  ← alarmed on" } else { "" };
                    lines.push(widgets::kv(if n == 0 { "Queries" } else { "" }, format!("{}: {what}{mark}", q.id), kw));
                }
                if let Some(t) = &a.treat_missing {
                    lines.push(widgets::kv("Missing", format!("treated as {t}"), kw));
                }
                let actions: Vec<&str> = a.alarm_actions.iter().map(|x| arn_name(x)).collect();
                let actions = if actions.is_empty() { "none".to_string() } else { actions.join(", ") };
                let disabled = if a.actions_enabled { Span::raw("") } else { Span::styled("  actions disabled", Style::new().fg(theme::WARN)) };
                lines.push(Line::from(vec![Span::styled(format!("{:<kw$} ", "On alarm"), theme::muted()), Span::raw(actions), disabled]));
                if let Some(d) = &a.description {
                    lines.push(widgets::kv("About", d.clone(), kw));
                }

                let key = (env.name.clone(), a.name.clone());
                let detail = self.details.get(&key);
                let waiting = || Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" loading…")]);
                lines.push(Line::raw(""));
                match (a.graphable(), detail) {
                    (None, _) if a.composite => {}
                    (None, _) => {
                        lines.push(widgets::section("Graph"));
                        lines.push(Line::styled("A metric math alarm: see its queries above, or o for the console's graph.", theme::muted()));
                    }
                    (Some(_), None) => {
                        lines.push(widgets::section("Graph"));
                        lines.push(waiting());
                    }
                    (Some(_), Some(d)) => {
                        lines.push(widgets::section(&format!("Graph · last {}", span_label(d.span.1 - d.span.0))));
                        match &d.series {
                            Some(Ok(series)) if series.points.is_empty() => {
                                lines.push(Line::styled("No datapoints in this time range.", theme::muted()));
                            }
                            Some(Ok(series)) => {
                                let firing = match &d.history {
                                    Ok(changes) => alarms::firing_spans(changes, d.span.1),
                                    Err(_) => Vec::new(),
                                };
                                lines.extend(graph(a, &series.points, d.span, &firing, inner_width));
                            }
                            Some(Err(err)) => lines.push(Line::styled(err.clone(), Style::new().fg(theme::ERR))),
                            None => {}
                        }
                    }
                }
                lines.push(Line::raw(""));
                lines.push(widgets::section("State changes"));
                match detail.map(|d| &d.history) {
                    None => lines.push(waiting()),
                    Some(Err(err)) => lines.push(Line::styled(err.clone(), Style::new().fg(theme::ERR))),
                    Some(Ok(changes)) if changes.is_empty() => lines.push(Line::styled("None recorded", theme::muted())),
                    Some(Ok(changes)) => {
                        for c in changes.iter().take(8) {
                            let mut spans = vec![Span::styled(format!("{:>9}  ", fmt_ago(c.at)), theme::muted())];
                            match (c.from, c.to) {
                                (Some(from), Some(to)) => {
                                    spans.push(Span::styled(look(from).2, look(from).1));
                                    spans.push(Span::styled(" → ", theme::muted()));
                                    spans.push(Span::styled(look(to).2, look(to).1.bold()));
                                }
                                _ => spans.push(Span::raw(c.summary.clone())),
                            }
                            lines.push(Line::from(spans));
                        }
                    }
                }
                lines.push(Line::raw(""));
                let open = match a.resource() {
                    Some((module, target)) => format!("Enter opens {} in {module} · ", arn_name(&target)),
                    None => String::new(),
                };
                lines.push(Line::styled(format!("{open}y copies a summary · o console"), theme::muted()));
                f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
            }
        }
    }
}

impl Module for Alarms {
    fn title(&self) -> &'static str {
        "Alarms"
    }

    fn description(&self) -> &'static str {
        "CloudWatch alarms firing, in every environment · graphs, history, the resource behind each"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Alarms::new();
        let current = ctx.env();
        for (name, e) in &ctx.config().environments {
            // The current environment shares the app's session (an L sign-in refreshes it).
            let aws = if *name == current.name { current.aws.clone() } else { Aws::new(&e.profile, &e.region) };
            self.envs.push(EnvAlarms { name: name.clone(), profile: e.profile.clone(), aws, load: Load::Idle, refreshed: None, in_flight: false, seen: None });
        }
        self.current = match self.envs.iter().position(|e| e.name == current.name) {
            Some(i) => i,
            None => {
                self.envs.push(EnvAlarms {
                    name: current.name.clone(),
                    profile: current.profile.clone(),
                    aws: current.aws.clone(),
                    load: Load::Idle,
                    refreshed: None,
                    in_flight: false,
                    seen: None,
                });
                self.envs.len() - 1
            }
        };
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        self.opened = true;
        self.refresh_due(ctx, true);
    }

    /// Selects alarm `target` of the current environment, now or once read.
    fn open(&mut self, target: &str, _ctx: &mut Ctx) {
        self.open_target = Some(target.to_string());
        self.try_open();
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
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
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(usize::MAX),
            KeyCode::Enter | KeyCode::Right => self.open_selected(ctx),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Char('a') => {
                self.all = !self.all;
                self.refilter();
                self.refresh_due(ctx, true);
            }
            KeyCode::Char('y') => self.copy_summary(ctx),
            KeyCode::Char('o') => self.open_console(ctx),
            KeyCode::Char('r') => {
                for i in 0..self.envs.len() {
                    if (self.all || i == self.current) && !self.envs[i].in_flight {
                        self.fetch(i, ctx);
                    }
                }
                if let Some(key) = self.selected().and_then(|r| self.key_of(r)) {
                    self.details.remove(&key);
                    self.queue_detail();
                }
            }
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                // Rows start below the filter bar and the panel border.
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
            Msg::Alarms { env, result } => self.on_alarms(env, result, ctx),
            Msg::Detail { key, detail } => {
                self.detail_in_flight.remove(&key);
                self.details.insert(key, *detail);
            }
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if active
            && let Some((key, at)) = &self.pending_detail
            && at.elapsed() >= DEBOUNCE
        {
            let key = key.clone();
            self.pending_detail = None;
            if self.selected().and_then(|r| self.key_of(r)).as_ref() == Some(&key) {
                self.fetch_detail(key, ctx);
            }
        }
        self.refresh_due(ctx, active);
        // The selected alarm's graph and history keep up while it is shown.
        if active
            && let Some(key) = self.selected().filter(|r| matches!(r, Row::Alarm(..))).and_then(|r| self.key_of(r))
            && self.details.get(&key).is_some_and(|d| d.loaded.elapsed() >= REFRESH_EVERY)
            && !self.detail_in_flight.contains(&key)
        {
            self.fetch_detail(key, ctx);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [left, right] = if area.width < 110 {
            area.layout(&Layout::vertical([Constraint::Percentage(45), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(50), Constraint::Fill(1)]))
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
            ("Enter", "open its resource"),
            ("/", "filter"),
            ("a", if self.all { "this env only" } else { "every env" }),
            ("y", "copy summary"),
            ("o", "console"),
            ("r", "refresh"),
        ]
    }

    fn badge(&self) -> Option<(String, Color)> {
        let firing = self.firing_here();
        (firing > 0).then(|| (format!("● {firing}"), theme::ERR))
    }

    fn animating(&self) -> bool {
        self.envs.iter().any(|e| e.in_flight) || self.pending_detail.is_some() || !self.detail_in_flight.is_empty()
    }

    fn capturing(&self) -> bool {
        self.filtering
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if self.filtering {
            self.filter.insert_str(text, false);
            self.refilter();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::alarms::MetricStat;

    #[test]
    fn graphs_values_against_the_threshold() {
        let alarm = Alarm {
            name: "errors".into(),
            arn: String::new(),
            composite: false,
            description: None,
            state: State::Alarm,
            reason: None,
            since: None,
            metric: Some(MetricStat { namespace: "AWS/Lambda".into(), name: "Errors".into(), dimensions: vec![], stat: "Sum".into(), period: 60 }),
            queries: vec![],
            threshold: Some(2.0),
            comparison: Some("GreaterThanThreshold".into()),
            evaluation_periods: Some(1),
            datapoints_to_alarm: None,
            treat_missing: None,
            threshold_metric: None,
            rule: None,
            actions_enabled: true,
            alarm_actions: vec![],
            ok_actions: vec![],
            insufficient_actions: vec![],
        };
        // Ten minutes, ten columns: 0 0 … then 4 in the last two minutes.
        let points: Vec<(f64, f64)> = (0..10).map(|m| (f64::from(m) * 60.0, if m >= 8 { 4.0 } else { 0.0 })).collect();
        let lines = graph(&alarm, &points, (0.0, 600.0), &[(480.0, 600.0)], 18);
        let text: Vec<String> = lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect();
        assert_eq!(text.len(), GRAPH_ROWS + 2);
        assert!(text[0].starts_with("   4.4 ┤"), "{text:?}");
        // The threshold (2 of 4.4) sits on the third row from the bottom, dotted where no bar covers it.
        assert_eq!(text[GRAPH_ROWS - 3], "     2 ┤┄┄┄┄┄┄┄┄██");
        assert_eq!(text[GRAPH_ROWS], " alarm          ▀▀");
        assert_eq!(short(1520.0), "1520");
        assert_eq!(short(15_200.0), "15.2k");
        assert_eq!(short(0.25), "0.25");
        assert_eq!((span_label(3.0 * 3600.0), span_label(7.0 * 86_400.0), span_label(5400.0)), ("3h".into(), "7d".into(), "1h 30m".into()));
    }
}

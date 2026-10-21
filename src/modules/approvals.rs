//! Approvals: every manual approval waiting in the pipelines of every environment, to
//! approve or reject without opening each one. It looks in the background (the header
//! shows the count), also while another module is open, and notifies new ones.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::aws::{self, Aws, codepipeline as cp};
use crate::aws::credentials::effective_profile;
use crate::config::is_production;
use crate::pipeline::{self, Waiting};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::{theme, widgets};
use crate::util::{ellipsize, fmt_ago, fmt_utc};

/// How often every environment is looked at again.
const SCAN_EVERY: Duration = Duration::from_secs(60);
const DECISIONS: &[&str] = &["Approve", "Reject"];

enum Msg {
    Scanned { env: usize, pipeline: String, result: aws::Result<Vec<Waiting>> },
    Decided { env: usize, pipeline: String, approved: bool, result: aws::Result<()> },
}

struct Column {
    env: String,
    profile: String,
    aws: Arc<Aws>,
}

/// What a scan of one pipeline found: the approvals waiting, or why it could not look.
enum Found {
    Waiting(Vec<Waiting>),
    /// The environment's AWS session expired.
    Login,
    Failed(String),
}

/// A line of the list.
enum Row<'a> {
    Waiting(usize, &'a Waiting),
    Login(usize),
    Failed(usize, &'a str, &'a str),
}

struct Decision {
    env: usize,
    waiting: Waiting,
    form: Form,
    busy: bool,
}

pub struct Approvals {
    columns: Vec<Column>,
    /// Per (environment index, pipeline), in config order.
    found: BTreeMap<(usize, String), Found>,
    in_flight: usize,
    scanned: Option<Instant>,
    /// Approval tokens already seen, so only new ones are notified.
    seen: HashSet<String>,
    selected: usize,
    list_state: ListState,
    list_area: Rect,
    dialog: Option<Decision>,
    current_env: String,
}

impl Approvals {
    pub fn new() -> Approvals {
        Approvals {
            columns: Vec::new(),
            found: BTreeMap::new(),
            in_flight: 0,
            scanned: None,
            seen: HashSet::new(),
            selected: 0,
            list_state: ListState::default(),
            list_area: Rect::default(),
            dialog: None,
            current_env: String::new(),
        }
    }

    fn scan(&mut self, ctx: &mut Ctx) {
        self.scanned = Some(Instant::now());
        for (env, column) in self.columns.iter().enumerate() {
            let Some(cfg) = ctx.config().environments.get(&column.env) else { continue };
            for p in &cfg.pipelines {
                self.in_flight += 1;
                let (aws, pipeline) = (column.aws.clone(), p.name.clone());
                ctx.spawn(move || {
                    let result = pipeline::waiting_approvals(&aws, &pipeline);
                    Msg::Scanned { env, pipeline, result }
                });
            }
        }
    }

    fn rows(&self) -> Vec<Row<'_>> {
        let mut rows = Vec::new();
        for env in 0..self.columns.len() {
            let found: Vec<(&String, &Found)> =
                self.found.range((env, String::new())..(env + 1, String::new())).map(|((_, p), f)| (p, f)).collect();
            if found.iter().any(|(_, f)| matches!(f, Found::Login)) {
                rows.push(Row::Login(env));
                continue;
            }
            for (pipeline, f) in found {
                match f {
                    Found::Waiting(list) => rows.extend(list.iter().map(|w| Row::Waiting(env, w))),
                    Found::Failed(e) => rows.push(Row::Failed(env, pipeline, e)),
                    Found::Login => {}
                }
            }
        }
        rows
    }

    fn waiting_count(&self) -> usize {
        self.found.values().map(|f| if let Found::Waiting(list) = f { list.len() } else { 0 }).sum()
    }

    fn select(&mut self, index: usize) {
        let count = self.rows().len();
        self.selected = index.min(count.saturating_sub(1));
        self.list_state.select((count > 0).then_some(self.selected));
    }

    fn open_decision(&mut self, ctx: &mut Ctx) {
        let (env, waiting) = match self.rows().get(self.selected) {
            Some(Row::Waiting(env, w)) => (*env, (*w).clone()),
            _ => return ctx.toast(Level::Info, "Select an approval"),
        };
        let column = &self.columns[env];
        let mut fields = vec![
            Field::fixed("Approval", format!("{} · {} · {} / {}", column.env, waiting.pipeline, waiting.stage, waiting.action)),
        ];
        if let Some(e) = &waiting.execution {
            fields.push(Field::fixed("Revision", pipeline::revision(e, &[]).one_line()));
        }
        fields.push(Field::choice("Decision", DECISIONS, "Approve"));
        fields.push(Field::text("Comment", "").optional("optional"));
        let mut form = Form::new(format!("Manual approval · {}", column.env), fields).enter_submits().submit_label("submit");
        if is_production(&column.env) {
            form = form.confirm_with(&column.env);
        }
        let request: Vec<String> = [waiting.custom_data.clone(), waiting.link.clone()].into_iter().flatten().collect();
        if !request.is_empty() {
            form.note = Some(request.join(" · "));
        }
        self.dialog = Some(Decision { env, waiting, form, busy: false });
    }

    fn dialog_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(d) = &mut self.dialog else { return };
        if d.busy {
            return;
        }
        match d.form.handle(key) {
            FormEvent::Cancel => self.dialog = None,
            FormEvent::Pending => {}
            FormEvent::Submit => {
                let approved = d.form.get("Decision") == "Approve";
                let summary = match d.form.get("Comment").trim() {
                    "" if approved => "Approved via dcli".to_string(),
                    "" => "Rejected via dcli".to_string(),
                    text => text.to_string(),
                };
                d.busy = true;
                let (env, w, aws) = (d.env, d.waiting.clone(), self.columns[d.env].aws.clone());
                ctx.spawn(move || {
                    let result = cp::put_approval(&aws, &w.pipeline, &w.stage, &w.action, &w.token, approved, &summary);
                    Msg::Decided { env, pipeline: w.pipeline, approved, result }
                });
            }
        }
    }

    fn on_scanned(&mut self, env: usize, pipeline: String, result: aws::Result<Vec<Waiting>>, ctx: &mut Ctx) {
        self.in_flight = self.in_flight.saturating_sub(1);
        let key = (env, pipeline);
        let looked_before = self.found.contains_key(&key);
        let found = match result {
            Ok(list) => {
                let env_name = self.columns.get(env).map(|c| c.env.as_str()).unwrap_or_default();
                for w in &list {
                    // Pipelines already tells about approvals in the current environment.
                    if self.seen.insert(w.token.clone()) && looked_before && env_name != self.current_env {
                        ctx.notify(Level::Warn, &format!("{} needs approval", w.pipeline), &format!("{env_name} · {} / {}", w.stage, w.action));
                    }
                }
                Found::Waiting(list)
            }
            Err(e) if e.is_login_required() => Found::Login,
            Err(e) => Found::Failed(e.short()),
        };
        self.found.insert(key, found);
        self.select(self.selected);
    }

    fn on_decided(&mut self, env: usize, pipeline: String, approved: bool, result: aws::Result<()>, ctx: &mut Ctx) {
        match result {
            Ok(()) => {
                self.dialog = None;
                ctx.toast(Level::Success, format!("{} {pipeline}", if approved { "Approved" } else { "Rejected" }));
                // Look again at that pipeline now.
                if let Some(column) = self.columns.get(env) {
                    self.in_flight += 1;
                    let aws = column.aws.clone();
                    ctx.spawn(move || {
                        let result = pipeline::waiting_approvals(&aws, &pipeline);
                        Msg::Scanned { env, pipeline, result }
                    });
                }
            }
            Err(e) => {
                if let Some(d) = &mut self.dialog {
                    d.busy = false;
                    d.form.fail(e.short());
                }
            }
        }
    }

    fn open_link(&self, ctx: &mut Ctx) {
        let rows = self.rows();
        let Some(Row::Waiting(env, w)) = rows.get(self.selected) else { return };
        let region = &self.columns[*env].aws.region;
        let url = w.link.clone().unwrap_or_else(|| {
            aws::console_url(region, &format!("codesuite/codepipeline/pipelines/{}/view?region={region}", w.pipeline))
        });
        if !crate::platform::is_web_url(&url) {
            return ctx.toast(Level::Warn, format!("The approval's link is not a web address; dcli does not open it: {url}"));
        }
        if !crate::platform::open_url(&url) {
            ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
        }
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let count = self.waiting_count();
        let state = if self.in_flight > 0 {
            format!("{} looking", theme::spinner(view.tick))
        } else {
            format!("looked {}s ago", self.scanned.map_or(0, |t| t.elapsed().as_secs()))
        };
        let block = widgets::panel(format!(" Waiting for approval · {count} "), true)
            .title_top(Line::styled(format!(" {state} "), theme::muted()).right_aligned());
        let rows = self.rows();
        if rows.is_empty() {
            let envs: Vec<&str> = self.columns.iter().map(|c| c.env.as_str()).collect();
            let text = if self.scanned.is_none() || self.in_flight > 0 {
                Line::styled(format!("Looking in {}…", envs.join(", ")), theme::muted())
            } else if self.found.is_empty() {
                Line::styled("No pipelines configured: add them with n in Pipelines", theme::muted())
            } else {
                Line::styled(format!("Nothing waiting for approval in {}", envs.join(", ")), theme::muted())
            };
            f.render_widget(widgets::placeholder(text).block(block), area);
            return;
        }
        let width = area.width.saturating_sub(4) as usize;
        let env_w = self.columns.iter().map(|c| c.env.chars().count()).max().unwrap_or(3);
        let items: Vec<ListItem> = rows
            .iter()
            .map(|row| {
                let env_span = |env: usize| {
                    let name = &self.columns[env].env;
                    Span::styled(format!("{name:<env_w$}  "), Style::new().fg(theme::env_color(name)).bold())
                };
                let line = match row {
                    Row::Waiting(env, w) => {
                        let since = w.since.map(fmt_ago).unwrap_or_default();
                        let commit = w.execution.as_ref().and_then(|e| pipeline::revision(e, &[]).commit).unwrap_or_default();
                        let text = format!("{}  {} / {}  {commit}", w.pipeline, w.stage, w.action);
                        let room = width.saturating_sub(env_w + 6 + since.chars().count());
                        let text = ellipsize(&text, room);
                        let pad = room.saturating_sub(text.chars().count());
                        Line::from(vec![
                            Span::styled("◆ ", Style::new().fg(theme::APPROVAL)),
                            env_span(*env),
                            Span::raw(text),
                            Span::raw(" ".repeat(pad)),
                            Span::styled(since, theme::muted()),
                        ])
                    }
                    Row::Login(env) => Line::from(vec![
                        Span::styled("! ", Style::new().fg(theme::WARN)),
                        env_span(*env),
                        Span::styled("session expired · Enter signs in", Style::new().fg(theme::WARN)),
                    ]),
                    Row::Failed(env, pipeline, e) => Line::from(vec![
                        Span::styled("✗ ", Style::new().fg(theme::ERR)),
                        env_span(*env),
                        Span::styled(ellipsize(&format!("{pipeline}: {e}"), width.saturating_sub(env_w + 4)), Style::new().fg(theme::ERR)),
                    ]),
                };
                ListItem::new(line)
            })
            .collect();
        let list = List::new(items).block(block).highlight_style(theme::selected());
        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect) {
        let kw = 10;
        let mut lines = Vec::new();
        let title = match self.rows().get(self.selected) {
            Some(Row::Waiting(env, w)) => {
                let column = &self.columns[*env];
                lines.push(widgets::kv("Pipeline", Span::styled(w.pipeline.clone(), theme::bold()), kw));
                lines.push(widgets::kv("Waits at", format!("{} / {}", w.stage, w.action), kw));
                if let Some(t) = w.since {
                    lines.push(widgets::kv("Since", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
                }
                if let Some(e) = &w.execution {
                    lines.push(widgets::kv("Revision", pipeline::revision(e, &[]).one_line(), kw));
                    let id = &e.pipeline_execution_id;
                    lines.push(widgets::kv("Execution", id[..8.min(id.len())].to_string(), kw));
                }
                if let Some(data) = &w.custom_data {
                    lines.push(Line::raw(""));
                    lines.extend(data.lines().map(|l| Line::raw(l.to_string())));
                }
                if let Some(link) = &w.link {
                    lines.push(widgets::kv("Link", Span::styled(link.clone(), theme::accent()), kw));
                }
                lines.push(Line::raw(""));
                let mut hint = "a approve or reject · Enter opens it in Pipelines · o ".to_string();
                hint.push_str(if w.link.is_some() { "opens the link" } else { "console" });
                lines.push(Line::styled(hint, theme::muted()));
                format!(" {} · {} ", column.env, w.pipeline)
            }
            Some(Row::Login(env)) => {
                let column = &self.columns[*env];
                lines.push(Line::styled(
                    format!("The AWS session of {} (profile {}) expired.", column.env, effective_profile(&column.profile)),
                    Style::new().fg(theme::WARN),
                ));
                lines.push(Line::styled("Enter signs in; its approvals show up afterwards.", theme::muted()));
                format!(" {} ", column.env)
            }
            Some(Row::Failed(env, pipeline, e)) => {
                lines.push(Line::styled(format!("Could not look at {pipeline}: {e}"), Style::new().fg(theme::ERR)));
                format!(" {} · {pipeline} ", self.columns[*env].env)
            }
            None => {
                lines.push(Line::styled("Manual approvals of every environment show up here.", theme::muted()));
                lines.push(Line::styled("They are looked for every minute, also while another module is open.", theme::muted()));
                " Approvals ".to_string()
            }
        };
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(widgets::panel(title, false)), area);
    }
}

impl Module for Approvals {
    fn title(&self) -> &'static str {
        "Approvals"
    }

    fn description(&self) -> &'static str {
        "Manual approvals waiting, in every environment"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Approvals::new();
        self.current_env = ctx.env().name.clone();
        self.columns = ctx
            .config()
            .environments
            .iter()
            .map(|(name, e)| Column { env: name.clone(), profile: e.profile.clone(), aws: Aws::new(&e.profile, &e.region) })
            .collect();
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() {
            self.dialog_key(key, ctx);
            return Handled::Yes;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected + 1),
            KeyCode::Char('a') => self.open_decision(ctx),
            KeyCode::Enter => match self.rows().get(self.selected) {
                Some(Row::Login(env)) => ctx.login(self.columns[*env].profile.clone()),
                Some(Row::Waiting(env, w)) => {
                    let (env, pipeline) = (self.columns[*env].env.clone(), w.pipeline.clone());
                    if env != ctx.env().name {
                        ctx.switch_env(env);
                    }
                    ctx.jump("Pipelines", pipeline);
                }
                _ => {}
            },
            KeyCode::Char('o') => self.open_link(ctx),
            KeyCode::Char('r') => {
                if self.in_flight == 0 {
                    self.scan(ctx);
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() || !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.select(self.selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(self.selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = event.row.saturating_sub(self.list_area.y + 1) as usize;
                self.select(self.list_state.offset() + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Scanned { env, pipeline, result } => self.on_scanned(env, pipeline, result, ctx),
            Msg::Decided { env, pipeline, approved, result } => self.on_decided(env, pipeline, approved, result, ctx),
        }
    }

    /// Looks again every minute, whether or not the module is open (for the header count).
    fn tick(&mut self, ctx: &mut Ctx, _active: bool) {
        if self.in_flight == 0 && self.scanned.is_none_or(|t| t.elapsed() >= SCAN_EVERY) {
            self.scan(ctx);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [list, detail] = if area.width >= 110 {
            area.layout(&Layout::horizontal([Constraint::Percentage(55), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::vertical([Constraint::Fill(1), Constraint::Length(12)]))
        };
        self.render_list(f, list, view);
        self.render_detail(f, detail);
        if let Some(d) = &self.dialog {
            let color = if is_production(&self.columns[d.env].env) { theme::ERR } else { theme::APPROVAL };
            d.form.render(f, area, color);
            if d.busy {
                let body = ratatui::text::Text::from(vec![
                    Line::raw(""),
                    Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw("Sending…")]),
                ]);
                widgets::popup(f, widgets::popup_area(area, 30, 5), "Approvals", theme::ACCENT, body);
            }
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.dialog.is_some() {
            return vec![("←→", "approve / reject"), ("Tab", "next field"), ("Enter", "submit"), ("Esc", "cancel")];
        }
        vec![("↑↓", "move"), ("a", "approve / reject"), ("Enter", "open / sign in"), ("o", "link"), ("r", "refresh")]
    }

    fn badge(&self) -> Option<(String, Color)> {
        let count = self.waiting_count();
        (count > 0).then(|| (count.to_string(), theme::APPROVAL))
    }

    fn animating(&self) -> bool {
        self.in_flight > 0 || self.dialog.as_ref().is_some_and(|d| d.busy)
    }

    fn capturing(&self) -> bool {
        self.dialog.is_some()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if let Some(d) = &mut self.dialog {
            d.form.paste(text);
        }
    }
}

//! What is deployed where: applications × environments, the commit each environment runs
//! (its last successful execution), what is deploying now and how far each environment
//! trails the one to its left. Enter opens the pipeline; `g` compares the commits; `p`
//! promotes what the environment to the left runs and `R` rolls back (both confirmed and
//! run by Pipelines, in that environment).

use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::aws::{self, Aws};
use crate::aws::credentials::effective_profile;
use crate::matrix::{App, Cell, Drift, drift};
use crate::pipeline::{self, Intent, Status};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets;
use crate::util::{ellipsize, fmt_ago, fmt_utc};

const REFRESH_EVERY: Duration = Duration::from_secs(90);

enum Msg {
    Cell { row: usize, col: usize, result: aws::Result<Cell> },
}

enum State {
    /// The application has no pipeline in that environment.
    Missing,
    Loading,
    /// The environment's AWS session expired.
    Login,
    Failed(String),
    Ready(Cell),
}

struct Column {
    env: String,
    profile: String,
    aws: Arc<Aws>,
}

pub struct Matrix {
    columns: Vec<Column>,
    apps: Vec<App>,
    cells: Vec<Vec<State>>,
    row: usize,
    col: usize,
    in_flight: usize,
    refreshed: Option<Instant>,
    table_area: Rect,
    app_width: u16,
    col_width: u16,
    /// The app's environment, marked in the header.
    current_env: String,
}

impl Matrix {
    pub fn new() -> Matrix {
        Matrix {
            columns: Vec::new(),
            apps: Vec::new(),
            cells: Vec::new(),
            row: 0,
            col: 0,
            in_flight: 0,
            refreshed: None,
            table_area: Rect::default(),
            app_width: 0,
            col_width: 0,
            current_env: String::new(),
        }
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        self.refreshed = Some(Instant::now());
        for (row, app) in self.apps.iter().enumerate() {
            for (col, pipeline) in app.pipelines.iter().enumerate() {
                let Some(pipeline) = pipeline.clone() else { continue };
                // Keep showing the last data while it refreshes.
                if !matches!(self.cells[row][col], State::Ready(_)) {
                    self.cells[row][col] = State::Loading;
                }
                self.in_flight += 1;
                let aws = self.columns[col].aws.clone();
                ctx.spawn(move || Msg::Cell { row, col, result: Cell::fetch(&aws, &pipeline) });
            }
        }
    }

    fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        match self.cells.get(row)?.get(col)? {
            State::Ready(c) => Some(c),
            _ => None,
        }
    }

    /// Drift of a cell against the nearest environment to its left that runs the app.
    fn drift_of(&self, row: usize, col: usize) -> Option<(usize, Drift)> {
        let cell = self.cell(row, col)?;
        let left = (0..col).rev().find(|&c| self.apps[row].pipelines[c].is_some())?;
        Some((left, drift(self.cell(row, left)?, cell)?))
    }

    fn compare_link(&self) -> Result<String, &'static str> {
        let (row, col) = (self.row, self.col);
        let Some((left, d)) = self.drift_of(row, col) else { return Err("Nothing to compare: no environment to the left with this app") };
        if d == Drift::InSync {
            return Err("Same commit as the environment to the left");
        }
        let (Some(newer), Some(older)) = (self.cell(row, left).and_then(Cell::deployed), self.cell(row, col).and_then(Cell::deployed))
        else {
            return Err("Nothing to compare");
        };
        pipeline::compare_url(older, newer, None).ok_or("No commit comparison for this source (GitHub, GitLab or Bitbucket only)")
    }

    /// `p`: deploys to the selected cell what the environment to its left runs. Pipelines,
    /// in that environment, confirms and runs it.
    fn promote(&self, ctx: &mut Ctx) {
        let (row, col) = (self.row, self.col);
        let Some(pipeline) = self.apps.get(row).and_then(|a| a.pipelines.get(col)?.clone()) else {
            return ctx.toast(Level::Info, "No pipeline here to promote to");
        };
        let (app, env) = (&self.apps[row].name, &self.columns[col].env);
        let Some(left) = (0..col).rev().find(|&c| self.apps[row].pipelines[c].is_some()) else {
            return ctx.toast(Level::Info, format!("{env} is the first environment of {app}: nothing to promote from"));
        };
        let left_env = self.columns[left].env.clone();
        match &self.cells[row][col] {
            State::Ready(_) => {}
            State::Login => return ctx.toast(Level::Warn, format!("Sign in to {env} first (Enter)")),
            _ => return ctx.toast(Level::Info, format!("{env} is still loading")),
        }
        let Some(theirs) = self.cell(row, left).and_then(Cell::deployed) else {
            return ctx.toast(Level::Info, format!("{left_env} has no successful deploy of {app} to promote (or is still loading)"));
        };
        let behind = match self.drift_of(row, col).map(|(_, d)| d) {
            Some(Drift::InSync) => return ctx.toast(Level::Info, format!("{env} already runs what {left_env} runs")),
            Some(Drift::Behind(n)) => Some(n),
            _ => None,
        };
        let Some(revision) = crate::matrix::commit_of(theirs) else {
            return ctx.toast(Level::Info, format!("{left_env}'s deploy of {app} has no revision to promote"));
        };
        let intent = Intent::Promote {
            pipeline,
            revision: revision.to_string(),
            source_action: theirs.source_revisions.first().map(|r| r.action_name.clone()).unwrap_or_default(),
            from_env: left_env,
            summary: pipeline::revision(theirs, &[]).one_line(),
            behind,
        };
        self.hand_off(col, intent, ctx);
    }

    /// `R`: goes back to the previous successful deploy of the selected cell.
    fn rollback(&self, ctx: &mut Ctx) {
        let (row, col) = (self.row, self.col);
        let Some(pipeline) = self.apps.get(row).and_then(|a| a.pipelines.get(col)?.clone()) else {
            return ctx.toast(Level::Info, "No pipeline here to roll back");
        };
        if matches!(self.cells[row][col], State::Login) {
            return ctx.toast(Level::Warn, format!("Sign in to {} first (Enter)", self.columns[col].env));
        }
        self.hand_off(col, Intent::Rollback { pipeline }, ctx);
    }

    /// Opens Pipelines in the column's environment with `intent`.
    fn hand_off(&self, col: usize, intent: Intent, ctx: &mut Ctx) {
        let env = &self.columns[col].env;
        if *env != ctx.env().name {
            ctx.switch_env(env.clone());
        }
        ctx.jump_intent(intent);
    }

    fn render_cell(&self, row: usize, col: usize, view: &View, width: usize) -> Vec<Span<'static>> {
        match &self.cells[row][col] {
            State::Missing => vec![Span::styled("—", theme::muted())],
            State::Loading => vec![Span::styled(theme::spinner(view.tick), theme::muted())],
            State::Login => vec![Span::styled("sign in · Enter", Style::new().fg(theme::WARN))],
            State::Failed(_) => vec![Span::styled("! error", Style::new().fg(theme::ERR))],
            State::Ready(cell) => {
                let (symbol, style) = match cell.activity().map(|e| Status::parse(&e.status)) {
                    Some(s) if s.is_running() => (theme::spinner(view.tick), Style::new().fg(theme::RUNNING)),
                    Some(Status::Failed) => ("✗", Style::new().fg(theme::ERR)),
                    Some(Status::Stopped) => ("■", Style::new().fg(theme::WARN)),
                    _ if cell.deployed().is_some() => ("✓", Style::new().fg(theme::OK)),
                    _ => ("○", theme::muted()),
                };
                let Some(deployed) = cell.deployed() else {
                    return vec![Span::styled(format!("{symbol} "), style), Span::styled("no success yet", theme::muted())];
                };
                let rev = pipeline::revision(deployed, &[]);
                let when = deployed.last_update_time.map(fmt_ago).unwrap_or_default().replace(" ago", "");
                let (mark, mark_style) = match self.drift_of(row, col).map(|(_, d)| d) {
                    Some(Drift::Behind(n)) => (format!(" ↓{n}"), Style::new().fg(theme::WARN)),
                    Some(Drift::Differs) => (" ≠".to_string(), Style::new().fg(theme::WARN)),
                    _ => (String::new(), Style::new()),
                };
                let text = ellipsize(&format!("{} · {when}", rev.commit.unwrap_or_default()), width.saturating_sub(3 + mark.chars().count()));
                vec![Span::styled(format!("{symbol} "), style), Span::raw(text), Span::styled(mark, mark_style.bold())]
            }
        }
    }

    fn render_table(&mut self, f: &mut Frame, area: Rect, view: &View, current_env: &str) {
        self.table_area = area;
        let age = self.refreshed.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let state = if self.in_flight > 0 { format!("{} refreshing", theme::spinner(view.tick)) } else { format!("refreshed {age}s ago") };
        let block = widgets::panel(" What is deployed where ", true).title_top(Line::styled(format!(" {state} "), theme::muted()).right_aligned());
        let inner = block.inner(area);
        f.render_widget(block, area);

        let app_width = self.apps.iter().map(|a| a.name.chars().count()).max().unwrap_or(4).clamp(8, 36) as u16 + 2;
        let col_width = (inner.width.saturating_sub(app_width) / self.columns.len().max(1) as u16).max(14);
        self.app_width = app_width;
        self.col_width = col_width;

        let mut header = vec![Span::styled(format!("{:<w$}", "", w = app_width as usize), Style::new())];
        for c in &self.columns {
            let marker = if c.env == current_env { "● " } else { "" };
            header.push(Span::styled(
                format!("{:<w$}", format!("{marker}{}", c.env), w = col_width as usize),
                Style::new().fg(theme::env_color(&c.env)).bold(),
            ));
        }
        let mut lines = vec![Line::from(header), Line::raw("")];
        for (row, app) in self.apps.iter().enumerate() {
            let mut spans =
                vec![Span::styled(format!("{:<w$}", ellipsize(&app.name, app_width as usize - 2), w = app_width as usize), theme::bold())];
            for col in 0..self.columns.len() {
                let cell = self.render_cell(row, col, view, col_width as usize);
                let used: usize = cell.iter().map(|s| s.width()).sum();
                let selected = row == self.row && col == self.col;
                let pad = Span::raw(" ".repeat((col_width as usize).saturating_sub(used)));
                for mut s in cell.into_iter().chain([pad]) {
                    if selected {
                        s = s.patch_style(theme::selected());
                    }
                    spans.push(s);
                }
            }
            lines.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect) {
        let (Some(app), Some(column)) = (self.apps.get(self.row), self.columns.get(self.col)) else { return };
        let pipeline = app.pipelines[self.col].as_deref();
        let title = format!(" {} · {} ", app.name, column.env);
        let kw = 10;
        // Finished runs by when they ended; running ones by when they started.
        let describe = |e: &crate::aws::codepipeline::ExecutionSummary| {
            let rev = pipeline::revision(e, &[]);
            let when = if Status::parse(&e.status).is_running() {
                e.start_time.map(|t| format!(" · started {}", fmt_ago(t)))
            } else {
                e.last_update_time.or(e.start_time).map(|t| format!(" · {} · {}", fmt_ago(t), fmt_utc(t)))
            };
            format!("{}{}", rev.one_line(), when.unwrap_or_default())
        };
        let mut lines = Vec::new();
        match (&self.cells[self.row][self.col], pipeline) {
            (State::Missing, _) | (_, None) => {
                lines.push(Line::styled(format!("{} has no pipeline in {}.", app.name, column.env), theme::muted()));
                lines.push(Line::styled(
                    "Add one with n in Pipelines, or group differently named pipelines with `app:` in the config.",
                    theme::muted(),
                ));
            }
            (State::Loading, Some(p)) => lines.push(widgets::kv("Pipeline", p.to_string(), kw)),
            (State::Login, Some(_)) => {
                lines.push(Line::styled(
                    format!("The AWS session of {} (profile {}) expired.", column.env, effective_profile(&column.profile)),
                    Style::new().fg(theme::WARN),
                ));
                lines.push(Line::styled("Enter signs in; the matrix refreshes afterwards.", theme::muted()));
            }
            (State::Failed(e), Some(p)) => {
                lines.push(widgets::kv("Pipeline", p.to_string(), kw));
                lines.push(Line::styled(e.clone(), Style::new().fg(theme::ERR)));
            }
            (State::Ready(cell), Some(p)) => {
                lines.push(widgets::kv("Pipeline", p.to_string(), kw));
                match cell.deployed() {
                    Some(d) => lines.push(widgets::kv("Deployed", describe(d), kw)),
                    None => lines.push(widgets::kv("Deployed", "no successful run in the recent history".to_string(), kw)),
                }
                if let Some(a) = cell.activity() {
                    let s = Status::parse(&a.status);
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:<kw$} ", "Now"), theme::muted()),
                        Span::styled(format!("{} {} ", s.symbol(), s.label()), theme::status_style(s)),
                        Span::raw(describe(a)),
                    ]));
                }
                if let Some((left, d)) = self.drift_of(self.row, self.col) {
                    let left_env = &self.columns[left].env;
                    let theirs = self.cell(self.row, left).and_then(Cell::deployed).map(describe).unwrap_or_default();
                    let text = match d {
                        Drift::InSync => format!("same commit as {left_env}"),
                        Drift::Behind(n) => format!(
                            "{n} deploy{} behind {left_env}, which runs {theirs} · g compares the commits · p promotes it",
                            if n == 1 { "" } else { "s" }
                        ),
                        Drift::Differs => format!("differs from {left_env} ({theirs}); not in its recent history · g compares · p deploys {left_env}'s"),
                    };
                    let style = if d == Drift::InSync { Style::new().fg(theme::OK) } else { Style::new().fg(theme::WARN) };
                    lines.push(Line::from(vec![Span::styled(format!("{:<kw$} ", format!("vs {left_env}")), theme::muted()), Span::styled(text, style)]));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled("Enter opens it in Pipelines · R rolls it back to its previous deploy", theme::muted()));
            }
        }
        f.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: true }).block(widgets::panel(title, false)),
            area,
        );
    }
}

impl Module for Matrix {
    fn title(&self) -> &'static str {
        "Matrix"
    }

    fn description(&self) -> &'static str {
        "What is deployed where · commit per environment, drift, what is deploying"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Matrix::new();
        let (envs, apps) = crate::matrix::layout(ctx.config());
        self.columns = envs
            .iter()
            .map(|name| {
                let e = &ctx.config().environments[name];
                Column { env: name.clone(), profile: e.profile.clone(), aws: Aws::new(&e.profile, &e.region) }
            })
            .collect();
        self.cells = apps
            .iter()
            .map(|a| a.pipelines.iter().map(|p| if p.is_some() { State::Loading } else { State::Missing }).collect())
            .collect();
        self.apps = apps;
        self.current_env = ctx.env().name.clone();
        self.col = envs.iter().position(|e| *e == self.current_env).unwrap_or(0);
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if self.refreshed.is_none() {
            self.refresh(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let (rows, cols) = (self.apps.len(), self.columns.len());
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.row = self.row.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.row = (self.row + 1).min(rows.saturating_sub(1)),
            KeyCode::Left | KeyCode::Char('h') => self.col = self.col.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => self.col = (self.col + 1).min(cols.saturating_sub(1)),
            KeyCode::Enter => {
                let Some(column) = self.columns.get(self.col) else { return Handled::Yes };
                match (&self.cells[self.row][self.col], &self.apps[self.row].pipelines[self.col]) {
                    (State::Login, _) => ctx.login(column.profile.clone()),
                    (_, Some(pipeline)) => {
                        if column.env != ctx.env().name {
                            ctx.switch_env(column.env.clone());
                        }
                        ctx.jump("Pipelines", pipeline.clone());
                    }
                    _ => {}
                }
            }
            KeyCode::Char('p') => self.promote(ctx),
            KeyCode::Char('R') => self.rollback(ctx),
            KeyCode::Char('g') => match self.compare_link() {
                Ok(url) => {
                    if !crate::platform::open_url(&url) {
                        ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                    }
                }
                Err(why) => ctx.toast(Level::Info, why),
            },
            KeyCode::Char('r') => {
                if self.in_flight == 0 {
                    self.refresh(ctx);
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        let MouseEventKind::Down(MouseButton::Left) = event.kind else { return Handled::No };
        let area = self.table_area;
        // Inside the border: a header row and a blank row, then one row per app.
        let (x, y) = (event.column.saturating_sub(area.x + 2), event.row.saturating_sub(area.y + 3));
        if !area.contains((event.column, event.row).into()) || event.row < area.y + 3 || x < self.app_width {
            return Handled::No;
        }
        let (row, col) = (y as usize, ((x - self.app_width) / self.col_width.max(1)) as usize);
        if row < self.apps.len() && col < self.columns.len() {
            self.row = row;
            self.col = col;
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, _ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        let Msg::Cell { row, col, result } = *msg;
        self.in_flight = self.in_flight.saturating_sub(1);
        if let Some(slot) = self.cells.get_mut(row).and_then(|r| r.get_mut(col)) {
            *slot = match result {
                Ok(cell) => State::Ready(cell),
                Err(e) if e.is_login_required() => State::Login,
                Err(e) => State::Failed(e.short()),
            };
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if active && self.in_flight == 0 && self.refreshed.is_some_and(|t| t.elapsed() >= REFRESH_EVERY) {
            self.refresh(ctx);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if self.apps.is_empty() {
            let text = vec![
                Line::raw(""),
                Line::styled("No pipelines configured.", theme::bold()).centered(),
                Line::styled("Add them with n in Pipelines; the matrix compares them across environments.", theme::muted()).centered(),
            ];
            f.render_widget(Paragraph::new(text).block(widgets::panel(" What is deployed where ", true)), area);
            return;
        }
        let table_height = (self.apps.len() as u16 + 4).min(area.height.saturating_sub(7)).max(5);
        let [table, detail] = area.layout(&Layout::vertical([Constraint::Length(table_height), Constraint::Fill(1)]));
        let current = self.current_env.clone();
        self.render_table(f, table, view, &current);
        self.render_detail(f, detail);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("↑↓←→", "move"), ("Enter", "open / sign in"), ("p", "promote"), ("R", "rollback"), ("g", "compare commits"), ("r", "refresh")]
    }

    fn animating(&self) -> bool {
        self.in_flight > 0
            || self.cells.iter().flatten().any(|c| match c {
                State::Ready(cell) => cell.activity().is_some_and(|e| Status::parse(&e.status).is_running()),
                State::Loading => true,
                _ => false,
            })
    }
}

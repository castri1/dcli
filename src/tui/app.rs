//! The app shell: header with modules and environment, footer with key hints, toasts,
//! global overlays (module palette, environment picker, help, SSO login) and event routing.
//!
//! The header lists the modules while they fit; past that it shows only the current one,
//! and `:` (a searchable palette) reaches any module or environment.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::module::{AppEvent, Ctx, Env, Handled, Level, Module, Payload, Shared, View};
use super::picker::{Item, Outcome, Picker};
use super::{theme, widgets};
use crate::aws::Aws;
use crate::aws::credentials::{effective_profile, sso_login_command};
use crate::config::{Config, Loaded};
use crate::util::now_secs;

/// Pseudo module index for the app's own background jobs.
const APP: usize = usize::MAX;

enum LoginMsg {
    Line(String),
    Done(Result<(), String>),
}

/// A newer dcli release exists (tag).
struct UpdateAvailable(String);

/// The resources of an environment, listed for the `:` palette.
struct CatalogLoaded {
    env: String,
    resources: Vec<crate::catalog::Resource>,
    listed: usize,
}

/// How long the resources listed for the palette are reused.
const CATALOG_TTL: Duration = Duration::from_secs(300);

/// What a palette entry does.
#[derive(Clone)]
enum Jump {
    Module(usize),
    Env(String),
    /// Open a resource: the module's title and what to pass to its `open`.
    Resource(&'static str, String),
}

enum Overlay {
    Help,
    /// Module palette (`:`, with modules and resources) or environment picker (`e`).
    Palette { picker: Picker, jumps: Vec<Jump>, modules: bool },
    Login { profile: String, lines: Vec<String>, error: Option<String> },
}

pub struct App {
    modules: Vec<Box<dyn Module>>,
    active: usize,
    activated: Vec<bool>,
    shared: Shared,
    overlay: Option<Overlay>,
    /// Resources listed per environment for the palette, and when.
    catalogs: HashMap<String, (Instant, Vec<crate::catalog::Resource>)>,
    catalog_loading: HashSet<String>,
    started: Instant,
    tab_hits: Vec<(Rect, usize)>,
    login_running: bool,
    login_profile: String,
    pub quit: bool,
}

fn make_env(config: &Config, name: &str) -> Arc<Env> {
    let e = config.environments.get(name).cloned().unwrap_or_default();
    Arc::new(Env {
        name: name.to_string(),
        aws: Aws::new(&e.profile, &e.region),
        profile: e.profile,
        region: e.region,
        pipelines: e.pipelines,
    })
}

fn toast_ttl(level: Level) -> Duration {
    match level {
        Level::Error => Duration::from_secs(8),
        _ => Duration::from_secs(5),
    }
}

impl App {
    pub fn new(
        loaded: Loaded,
        env_name: &str,
        tx: std::sync::mpsc::Sender<AppEvent>,
        modules: Vec<Box<dyn Module>>,
    ) -> App {
        let env = make_env(&loaded.config, env_name);
        let count = modules.len();
        let mut app = App {
            modules,
            active: 0,
            activated: vec![false; count],
            shared: Shared {
                env,
                loaded,
                tx,
                generation: 0,
                toasts: Vec::new(),
                jump: None,
                switch_env: None,
                login: None,
            },
            overlay: None,
            catalogs: HashMap::new(),
            catalog_loading: HashSet::new(),
            started: Instant::now(),
            tab_hits: Vec::new(),
            login_running: false,
            login_profile: String::new(),
            quit: false,
        };
        app.reset_modules();
        let tx = app.shared.tx.clone();
        std::thread::spawn(move || {
            if let Some(tag) = crate::update::newer_version_available() {
                let _ = tx.send(AppEvent::Task { module: APP, generation: 0, payload: Box::new(UpdateAvailable(tag)) });
            }
        });
        app
    }

    fn with_module<R>(&mut self, i: usize, f: impl FnOnce(&mut dyn Module, &mut Ctx) -> R) -> R {
        let mut ctx = Ctx { shared: &mut self.shared, module: i };
        f(self.modules[i].as_mut(), &mut ctx)
    }

    /// New generation: in-flight results of the old one are discarded.
    fn reset_modules(&mut self) {
        self.shared.generation += 1;
        self.activated.iter_mut().for_each(|a| *a = false);
        for i in 0..self.modules.len() {
            self.with_module(i, |m, ctx| m.reset(ctx));
        }
        self.select_module(self.active);
    }

    fn select_module(&mut self, i: usize) {
        if i >= self.modules.len() {
            return;
        }
        self.active = i;
        self.activated[i] = true;
        self.with_module(i, |m, ctx| m.activate(ctx));
    }

    fn switch_env(&mut self, name: &str) {
        if name == self.shared.env.name {
            return;
        }
        self.shared.env = make_env(&self.shared.loaded.config, name);
        self.reset_modules();
        let env = self.shared.env.clone();
        self.shared.toasts.push(super::module::Toast {
            level: Level::Info,
            text: format!("Environment: {} ({} · {})", env.name, effective_profile(&env.profile), env.region),
            at: Instant::now(),
        });
    }

    pub fn animating(&self) -> bool {
        self.modules[self.active].animating() || !self.shared.toasts.is_empty() || self.login_running
    }

    pub fn tick(&mut self) {
        self.shared.toasts.retain(|t| t.at.elapsed() < toast_ttl(t.level));
        for i in 0..self.modules.len() {
            let active = i == self.active;
            self.with_module(i, |m, ctx| m.tick(ctx, active));
        }
    }

    pub fn mouse_wanted(&self) -> bool {
        self.modules[self.active].wants_mouse()
    }

    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Input(Event::Key(key)) if key.kind != KeyEventKind::Release => self.key(key),
            AppEvent::Input(Event::Mouse(m)) => self.mouse(m),
            AppEvent::Input(Event::Paste(text)) => match &mut self.overlay {
                Some(Overlay::Palette { picker, .. }) => picker.paste(&text),
                Some(_) => {}
                None => {
                    let active = self.active;
                    self.with_module(active, |m, ctx| m.paste(&text, ctx));
                }
            },
            AppEvent::Input(_) => {}
            AppEvent::Task { module: APP, payload, .. } => self.app_task(payload),
            AppEvent::Task { module, generation, payload } => {
                if generation == self.shared.generation && module < self.modules.len() {
                    self.with_module(module, |m, ctx| m.task(payload, ctx));
                }
            }
        }
        if let Some(name) = self.shared.switch_env.take() {
            self.switch_env(&name);
        }
        if let Some((title, target)) = self.shared.jump.take()
            && let Some(i) = self.modules.iter().position(|m| m.title() == title)
        {
            self.select_module(i);
            self.with_module(i, |m, ctx| m.open(&target, ctx));
        }
        if let Some(profile) = self.shared.login.take() {
            self.start_login_for(profile);
        }
    }

    fn key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.overlay.is_some() {
            self.overlay_key(key);
            return;
        }
        let active = self.active;
        if self.modules[active].capturing() {
            self.with_module(active, |m, ctx| m.key(key, ctx));
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.overlay = Some(Overlay::Help),
            KeyCode::Char(':') => self.open_palette(true),
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => self.open_palette(true),
            KeyCode::Char('e') => self.open_palette(false),
            KeyCode::Char('L') => self.start_login_for(self.shared.env.profile.clone()),
            KeyCode::Tab => self.select_module((active + 1) % self.modules.len()),
            KeyCode::BackTab => self.select_module((active + self.modules.len() - 1) % self.modules.len()),
            KeyCode::Char(c @ '1'..='9') => self.select_module(c as usize - '1' as usize),
            _ => {
                self.with_module(active, |m, ctx| m.key(key, ctx));
            }
        }
    }

    fn overlay_key(&mut self, key: KeyEvent) {
        let Some(overlay) = &mut self.overlay else { return };
        match overlay {
            Overlay::Help => self.overlay = None,
            Overlay::Login { .. } => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    self.overlay = None;
                }
            }
            Overlay::Palette { picker, jumps, .. } => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.overlay = None,
                Outcome::Done(chosen) => {
                    let jump = chosen.first().and_then(|&i| jumps.get(i)).cloned();
                    self.overlay = None;
                    match jump {
                        Some(Jump::Module(i)) => self.select_module(i),
                        Some(Jump::Env(name)) => self.switch_env(&name),
                        Some(Jump::Resource(module, target)) => {
                            if let Some(i) = self.modules.iter().position(|m| m.title() == module) {
                                self.select_module(i);
                                self.with_module(i, |m, ctx| m.open(&target, ctx));
                            }
                        }
                        None => {}
                    }
                }
            },
        }
    }

    /// `:` lists modules, environments and (as you type) the environment's resources; `e`
    /// only environments.
    fn open_palette(&mut self, with_modules: bool) {
        if with_modules {
            self.list_resources();
        }
        let (items, jumps) = self.palette_items(with_modules);
        let cursor = if with_modules {
            self.active
        } else {
            self.shared.loaded.config.environments.get_index_of(&self.shared.env.name).unwrap_or(0)
        };
        let (title, submit) = if with_modules { ("Go to", "go") } else { ("Environment", "switch") };
        let picker = Picker::new(title, submit, items, false).with_cursor(cursor);
        self.overlay = Some(Overlay::Palette { picker, jumps, modules: with_modules });
    }

    fn palette_items(&self, with_modules: bool) -> (Vec<Item>, Vec<Jump>) {
        let mut items = Vec::new();
        let mut jumps = Vec::new();
        if with_modules {
            for (i, m) in self.modules.iter().enumerate() {
                items.push(Item::new(m.title(), m.description()).current(i == self.active));
                jumps.push(Jump::Module(i));
            }
        }
        let files = crate::aws::profile::AwsFiles::load();
        for (name, e) in &self.shared.loaded.config.environments {
            let profile = effective_profile(&e.profile);
            let account = files.profile(&profile).and_then(|p| p.get("sso_account_id").cloned());
            let mut detail = format!("{profile} · {}", e.region);
            if let Some(a) = account {
                detail.push_str(&format!(" · {a}"));
            }
            if with_modules {
                detail = format!("environment · {detail}");
            }
            items.push(
                Item::new(name.as_str(), detail).color(theme::env_color(name)).current(*name == self.shared.env.name),
            );
            jumps.push(Jump::Env(name.clone()));
        }
        if with_modules {
            match self.catalogs.get(&self.shared.env.name) {
                Some((_, resources)) => {
                    for r in resources {
                        items.push(Item::new(r.name.as_str(), r.detail.as_str()).on_search());
                        jumps.push(Jump::Resource(r.module, r.target.clone()));
                    }
                }
                None if self.catalog_loading.contains(&self.shared.env.name) => {
                    items.push(Item::new("…", "listing this environment's resources").on_search());
                    jumps.push(Jump::Module(self.active));
                }
                None => {}
            }
        }
        (items, jumps)
    }

    /// Lists the current environment's resources for the palette in the background, unless
    /// a recent list exists.
    fn list_resources(&mut self) {
        let env = self.shared.env.clone();
        let fresh = self.catalogs.get(&env.name).is_some_and(|(at, _)| at.elapsed() < CATALOG_TTL);
        if fresh || !self.catalog_loading.insert(env.name.clone()) {
            return;
        }
        let tx = self.shared.tx.clone();
        let pipelines: Vec<String> = env.pipelines.iter().map(|p| p.name.clone()).collect();
        std::thread::spawn(move || {
            let (resources, listed) = crate::catalog::list(&env.aws, &pipelines);
            let loaded = CatalogLoaded { env: env.name.clone(), resources, listed };
            let _ = tx.send(AppEvent::Task { module: APP, generation: 0, payload: Box::new(loaded) });
        });
    }

    fn mouse(&mut self, event: MouseEvent) {
        if self.overlay.is_some() {
            return;
        }
        if let MouseEventKind::Down(MouseButton::Left) = event.kind
            && let Some(&(_, i)) = self
                .tab_hits
                .iter()
                .find(|(r, _)| r.contains((event.column, event.row).into()))
        {
            self.select_module(i);
            return;
        }
        let active = self.active;
        if let Handled::No = self.with_module(active, |m, ctx| m.mouse(event, ctx)) {}
    }

    // ---- SSO login --------------------------------------------------------------

    /// SSO sign-in for `profile` (the current environment's, or another one's).
    fn start_login_for(&mut self, profile: String) {
        self.overlay = Some(Overlay::Login { profile: effective_profile(&profile), lines: Vec::new(), error: None });
        if self.login_running {
            return;
        }
        self.login_running = true;
        self.login_profile = profile.clone();
        let tx = self.shared.tx.clone();
        std::thread::spawn(move || {
            let send = |m: LoginMsg| tx.send(AppEvent::Task { module: APP, generation: 0, payload: Box::new(m) });
            let child = sso_login_command(&profile)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let _ = send(LoginMsg::Done(Err(
                        "AWS CLI not found. Install it: https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html".into(),
                    )));
                    return;
                }
                Err(e) => {
                    let _ = send(LoginMsg::Done(Err(format!("cannot run aws: {e}"))));
                    return;
                }
            };
            if let Some(err) = child.stderr.take() {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for line in BufReader::new(err).lines().map_while(Result::ok) {
                        let _ = tx.send(AppEvent::Task {
                            module: APP,
                            generation: 0,
                            payload: Box::new(LoginMsg::Line(line)),
                        });
                    }
                });
            }
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let _ = send(LoginMsg::Line(line));
                }
            }
            let result = match child.wait() {
                Ok(s) if s.success() => Ok(()),
                Ok(s) => Err(format!("aws sso login exited with {s}")),
                Err(e) => Err(e.to_string()),
            };
            let _ = send(LoginMsg::Done(result));
        });
    }

    fn app_task(&mut self, payload: Payload) {
        let payload = match payload.downcast::<CatalogLoaded>() {
            Ok(loaded) => {
                let CatalogLoaded { env, resources, listed } = *loaded;
                self.catalog_loading.remove(&env);
                // Nothing could be listed (an expired session): try again next time.
                if listed > 0 {
                    self.catalogs.insert(env, (Instant::now(), resources));
                }
                if matches!(self.overlay, Some(Overlay::Palette { modules: true, .. })) {
                    let (items, new_jumps) = self.palette_items(true);
                    if let Some(Overlay::Palette { picker, jumps, .. }) = &mut self.overlay {
                        picker.set_items(items);
                        *jumps = new_jumps;
                    }
                }
                return;
            }
            Err(other) => other,
        };
        let payload = match payload.downcast::<UpdateAvailable>() {
            Ok(update) => {
                let text = format!(
                    "dcli {} is available (you have {}). Quit and run: {} update",
                    update.0.trim_start_matches('v'),
                    crate::update::CURRENT,
                    crate::util::program()
                );
                let mut ctx = Ctx { shared: &mut self.shared, module: APP };
                ctx.toast(Level::Info, text);
                return;
            }
            Err(other) => other,
        };
        let Ok(msg) = payload.downcast::<LoginMsg>() else { return };
        match *msg {
            LoginMsg::Line(line) => {
                if let Some(Overlay::Login { lines, .. }) = &mut self.overlay {
                    let line = crate::util::strip_ansi(&line);
                    if !line.trim().is_empty() {
                        lines.push(line);
                    }
                }
            }
            LoginMsg::Done(Ok(())) => {
                self.login_running = false;
                if matches!(self.overlay, Some(Overlay::Login { .. })) {
                    self.overlay = None;
                }
                // Credentials are cached per profile: drop the signed-in one's.
                crate::aws::CredentialProvider::for_profile(&self.login_profile).invalidate();
                self.shared.env.aws.reset_credentials();
                let mut ctx = Ctx { shared: &mut self.shared, module: APP };
                ctx.toast(Level::Success, format!("Signed in to AWS ({})", effective_profile(&self.login_profile)));
                self.reset_modules();
            }
            LoginMsg::Done(Err(e)) => {
                self.login_running = false;
                match &mut self.overlay {
                    Some(Overlay::Login { error, .. }) => *error = Some(e),
                    _ => {
                        let mut ctx = Ctx { shared: &mut self.shared, module: APP };
                        ctx.toast(Level::Error, e);
                    }
                }
            }
        }
    }

    // ---- rendering --------------------------------------------------------------

    pub fn render(&mut self, f: &mut Frame) {
        let view = View { tick: (self.started.elapsed().as_millis() / 100) as usize, now: now_secs() };
        let [header, body, footer] =
            f.area().layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]));
        self.render_header(f, header);
        self.modules[self.active].render(f, body, &view);
        self.render_footer(f, footer);
        self.render_toasts(f, body);
        self.render_overlay(f, body, &view);
    }

    fn render_header(&mut self, f: &mut Frame, area: Rect) {
        let env = &self.shared.env;
        let badge = Line::from(vec![
            Span::styled(format!(" ● {} ", env.name), Style::new().fg(theme::env_color(&env.name)).bold()),
            Span::styled(format!("{} · {} ", effective_profile(&env.profile), env.region), theme::muted()),
        ])
        .right_aligned();

        let logo = format!(" ◆ {} ", crate::util::program());
        let active = Style::new().fg(ratatui::style::Color::Black).bg(theme::ACCENT).bold();
        let labels: Vec<String> =
            self.modules.iter().enumerate().map(|(i, m)| format!(" {} {} ", i + 1, m.title())).collect();
        let badges: Vec<Option<(String, ratatui::style::Color)>> = self.modules.iter().map(|m| m.badge()).collect();
        let badge_width = |b: &Option<(String, ratatui::style::Color)>| b.as_ref().map_or(0, |(t, _)| t.chars().count() + 2);
        let tabs_width: usize = labels.iter().zip(&badges).map(|(l, b)| l.chars().count() + badge_width(b) + 1).sum();
        let fits = logo.chars().count() + 1 + tabs_width + badge.width() + 2 <= area.width as usize;

        let mut spans = vec![Span::styled(logo.clone(), theme::accent().bold()), Span::raw(" ")];
        let mut x = area.x + logo.chars().count() as u16 + 1;
        self.tab_hits.clear();
        if fits {
            for (i, (label, badge)) in labels.into_iter().zip(&badges).enumerate() {
                let width = (label.chars().count() + badge_width(badge)) as u16;
                self.tab_hits.push((Rect::new(x, area.y, width, 1), i));
                spans.push(Span::styled(label, if i == self.active { active } else { theme::muted() }));
                if let Some((text, color)) = badge {
                    spans.push(Span::styled(format!(" {text} "), Style::new().fg(*color).bold()));
                }
                spans.push(Span::raw(" "));
                x += width + 1;
            }
        } else {
            // Too many modules (or a narrow terminal): the current one, the palette key, and
            // the modules with something to say (clickable).
            let current = format!(" {} ", self.modules[self.active].title());
            x += (current.chars().count() + "  : modules".len()) as u16;
            spans.push(Span::styled(current, active));
            spans.push(Span::styled("  : modules", theme::muted()));
            let env_width = badge.width() as u16;
            for (i, count) in badges.iter().enumerate() {
                let Some((text, color)) = count else { continue };
                let label = format!("   {} {text}", self.modules[i].title());
                let width = label.chars().count() as u16;
                if x + width + env_width > area.right() {
                    break;
                }
                self.tab_hits.push((Rect::new(x + 3, area.y, width - 3, 1), i));
                spans.push(Span::styled(label, Style::new().fg(*color).bold()));
                x += width;
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        f.render_widget(Paragraph::new(badge), area);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let hints = self.modules[self.active].hints();
        let mut line = widgets::hints_line(&hints);
        line.spans.insert(0, Span::raw(" "));
        let used = line.width();
        f.render_widget(Paragraph::new(line), area);
        let full = [(":", "go to"), ("e", "env"), ("L", "login"), ("?", "help"), ("q", "quit")];
        let roomy = used + widgets::hints_line(&full).width() + 4 <= area.width as usize;
        let mut global = widgets::hints_line(if roomy { &full[..] } else { &full[..1] });
        global.spans.insert(0, Span::styled(" │ ", theme::muted()));
        let width = global.width() as u16 + 1;
        if area.width > width + 20 {
            let right = Rect::new(area.right() - width, area.y, width, 1);
            f.render_widget(Clear, right);
            f.render_widget(Paragraph::new(global), right);
        }
    }

    fn render_toasts(&self, f: &mut Frame, area: Rect) {
        let mut bottom = area.bottom().saturating_sub(1);
        for toast in self.shared.toasts.iter().rev().take(3) {
            let (symbol, color) = match toast.level {
                Level::Info => ("i", theme::ACCENT),
                Level::Success => ("✓", theme::OK),
                Level::Warn => ("!", theme::WARN),
                Level::Error => ("✗", theme::ERR),
            };
            let text = crate::util::ellipsize(&toast.text, 120);
            let text_width = text.chars().count() as u16 + 2;
            let width = (text_width + 4).min(area.width.saturating_sub(4).min(72));
            let lines = text_width.div_ceil(width.saturating_sub(4).max(1)).clamp(1, 4);
            let height = lines + 2;
            if bottom < area.y + height {
                break;
            }
            let rect = Rect::new(area.right().saturating_sub(width + 2), bottom - height, width, height);
            f.render_widget(Clear, rect);
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(color))
                .padding(ratatui::widgets::Padding::horizontal(1));
            let body = Line::from(vec![Span::styled(format!("{symbol} "), Style::new().fg(color).bold()), Span::raw(text)]);
            f.render_widget(Paragraph::new(body).wrap(ratatui::widgets::Wrap { trim: true }).block(block), rect);
            bottom -= height;
        }
    }

    fn render_overlay(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let Some(overlay) = &mut self.overlay else { return };
        match overlay {
            Overlay::Help => {
                let global = [
                    (": / Ctrl+K", "go to module/environment"),
                    ("Tab / 1-9", "next module / module n"),
                    ("e", "choose environment"),
                    ("L", "sign in with AWS SSO"),
                    ("?", "this help"),
                    ("q / Ctrl+C", "quit"),
                ];
                let module = self.modules[self.active].hints();
                // One column, or two when the terminal is too short for one.
                let build = |columns: usize| {
                    let rows = |keys: &[(&str, &str)]| -> Vec<Line> {
                        keys.chunks(columns)
                            .map(|chunk| {
                                Line::from(
                                    chunk
                                        .iter()
                                        .flat_map(|(k, d)| {
                                            [Span::styled(format!("  {k:<12}"), theme::key()), Span::raw(format!("{d:<24}"))]
                                        })
                                        .collect::<Vec<_>>(),
                                )
                            })
                            .collect()
                    };
                    let mut lines = vec![widgets::section("Global"), Line::raw("")];
                    lines.extend(rows(&global));
                    lines.extend([Line::raw(""), widgets::section(self.modules[self.active].title()), Line::raw("")]);
                    lines.extend(rows(&module));
                    lines.extend([Line::raw(""), Line::styled("Press any key to close", theme::muted())]);
                    lines
                };
                let mut lines = build(1);
                let mut width = 44;
                if lines.len() as u16 + 2 > area.height {
                    lines = build(2);
                    width = 80;
                }
                let height = lines.len() as u16 + 2;
                widgets::popup(f, widgets::popup_area(area, width, height), "Keys", theme::ACCENT, Text::from(lines));
            }
            Overlay::Palette { picker, .. } => {
                let rect = widgets::popup_area(area, 84, picker.height(area.height.saturating_sub(4)));
                picker.render(f, rect, theme::ACCENT);
            }
            Overlay::Login { profile, lines, error } => {
                let mut text = vec![Line::raw("")];
                match error {
                    Some(e) => text.push(Line::styled(format!("✗ {e}"), Style::new().fg(theme::ERR))),
                    None => text.push(Line::from(vec![
                        Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()),
                        Span::raw("Complete the sign-in in your browser…"),
                    ])),
                }
                text.push(Line::raw(""));
                for l in lines.iter().rev().take(10).rev() {
                    text.push(Line::styled(l.clone(), theme::muted()));
                }
                text.push(Line::raw(""));
                text.push(Line::styled("Esc hides this window; the login keeps running.", theme::muted()));
                let height = text.len() as u16 + 2;
                let title = format!("AWS SSO login · {profile}");
                let color = if error.is_some() { theme::ERR } else { theme::ACCENT };
                widgets::popup(f, widgets::popup_area(area, 90, height), &title, color, Text::from(text));
            }
        }
    }
}

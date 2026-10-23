//! Costs: what each environment's AWS account costs, by service and usage type, against last
//! month and the forecast; Savings Plans and reservations, their use and coverage; and cost
//! changes next to the deploys that came before them. From Cost Explorer (see `crate::costs`):
//! AWS bills each request, so it is read only when asked (`r`), or once a day when the user
//! chose that, and what was read is kept for the Savings module and `dcli costs`.

mod views;

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;

use crate::aws::profile::AwsFiles;
use crate::aws::{self, AwsError};
use crate::costs::analysis::{self, Deploy, Step};
use crate::costs::explorer;
use crate::costs::settings::{self, Refresh};
use crate::costs::{self, ServiceCost, Snapshot, UsageCost, services};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::util::now_secs;

const DAY: f64 = 86_400.0;
/// Days of deploys looked at: as far back as the daily costs go.
const DEPLOY_DAYS: f64 = 62.0;

const READ_CHOICES: &[&str] = &["when I press r", "once a day"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Services,
    Commitments,
}

/// A row of the services table: a service, or one of its usage types once it is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Service(usize),
    /// (service, index in the snapshot's usage types)
    Usage(usize, usize),
}

enum Dialog {
    Setup(Form),
    Read(Form),
}

enum Msg {
    Step(String),
    Read(Box<aws::Result<Snapshot>>),
    Deploys { deploys: Vec<Deploy>, failed: Vec<String> },
}

pub struct Costs {
    env: String,
    profile: String,
    snapshot: Option<Snapshot>,
    /// What the reading in progress is doing.
    reading: Option<String>,
    error: Option<AwsError>,
    tab: Tab,
    rows: Vec<Row>,
    /// Services whose usage types show.
    opened: HashSet<String>,
    list_state: ListState,
    /// The environment's successful deploys, and the pipelines that could not be read.
    deploys: Option<(Vec<Deploy>, Vec<String>)>,
    deploys_loading: bool,
    steps: Vec<Step>,
    dialog: Option<Dialog>,
    /// The first opening offered the settings already.
    offered_setup: bool,
    detail_scroll: u16,
    commitments_scroll: u16,
    list_area: Rect,
    /// Other environments in the same AWS account.
    shared_with: Vec<String>,
}

impl Costs {
    pub fn new() -> Costs {
        Costs {
            env: String::new(),
            profile: String::new(),
            snapshot: None,
            reading: None,
            error: None,
            tab: Tab::Services,
            rows: Vec::new(),
            opened: HashSet::new(),
            list_state: ListState::default(),
            deploys: None,
            deploys_loading: false,
            steps: Vec::new(),
            dialog: None,
            offered_setup: false,
            detail_scroll: 0,
            commitments_scroll: 0,
            list_area: Rect::default(),
            shared_with: Vec::new(),
        }
    }

    // ---- rows and selection -----------------------------------------------------------------

    fn rebuild(&mut self) {
        let keep = self.selected_key();
        self.rows.clear();
        if let Some(s) = &self.snapshot {
            for (i, svc) in s.services.iter().enumerate() {
                self.rows.push(Row::Service(i));
                if self.opened.contains(&svc.service) {
                    self.rows.extend(s.usage.iter().enumerate().filter(|(_, u)| u.service == svc.service).map(|(j, _)| Row::Usage(i, j)));
                }
            }
        }
        let index = keep.and_then(|k| self.rows.iter().position(|r| self.key_of(*r) == k));
        let index = index.or(self.list_state.selected()).unwrap_or(0).min(self.rows.len().saturating_sub(1));
        self.list_state.select(if self.rows.is_empty() { None } else { Some(index) });
    }

    /// A row's identity across readings: the service, and the usage type.
    fn key_of(&self, row: Row) -> (String, String) {
        let Some(s) = &self.snapshot else { return Default::default() };
        match row {
            Row::Service(i) => (s.services.get(i).map(|x| x.service.clone()).unwrap_or_default(), String::new()),
            Row::Usage(i, j) => (
                s.services.get(i).map(|x| x.service.clone()).unwrap_or_default(),
                s.usage.get(j).map(|u| u.usage_type.clone()).unwrap_or_default(),
            ),
        }
    }

    fn selected_row(&self) -> Option<Row> {
        self.rows.get(self.list_state.selected()?).copied()
    }

    fn selected_key(&self) -> Option<(String, String)> {
        self.selected_row().map(|r| self.key_of(r))
    }

    fn selected_service(&self) -> Option<&ServiceCost> {
        let s = self.snapshot.as_ref()?;
        match self.selected_row()? {
            Row::Service(i) | Row::Usage(i, _) => s.services.get(i),
        }
    }

    fn selected_usage(&self) -> Option<&UsageCost> {
        match self.selected_row()? {
            Row::Usage(_, j) => self.snapshot.as_ref()?.usage.get(j),
            Row::Service(_) => None,
        }
    }

    fn step_of(&self, service: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.service == service)
    }

    fn select(&mut self, index: usize) {
        if !self.rows.is_empty() {
            self.list_state.select(Some(index.min(self.rows.len() - 1)));
            self.detail_scroll = 0;
        }
    }

    fn move_by(&mut self, delta: i64) {
        let i = self.list_state.selected().unwrap_or(0) as i64;
        self.select((i + delta).max(0) as usize);
    }

    fn toggle(&mut self) {
        let Some(svc) = self.selected_service().map(|s| s.service.clone()) else { return };
        if !self.opened.remove(&svc) {
            self.opened.insert(svc.clone());
        }
        self.rebuild();
        // Stay on the service, not on one of its usage types.
        if let Some(i) = self.rows.iter().position(|r| self.key_of(*r) == (svc.clone(), String::new())) {
            self.select(i);
        }
    }

    /// Selects the service named `target` (Cost Explorer's name, or the dcli module's).
    fn find(&mut self, target: &str) {
        let Some(s) = &self.snapshot else { return };
        let lower = target.to_lowercase();
        let found = s
            .services
            .iter()
            .position(|x| x.service.to_lowercase() == lower || services::module(&x.service).is_some_and(|m| m.to_lowercase() == lower));
        if let Some(i) = found.and_then(|i| self.rows.iter().position(|r| *r == Row::Service(i))) {
            self.tab = Tab::Services;
            self.select(i);
        }
    }

    // ---- reading ---------------------------------------------------------------------------

    fn start_read(&mut self, ctx: &mut Ctx, auto: bool) {
        if self.reading.is_some() || explorer::is_reading(&self.env) {
            return ctx.toast(Level::Info, "Already reading the costs");
        }
        self.reading = Some("starting".into());
        self.error = None;
        if auto {
            ctx.toast(Level::Info, format!("Reading the costs of {} (once a day): {} to Cost Explorer", self.env, explorer::requests_text(&self.env)));
        }
        let env = ctx.env();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let progress = |step: &str| {
                em.emit(Msg::Step(step.to_string()));
            };
            let result = explorer::read(&env.name, &env.profile, &env.aws, now_secs(), &progress);
            em.emit(Msg::Read(Box::new(result)));
        });
    }

    fn load_deploys(&mut self, ctx: &mut Ctx) {
        let env = ctx.env();
        let names: Vec<String> = env.pipelines.iter().map(|p| p.name.clone()).collect();
        if names.is_empty() {
            self.deploys = Some((Vec::new(), Vec::new()));
            return;
        }
        self.deploys_loading = true;
        ctx.spawn(move || {
            let (deploys, failed) = analysis::deploys(&env.aws, &names, now_secs() - DEPLOY_DAYS * DAY);
            Msg::Deploys { deploys, failed }
        });
    }

    fn recompute_steps(&mut self) {
        self.steps = match (&self.snapshot, &self.deploys) {
            (Some(s), Some((deploys, _))) => analysis::steps(s, deploys, now_secs()),
            _ => Vec::new(),
        };
    }

    /// The other environments of the config whose profile is in the same account.
    fn find_shared(&mut self, ctx: &Ctx) {
        let accounts = settings::load().accounts;
        let account_of = |profile: &str| accounts.get(&aws::credentials::effective_profile(profile)).cloned();
        let mine = self.snapshot.as_ref().map(|s| s.account.clone()).or_else(|| account_of(&self.profile));
        self.shared_with = match mine {
            Some(mine) => ctx
                .config()
                .environments
                .iter()
                .filter(|(name, e)| **name != self.env && account_of(&e.profile).as_deref() == Some(mine.as_str()))
                .map(|(name, _)| name.clone())
                .collect(),
            None => Vec::new(),
        };
    }

    // ---- dialogs ---------------------------------------------------------------------------

    fn setup_form() -> Form {
        let s = settings::load();
        let profiles = AwsFiles::load().profile_names();
        let mut form = Form::new(
            "Costs · how to read them",
            vec![
                Field::text("Reader profile", s.profile.clone().unwrap_or_default()).optional("empty: each environment's own profile"),
                Field::choice("Read", READ_CHOICES, if s.refresh == Refresh::Daily { READ_CHOICES[1] } else { READ_CHOICES[0] }),
            ],
        );
        let listed = if profiles.is_empty() {
            "none in ~/.aws/config".to_string()
        } else {
            let mut names = profiles.iter().take(10).cloned().collect::<Vec<_>>().join(", ");
            if profiles.len() > 10 {
                names.push_str(&format!(" and {} more", profiles.len() - 10));
            }
            names
        };
        form.note = Some(format!(
            "With AWS Organizations, a profile in the management account reads every account's costs, and each \
             environment shows its own account's. Profiles: {listed}. Cost Explorer bills each request: dcli counts \
             them and keeps what it read."
        ));
        form.submit_label("save")
    }

    fn save_setup(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Setup(form)) = &mut self.dialog else { return };
        let profile = form.get("Reader profile").trim().to_string();
        if !profile.is_empty() && !AwsFiles::load().profile_names().contains(&profile) {
            return form.fail(format!("No profile named '{profile}' in ~/.aws/config"));
        }
        let changed = settings::load().reader().unwrap_or("") != profile;
        let refresh = if form.get("Read") == READ_CHOICES[1] { Refresh::Daily } else { Refresh::Manual };
        let s = match settings::update(|s| {
            s.profile = (!profile.is_empty()).then_some(profile);
            s.refresh = refresh;
        }) {
            Ok(s) => s,
            Err(e) => return form.fail(format!("Could not save the settings: {e}")),
        };
        self.dialog = None;
        ctx.toast(Level::Success, format!("Costs are read by {}", explorer::reader_label(&s)));
        if self.snapshot.is_none() || changed {
            self.dialog = Some(Dialog::Read(self.read_form()));
        }
    }

    fn read_form(&self) -> Form {
        let s = settings::load();
        let mut form = Form::new(
            format!("Read the costs of {}", self.env),
            vec![
                Field::fixed("Reader", explorer::reader_label(&s)),
                Field::fixed("Requests", format!("{} to Cost Explorer (more when an answer comes in pages)", explorer::requests_text(&self.env))),
                Field::fixed("This month", format!("{} requests so far", settings::requests_this_month())),
            ],
        )
        .submit_label("read")
        .enter_submits();
        form.note = Some(
            "AWS bills every Cost Explorer request. What is read stays on disk for this module, the Savings module and \
             dcli costs; reading again is only needed when you want newer figures (Cost Explorer updates a few times a day)."
                .into(),
        );
        form
    }

    fn copy_report(&mut self, ctx: &mut Ctx) {
        let Some(s) = &self.snapshot else { return ctx.toast(Level::Info, "Nothing read yet: r reads the costs") };
        let md = analysis::report(&self.env, s, &self.steps);
        if crate::platform::copy_to_clipboard(&md) {
            ctx.toast(Level::Success, "Copied the costs report for an AI agent");
        } else {
            ctx.toast(Level::Error, "Could not copy to the clipboard");
        }
    }

    fn jump_module(&mut self, ctx: &mut Ctx) {
        let Some(svc) = self.selected_service() else { return };
        match services::module(&svc.service) {
            Some(module) => ctx.jump(module, ""),
            None => {
                let text = format!("dcli has no module for {}", svc.service);
                ctx.toast(Level::Info, text);
            }
        }
    }

    fn jump_savings(&mut self, ctx: &mut Ctx) {
        let service = match self.tab {
            Tab::Services => self.selected_service().map(|s| s.service.clone()),
            // The recommendation is a Compute Savings Plan: Lambda is what dcli explores of it.
            Tab::Commitments => Some("AWS Lambda".to_string()),
        };
        match service.as_deref().and_then(services::check) {
            Some(check) => ctx.jump("Savings", format!("service:{check}")),
            None => ctx.jump("Savings", ""),
        }
    }
}

impl Module for Costs {
    fn title(&self) -> &'static str {
        "Costs"
    }

    fn description(&self) -> &'static str {
        "What the account costs · services, usage types, commitments, cost after deploys"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        let env = ctx.env();
        self.env = env.name.clone();
        self.profile = env.profile.clone();
        self.snapshot = costs::cached(&self.env);
        self.reading = None;
        self.error = None;
        self.deploys = None;
        self.deploys_loading = false;
        self.steps.clear();
        self.dialog = None;
        self.opened.clear();
        self.list_state = ListState::default();
        self.detail_scroll = 0;
        self.commitments_scroll = 0;
        self.rebuild();
        self.find_shared(ctx);
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if self.dialog.is_some() || self.reading.is_some() {
            return;
        }
        if !settings::exists() && !self.offered_setup {
            self.offered_setup = true;
            self.dialog = Some(Dialog::Setup(Costs::setup_form()));
            return;
        }
        // Once a day: not while a reading of this environment runs (one started before the
        // environment was switched and back, say), and not again soon after an attempt.
        let read_at = self.snapshot.as_ref().map(|s| s.read_at);
        if !explorer::is_reading(&self.env) && settings::daily_due(&self.env, read_at, now_secs()) {
            settings::daily_attempted(&self.env, now_secs());
            self.start_read(ctx, true);
        }
        if self.snapshot.is_some() && self.deploys.is_none() && !self.deploys_loading {
            self.load_deploys(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some(dialog) = &mut self.dialog {
            let (setup, event) = match dialog {
                Dialog::Setup(form) => (true, form.handle(key)),
                Dialog::Read(form) => (false, form.handle(key)),
            };
            match event {
                FormEvent::Submit if setup => self.save_setup(ctx),
                FormEvent::Submit => {
                    self.dialog = None;
                    self.start_read(ctx, false);
                }
                FormEvent::Cancel => self.dialog = None,
                FormEvent::Pending => {}
            }
            return Handled::Yes;
        }
        match key.code {
            KeyCode::Left | KeyCode::Right => {
                self.tab = if self.tab == Tab::Services { Tab::Commitments } else { Tab::Services };
            }
            KeyCode::Up if self.tab == Tab::Services => self.move_by(-1),
            KeyCode::Down if self.tab == Tab::Services => self.move_by(1),
            KeyCode::Up => self.commitments_scroll = self.commitments_scroll.saturating_sub(1),
            KeyCode::Down => self.commitments_scroll = self.commitments_scroll.saturating_add(1),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(self.rows.len().saturating_sub(1)),
            KeyCode::PageUp if self.tab == Tab::Services => self.detail_scroll = self.detail_scroll.saturating_sub(5),
            KeyCode::PageDown if self.tab == Tab::Services => self.detail_scroll = self.detail_scroll.saturating_add(5),
            KeyCode::PageUp => self.commitments_scroll = self.commitments_scroll.saturating_sub(10),
            KeyCode::PageDown => self.commitments_scroll = self.commitments_scroll.saturating_add(10),
            KeyCode::Enter if self.tab == Tab::Services => self.toggle(),
            KeyCode::Char('o') => self.jump_module(ctx),
            KeyCode::Char('g') => self.jump_savings(ctx),
            KeyCode::Char('r') => {
                if self.reading.is_some() {
                    ctx.toast(Level::Info, "Already reading the costs");
                } else {
                    self.dialog = Some(Dialog::Read(self.read_form()));
                }
            }
            KeyCode::Char('s') => self.dialog = Some(Dialog::Setup(Costs::setup_form())),
            KeyCode::Char('c') => self.copy_report(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() {
            return Handled::No;
        }
        match event.kind {
            MouseEventKind::ScrollDown if self.tab == Tab::Services => self.move_by(1),
            MouseEventKind::ScrollUp if self.tab == Tab::Services => self.move_by(-1),
            MouseEventKind::ScrollDown => self.commitments_scroll = self.commitments_scroll.saturating_add(2),
            MouseEventKind::ScrollUp => self.commitments_scroll = self.commitments_scroll.saturating_sub(2),
            MouseEventKind::Down(_) if self.tab == Tab::Services => {
                let a = self.list_area;
                // The panel's border and the column titles come before the first row.
                if event.column > a.x && event.column < a.x + a.width && event.row >= a.y + 2 && event.row < a.y + a.height.saturating_sub(1) {
                    let index = self.list_state.offset() + usize::from(event.row - a.y - 2);
                    if index < self.rows.len() {
                        if self.list_state.selected() == Some(index) {
                            self.toggle();
                        } else {
                            self.select(index);
                        }
                    }
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Step(step) => self.reading = Some(step),
            Msg::Read(result) => {
                self.reading = None;
                match *result {
                    Ok(s) => {
                        let text = format!("Read the costs of {}: {}", self.env, analysis::read_note(&s));
                        ctx.toast(if s.missing.is_empty() { Level::Success } else { Level::Warn }, text);
                        self.snapshot = Some(s);
                        self.error = None;
                        self.rebuild();
                        self.find_shared(ctx);
                        if self.deploys.is_none() && !self.deploys_loading {
                            self.load_deploys(ctx);
                        } else {
                            self.recompute_steps();
                        }
                    }
                    Err(e) => {
                        ctx.toast(Level::Error, format!("Could not read the costs: {}", e.short()));
                        self.error = Some(e);
                    }
                }
            }
            Msg::Deploys { deploys, failed } => {
                self.deploys_loading = false;
                self.deploys = Some((deploys, failed));
                self.recompute_steps();
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        views::render(self, f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.dialog {
            Some(Dialog::Setup(_)) => return vec![("Tab", "next field"), ("←→", "choose"), ("Enter", "save"), ("Esc", "cancel")],
            Some(Dialog::Read(_)) => return vec![("Enter", "read"), ("Esc", "cancel")],
            None => {}
        }
        let mut hints = vec![("←→", "services / commitments")];
        if self.tab == Tab::Services {
            hints.extend([("↑↓", "service"), ("Enter", "usage types"), ("o", "open in dcli")]);
        }
        hints.extend([("g", "Savings"), ("r", "read again"), ("s", "settings"), ("c", "copy for AI"), ("PgUp/PgDn", "scroll")]);
        hints
    }

    fn animating(&self) -> bool {
        self.reading.is_some()
    }

    fn capturing(&self) -> bool {
        self.dialog.is_some()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if let Some(Dialog::Setup(form)) = &mut self.dialog {
            form.paste(text);
        }
    }

    fn open(&mut self, target: &str, _ctx: &mut Ctx) {
        if !target.is_empty() {
            self.find(target);
        }
    }
}

//! AppConfig feature flags: applications → configuration profiles → a table of flags ×
//! AppConfig environments showing what each environment serves (its deployed version) and
//! deployments in progress. Editing a flag builds a new version from the one deployed in
//! that environment (so nothing else reaches it), shows the change, and deploys it with a
//! chosen strategy; a running deployment can be stopped (rolled back). Freeform profiles
//! are shown read-only.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::LoadState;
use crate::aws::appconfig::{
    self as ac, ALL_AT_ONCE, Application, Deployment, EnvState, Environment, Flag, FlagsDoc, Profile, Strategy,
};
use crate::aws;
use crate::config::is_production;
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{self, TextInput};
use crate::util::{ellipsize, fmt_ago, sanitize};

const POLL_EVERY: Duration = Duration::from_secs(3);
const ON_OFF: &[&str] = &["on", "off"];
const TRUE_FALSE: &[&str] = &["true", "false"];

/// A hosted version's document, parsed once.
struct Doc {
    raw: String,
    flags: Option<FlagsDoc>,
}

impl Doc {
    fn new(raw: String) -> Doc {
        let flags = FlagsDoc::parse(&raw);
        Doc { raw, flags }
    }

    fn pretty(&self) -> String {
        sanitize(&crate::util::pretty_json(&self.raw).unwrap_or_else(|| self.raw.clone()))
    }
}

struct OpenApp {
    app: Application,
    load: LoadState,
    envs: Vec<Environment>,
    profiles: Vec<Profile>,
    /// Environment id → its deployments, newest first.
    deployments: HashMap<String, Vec<Deployment>>,
}

type AppData = (Vec<Environment>, Vec<Profile>, Vec<(String, Vec<Deployment>)>);

enum Msg {
    Apps(aws::Result<Vec<Application>>),
    AppData { app: String, result: aws::Result<AppData> },
    Strategies(aws::Result<Vec<Strategy>>),
    Doc { profile: String, version: i64, result: aws::Result<String> },
    Latest { profile: String, result: aws::Result<Option<i64>> },
    Started { env: String, profile: String, doc: String, result: aws::Result<(i64, Deployment)> },
    Polled { env: String, result: aws::Result<Deployment> },
    Stopped { env: String, number: i64, result: aws::Result<()> },
    Refreshed { env: String, result: aws::Result<Vec<Deployment>> },
}

#[derive(PartialEq)]
enum Focus {
    List,
    Table,
}

/// A change ready to deploy.
struct Plan {
    env: usize,
    base: Option<i64>,
    /// What the environment served, and the profile's latest version, when the change was
    /// made: deploying re-checks both so nobody else's change is overwritten.
    env_version: Option<i64>,
    latest: Option<i64>,
    doc: String,
    diff: Vec<String>,
}

enum Dialog {
    Edit { form: Form, env: usize, flag: Flag, base: Option<i64> },
    Strategy { picker: Picker, plan: Plan },
    Confirm { plan: Plan, strategy: Strategy, input: TextInput, typed: Option<String> },
    Stop { env: usize, number: i64, version: String, input: TextInput, typed: Option<String> },
    Busy(String),
}

pub struct AppConfig {
    apps: Vec<Application>,
    apps_load: LoadState,
    open: Option<OpenApp>,
    list_state: ListState,
    strategies: Vec<Strategy>,
    focus: Focus,
    row: usize,
    col: usize,
    docs: HashMap<(String, i64), aws::Result<Doc>>,
    docs_loading: HashSet<(String, i64)>,
    latest: HashMap<String, Option<i64>>,
    dialog: Option<Dialog>,
    /// Deployments being watched: (environment id, deployment number).
    tracking: HashSet<(String, i64)>,
    last_poll: Option<Instant>,
    polling: usize,
    env_name: String,
    scroll: u16,
    list_area: Rect,
}

/// Grey hint for an empty attribute field.
fn kind_hint(kind: Option<&str>) -> &'static str {
    match kind {
        Some("number") => "number",
        Some("string[]") => "a, b, c",
        Some("number[]") => "1, 2, 3",
        _ => "",
    }
}

/// Form labels are `&'static str`; attribute names are interned (a handful per session).
fn label(name: &str) -> &'static str {
    static LABELS: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();
    let mut labels = LABELS.get_or_init(|| Mutex::new(Vec::new())).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(l) = labels.iter().find(|l| **l == name) {
        return l;
    }
    let l: &'static str = Box::leak(name.to_string().into_boxed_str());
    labels.push(l);
    l
}

fn env_color(name: &str) -> Color {
    if is_production(name) { theme::ERR } else { theme::OK }
}

impl AppConfig {
    pub fn new() -> AppConfig {
        AppConfig {
            apps: Vec::new(),
            apps_load: LoadState::Idle,
            open: None,
            list_state: ListState::default(),
            strategies: Vec::new(),
            focus: Focus::List,
            row: 0,
            col: 0,
            docs: HashMap::new(),
            docs_loading: HashSet::new(),
            latest: HashMap::new(),
            dialog: None,
            tracking: HashSet::new(),
            last_poll: None,
            polling: 0,
            env_name: String::new(),
            scroll: 0,
            list_area: Rect::default(),
        }
    }

    fn load_apps(&mut self, ctx: &mut Ctx) {
        self.apps_load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Apps(ac::list_applications(&aws)));
    }

    fn open_app(&mut self, index: usize, ctx: &mut Ctx) {
        let Some(app) = self.apps.get(index).cloned() else { return };
        let id = app.id.clone();
        self.open = Some(OpenApp {
            app,
            load: LoadState::Loading,
            envs: Vec::new(),
            profiles: Vec::new(),
            deployments: HashMap::new(),
        });
        self.list_state.select(Some(0));
        self.focus = Focus::List;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = (|| -> aws::Result<AppData> {
                let envs = ac::list_environments(&aws, &id)?;
                let profiles = ac::list_profiles(&aws, &id)?;
                let mut deployments = Vec::new();
                for e in &envs {
                    deployments.push((e.id.clone(), ac::list_deployments(&aws, &id, &e.id, 2)?));
                }
                Ok((envs, profiles, deployments))
            })();
            Msg::AppData { app: id, result }
        });
        if self.strategies.is_empty() {
            let aws = ctx.env().aws.clone();
            ctx.spawn(move || Msg::Strategies(ac::list_strategies(&aws)));
        }
    }

    fn close_app(&mut self) {
        let id = self.open.take().map(|o| o.app.id);
        let index = id.and_then(|id| self.apps.iter().position(|a| a.id == id)).unwrap_or(0);
        self.list_state.select(Some(index));
        self.focus = Focus::List;
    }

    fn profile(&self) -> Option<&Profile> {
        let open = self.open.as_ref()?;
        open.profiles.get(self.list_state.selected()?.checked_sub(1)?)
    }

    fn env_state(&self, env: usize, profile: &Profile) -> EnvState {
        let Some(open) = &self.open else { return EnvState::default() };
        let Some(e) = open.envs.get(env) else { return EnvState::default() };
        open.deployments.get(&e.id).map(|d| EnvState::of(d, &profile.id, &profile.name)).unwrap_or_default()
    }

    fn doc(&self, profile: &Profile, version: Option<i64>) -> Option<&aws::Result<Doc>> {
        self.docs.get(&(profile.id.clone(), version?))
    }

    /// Loads the documents the table needs: each environment's deployed version and the latest.
    fn ensure_docs(&mut self, ctx: &mut Ctx) {
        let (Some(open), Some(profile)) = (&self.open, self.profile().cloned()) else { return };
        if !profile.is_hosted() {
            return;
        }
        let app = open.app.id.clone();
        let mut versions: Vec<i64> = (0..open.envs.len()).filter_map(|e| self.env_state(e, &profile).version()).collect();
        match self.latest.get(&profile.id) {
            Some(Some(v)) => versions.push(*v),
            Some(None) => {}
            None => {
                self.latest.insert(profile.id.clone(), None);
                let (aws, app, id) = (ctx.env().aws.clone(), app.clone(), profile.id.clone());
                ctx.spawn(move || {
                    let result = ac::latest_version(&aws, &app, &id);
                    Msg::Latest { profile: id, result }
                });
            }
        }
        for version in versions {
            let key = (profile.id.clone(), version);
            if self.docs.contains_key(&key) || !self.docs_loading.insert(key) {
                continue;
            }
            let (aws, app, id) = (ctx.env().aws.clone(), app.clone(), profile.id.clone());
            ctx.spawn(move || {
                let result = ac::get_hosted_version(&aws, &app, &id, version);
                Msg::Doc { profile: id, version, result }
            });
        }
    }

    /// The definition of a flag from the newest loaded version that has it.
    fn definition(&self, profile: &Profile, key: &str) -> Option<serde_json::Value> {
        let mut versions: Vec<i64> = self.docs.keys().filter(|(p, _)| *p == profile.id).map(|(_, v)| *v).collect();
        versions.sort_unstable_by(|a, b| b.cmp(a));
        versions.into_iter().find_map(|v| match self.doc(profile, Some(v)) {
            Some(Ok(Doc { flags: Some(doc), .. })) => doc.definition(key),
            _ => None,
        })
    }

    /// Flags of every loaded version (latest first), by key.
    fn rows(&self, profile: &Profile) -> Vec<Flag> {
        let Some(open) = &self.open else { return Vec::new() };
        let mut versions: Vec<i64> = self.latest.get(&profile.id).copied().flatten().into_iter().collect();
        versions.extend((0..open.envs.len()).filter_map(|e| self.env_state(e, profile).version()));
        let mut rows: Vec<Flag> = Vec::new();
        for v in versions {
            if let Some(Ok(Doc { flags: Some(doc), .. })) = self.doc(profile, Some(v)) {
                for flag in doc.flags() {
                    if !rows.iter().any(|r| r.key == flag.key) {
                        rows.push(flag);
                    }
                }
            }
        }
        rows
    }

    fn select_list(&mut self, index: usize, ctx: &mut Ctx) {
        let len = match &self.open {
            Some(open) => open.profiles.len() + 1,
            None => self.apps.len(),
        };
        if len == 0 {
            return;
        }
        let index = index.min(len - 1);
        if self.list_state.selected() != Some(index) {
            self.row = 0;
            self.scroll = 0;
        }
        self.list_state.select(Some(index));
        self.ensure_docs(ctx);
    }

    fn prod_like(&self, env: &Environment) -> bool {
        is_production(&self.env_name) || is_production(&env.name)
    }

    // ---- editing --------------------------------------------------------------------

    fn open_editor(&mut self, ctx: &mut Ctx) {
        let (Some(open), Some(profile)) = (&self.open, self.profile().cloned()) else { return };
        if !profile.is_feature_flags() || !profile.is_hosted() {
            return ctx.toast(Level::Info, "Only hosted feature flag profiles can be edited here");
        }
        let Some(env) = open.envs.get(self.col).cloned() else { return };
        let Some(flag) = self.rows(&profile).get(self.row).cloned() else { return };
        let state = self.env_state(self.col, &profile);
        let base = state.version().or_else(|| self.latest.get(&profile.id).copied().flatten());
        let doc = match self.doc(&profile, base) {
            Some(Ok(Doc { flags: Some(doc), .. })) => doc.clone(),
            Some(Ok(_)) => return ctx.toast(Level::Error, "That version is not a feature flags document"),
            Some(Err(e)) => return ctx.toast(Level::Error, e.short()),
            None if base.is_none() => FlagsDoc::parse(r#"{"version":"1","flags":{},"values":{}}"#).expect("valid"),
            None => return ctx.toast(Level::Info, "Still loading that version"),
        };
        if let Some(active) = &state.active {
            ctx.toast(Level::Warn, format!("{} has a deployment {} — a new one waits for it", env.name, active.progress()));
        }
        let from = match (state.version(), base) {
            (Some(v), _) => format!("{} · now v{v}", env.name),
            (None, Some(v)) => format!("{} · not deployed yet, from latest v{v}", env.name),
            (None, None) => format!("{} · first version", env.name),
        };
        let mut fields = vec![
            Field::fixed("Flag", format!("{} ({})", flag.name, flag.key)),
            Field::fixed("Environment", from),
            Field::choice("Enabled", ON_OFF, ac::on_off(doc.enabled(&flag.key))),
        ];
        for attr in &flag.attributes {
            let name = if ["Flag", "Environment", "Enabled"].contains(&attr.name.as_str()) {
                label(&format!("attr {}", attr.name))
            } else {
                label(&attr.name)
            };
            let current = doc.value(&flag.key, &attr.name);
            let boolean = attr.kind.as_deref() == Some("boolean") || current.is_some_and(serde_json::Value::is_boolean);
            fields.push(if boolean {
                Field::choice(name, TRUE_FALSE, &current.map(ac::value_text).unwrap_or_else(|| "false".into()))
            } else {
                Field::text(name, current.map(ac::value_text).unwrap_or_default()).optional(kind_hint(attr.kind.as_deref()))
            });
        }
        let mut form = Form::new(format!("Edit flag · {} · {}", profile.name, env.name), fields);
        form.note = Some("Next: review the change and pick a deployment strategy.".into());
        self.dialog = Some(Dialog::Edit { form, env: self.col, flag, base });
    }

    /// Builds the new document from the edited form; `Err` keeps the form open.
    fn plan(&self, form: &Form, env: usize, flag: &Flag, base: Option<i64>) -> Result<Plan, String> {
        let profile = self.profile().ok_or("No profile selected")?;
        let doc = match self.doc(profile, base) {
            Some(Ok(Doc { flags: Some(doc), .. })) => doc.clone(),
            _ if base.is_none() => FlagsDoc::parse(r#"{"version":"1","flags":{},"values":{}}"#).expect("valid"),
            _ => return Err("The base version is not loaded".into()),
        };
        let mut attrs = Vec::new();
        for attr in &flag.attributes {
            let name = if ["Flag", "Environment", "Enabled"].contains(&attr.name.as_str()) {
                format!("attr {}", attr.name)
            } else {
                attr.name.clone()
            };
            let text = form.get(&name);
            let current = doc.value(&flag.key, &attr.name);
            if text.trim().is_empty() && current.is_none() {
                continue;
            }
            let value = ac::parse_value(text, attr.kind.as_deref(), current).map_err(|e| format!("{}: {e}", attr.name))?;
            attrs.push((attr.name.clone(), value));
        }
        // A flag this version lacks takes its definition from a version that has it.
        let definition = self.definition(profile, &flag.key);
        let newer = doc.with_values(&flag.key, form.get("Enabled") == "on", &attrs, definition.as_ref());
        let diff = doc.diff(&newer, &flag.key);
        if diff.is_empty() {
            return Err("Nothing changed".into());
        }
        let env_version = self.env_state(env, profile).version();
        let latest = self.latest.get(&profile.id).copied().flatten();
        Ok(Plan { env, base, env_version, latest, doc: newer.to_json(), diff })
    }

    fn choose_strategy(&mut self, plan: Plan) {
        let mut strategies = self.strategies.clone();
        if strategies.is_empty() {
            strategies.push(Strategy { id: ALL_AT_ONCE.into(), name: ALL_AT_ONCE.into(), ..Default::default() });
        }
        let prod = self.open.as_ref().and_then(|o| o.envs.get(plan.env)).is_some_and(|e| self.prod_like(e));
        let cursor = if prod {
            strategies.iter().position(|s| s.deployment_duration_in_minutes.unwrap_or(0.0) > 0.0).unwrap_or(0)
        } else {
            strategies.iter().position(|s| s.id == ALL_AT_ONCE || s.name.contains("AllAtOnce")).unwrap_or(0)
        };
        self.strategies = strategies;
        let items = self.strategies.iter().map(|s| Item::new(s.name.as_str(), s.summary())).collect();
        let env = self.open.as_ref().and_then(|o| o.envs.get(plan.env)).map(|e| e.name.clone()).unwrap_or_default();
        let picker = Picker::new(format!("Deployment strategy · {env}"), "next", items, false).with_cursor(cursor);
        self.dialog = Some(Dialog::Strategy { picker, plan });
    }

    fn confirm(&mut self, plan: Plan, strategy: Strategy) {
        let typed = self.open.as_ref().and_then(|o| o.envs.get(plan.env)).filter(|e| self.prod_like(e)).map(|e| e.name.clone());
        self.dialog = Some(Dialog::Confirm { plan, strategy, input: TextInput::default(), typed });
    }

    fn deploy(&mut self, plan: Plan, strategy: Strategy, ctx: &mut Ctx) {
        let (Some(open), Some(profile)) = (&self.open, self.profile().cloned()) else { return };
        let Some(env) = open.envs.get(plan.env).cloned() else { return };
        let app = open.app.id.clone();
        self.dialog = Some(Dialog::Busy(format!("Creating a version and deploying it to {}…", env.name)));
        let description = ellipsize(&format!("dcli: {}", plan.diff.join("; ")), 900);
        let aws = ctx.env().aws.clone();
        let (doc, env_version, known_latest) = (plan.doc, plan.env_version, plan.latest);
        ctx.spawn(move || {
            let result = (|| {
                // Built on what the environment served and the versions that existed when the
                // change was made: if either moved, someone else changed it meanwhile.
                let changed = |what: String| {
                    aws::AwsError::Config(format!("{what} since you opened the editor; press r to reload and edit again"))
                };
                let now = EnvState::of(&ac::list_deployments(&aws, &app, &env.id, 2)?, &profile.id, &profile.name);
                if let Some(active) = &now.active {
                    return Err(changed(format!("{} started a deployment ({})", env.name, active.progress())));
                }
                if now.version() != env_version {
                    return Err(changed(format!("{} moved to another version", env.name)));
                }
                let latest = ac::latest_version(&aws, &app, &profile.id)?;
                if latest != known_latest {
                    return Err(changed("A new version of this profile was created".to_string()));
                }
                let version = ac::create_hosted_version(&aws, &app, &profile.id, &doc, &description, latest)?;
                let deployment = ac::start_deployment(&aws, &app, &env.id, &profile.id, version, &strategy.id, &description)?;
                Ok((version, deployment))
            })();
            Msg::Started { env: env.id, profile: profile.id, doc, result }
        });
    }

    fn open_stop(&mut self, ctx: &mut Ctx) {
        let (Some(open), Some(profile)) = (&self.open, self.profile().cloned()) else { return };
        let Some(env) = open.envs.get(self.col).cloned() else { return };
        match self.env_state(self.col, &profile).active.filter(Deployment::can_stop) {
            Some(d) => {
                let typed = self.prod_like(&env).then(|| env.name.clone());
                self.dialog = Some(Dialog::Stop {
                    env: self.col,
                    number: d.deployment_number,
                    version: d.configuration_version.clone(),
                    input: TextInput::default(),
                    typed,
                });
            }
            None => ctx.toast(Level::Info, format!("No deployment of {} running in {}", profile.name, env.name)),
        }
    }

    fn stop(&mut self, env: usize, number: i64, ctx: &mut Ctx) {
        let Some(open) = &self.open else { return };
        let Some(e) = open.envs.get(env) else { return };
        let (aws, app, env_id) = (ctx.env().aws.clone(), open.app.id.clone(), e.id.clone());
        self.dialog = Some(Dialog::Busy(format!("Stopping deployment #{number}…")));
        ctx.spawn(move || {
            let result = ac::stop_deployment(&aws, &app, &env_id, number);
            Msg::Stopped { env: env_id, number, result }
        });
    }

    fn refresh_env(&mut self, env_id: String, ctx: &mut Ctx) {
        let Some(open) = &self.open else { return };
        let (aws, app) = (ctx.env().aws.clone(), open.app.id.clone());
        ctx.spawn(move || {
            let result = ac::list_deployments(&aws, &app, &env_id, 2);
            Msg::Refreshed { env: env_id, result }
        });
    }

    /// Watches every deployment still running in the open application.
    fn track_active(&mut self) {
        if let Some(open) = &self.open {
            for (env, list) in &open.deployments {
                for d in list.iter().filter(|d| d.is_active()) {
                    self.tracking.insert((env.clone(), d.deployment_number));
                }
            }
        }
    }

    fn upsert(&mut self, env: &str, d: Deployment) {
        if let Some(list) = self.open.as_mut().and_then(|o| o.deployments.get_mut(env)) {
            match list.iter_mut().find(|x| x.deployment_number == d.deployment_number) {
                Some(slot) => *slot = d,
                None => {
                    list.push(d);
                    list.sort_by_key(|x| std::cmp::Reverse(x.deployment_number));
                }
            }
        }
    }

    fn env_name_of(&self, env_id: &str) -> String {
        self.open
            .as_ref()
            .and_then(|o| o.envs.iter().find(|e| e.id == env_id))
            .map_or_else(|| env_id.to_string(), |e| e.name.clone())
    }

    fn copy_document(&self, ctx: &mut Ctx) {
        let Some(profile) = self.profile() else { return };
        let version = self.env_state(self.col, profile).version().or_else(|| self.latest.get(&profile.id).copied().flatten());
        match self.doc(profile, version) {
            Some(Ok(doc)) => {
                let text = crate::util::pretty_json(&doc.raw).unwrap_or_else(|| doc.raw.clone());
                let ok = crate::platform::copy_to_clipboard(&text);
                let what = version.map(|v| format!("v{v}")).unwrap_or_default();
                ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { format!("Copied {} {what}", profile.name) } else { "Clipboard not available".into() });
            }
            _ => ctx.toast(Level::Info, "Nothing loaded to copy"),
        }
    }

    // ---- rendering ------------------------------------------------------------------

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let focused = self.focus == Focus::List && self.dialog.is_none();
        let (title, items): (String, Vec<ListItem>) = match &self.open {
            None => {
                let title = match &self.apps_load {
                    LoadState::Loading => format!(" {} Applications ", theme::spinner(view.tick)),
                    _ => format!(" Applications · {} ", self.apps.len()),
                };
                let width = area.width.saturating_sub(4) as usize;
                let items = self
                    .apps
                    .iter()
                    .map(|a| {
                        let desc = a.description.as_deref().map(|d| format!("  {d}")).unwrap_or_default();
                        ListItem::new(Line::from(vec![
                            Span::styled(ellipsize(&a.name, width), theme::bold()),
                            Span::styled(ellipsize(&desc, width.saturating_sub(a.name.chars().count())), theme::muted()),
                        ]))
                    })
                    .collect();
                (title, items)
            }
            Some(open) => {
                let title = match &open.load {
                    LoadState::Loading => format!(" {} {} ", theme::spinner(view.tick), open.app.name),
                    _ => format!(" {} · profiles ", open.app.name),
                };
                let mut items = vec![ListItem::new(Line::styled("‹ applications", theme::muted()))];
                items.extend(open.profiles.iter().map(|p| {
                    let (badge, color) = match (p.is_feature_flags(), p.is_hosted()) {
                        (true, true) => ("flags", theme::APPROVAL),
                        (_, false) => ("not hosted", theme::MUTED),
                        _ => ("freeform", theme::MUTED),
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(if p.is_feature_flags() { "⚑ " } else { "{} " }, theme::accent()),
                        Span::raw(p.name.clone()),
                        Span::styled(format!("  {badge}"), Style::new().fg(color)),
                    ]))
                }));
                (title, items)
            }
        };
        let empty = match (&self.open, &self.apps_load) {
            (Some(o), _) if matches!(o.load, LoadState::Loading) => None,
            (Some(o), _) => match &o.load {
                LoadState::Failed(e) => Some(Line::styled(e.short(), Style::new().fg(theme::ERR))),
                _ => None,
            },
            (None, LoadState::Failed(e)) => Some(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            (None, LoadState::Loaded) if self.apps.is_empty() => Some(Line::styled("No AppConfig applications", theme::muted())),
            _ => None,
        };
        if let Some(text) = empty {
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, focused)), area);
            return;
        }
        let list = List::new(items).block(widgets::panel(title, focused)).highlight_style(theme::selected());
        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn render_flags(&self, f: &mut Frame, area: Rect, view: &View, profile: &Profile) {
        let Some(open) = &self.open else { return };
        let rows = self.rows(profile);
        let detail_h = 10u16.min(area.height / 2);
        let [table, detail] = area.layout(&Layout::vertical([Constraint::Fill(1), Constraint::Length(detail_h)]));
        let focused = self.focus == Focus::Table && self.dialog.is_none();
        let block = widgets::panel(format!(" {} · flags × environments ", profile.name), focused);
        let inner = block.inner(table);
        f.render_widget(block, table);

        let n = open.envs.len().max(1);
        let flag_w = rows.iter().map(|r| r.name.chars().count()).max().unwrap_or(4).clamp(8, 28) + 2;
        let col_w = ((inner.width as usize).saturating_sub(flag_w) / n).max(14);
        let mut header = vec![Span::raw(format!("{:<flag_w$}", ""))];
        let mut status = vec![Span::raw(format!("{:<flag_w$}", ""))];
        for (i, e) in open.envs.iter().enumerate() {
            header.push(Span::styled(
                format!("{:<col_w$}", ellipsize(&e.name, col_w - 1)),
                Style::new().fg(env_color(&e.name)).add_modifier(Modifier::BOLD),
            ));
            let state = self.env_state(i, profile);
            let (text, style) = match (&state.active, state.version()) {
                (Some(a), _) => (format!("{} → v{}", a.progress(), a.configuration_version), Style::new().fg(theme::RUNNING)),
                (None, Some(v)) => (format!("v{v}"), theme::muted()),
                (None, None) => ("not deployed".to_string(), theme::muted()),
            };
            status.push(Span::styled(format!("{:<col_w$}", ellipsize(&text, col_w - 1)), style));
        }
        let mut lines = vec![Line::from(header), Line::from(status), Line::raw("")];
        if rows.is_empty() {
            let text = if self.docs_loading.is_empty() { "No flags in this profile" } else { "Loading…" };
            lines.push(Line::styled(format!("{:<flag_w$}{text}", ""), theme::muted()));
        }
        let height = inner.height.saturating_sub(3) as usize;
        let first = self.row.saturating_sub(height.saturating_sub(1));
        for (r, flag) in rows.iter().enumerate().skip(first).take(height) {
            let mut spans = vec![Span::styled(format!("{:<flag_w$}", ellipsize(&flag.name, flag_w - 2)), theme::bold())];
            for e in 0..open.envs.len() {
                let state = self.env_state(e, profile);
                let (text, style) = match self.doc(profile, state.version()) {
                    _ if state.version().is_none() => ("—".to_string(), theme::muted()),
                    None => (theme::spinner(view.tick).to_string(), theme::muted()),
                    Some(Err(_)) => ("! error".to_string(), Style::new().fg(theme::ERR)),
                    Some(Ok(Doc { flags: None, .. })) => ("not flags".to_string(), theme::muted()),
                    Some(Ok(Doc { flags: Some(doc), .. })) if !doc.has(&flag.key) => ("—".to_string(), theme::muted()),
                    Some(Ok(Doc { flags: Some(doc), .. })) => {
                        let on = doc.enabled(&flag.key);
                        let summary = doc.summary(&flag.key, col_w.saturating_sub(3));
                        (format!("{} {summary}", if on { "●" } else { "○" }), Style::new().fg(if on { theme::OK } else { theme::MUTED }))
                    }
                };
                let mut style = style;
                if focused && r == self.row && e == self.col {
                    style = style.patch(theme::selected());
                }
                spans.push(Span::styled(format!("{:<col_w$}", ellipsize(&text, col_w - 1)), style));
            }
            lines.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(lines), inner);
        self.render_detail(f, detail, profile, rows.get(self.row));
    }

    fn render_detail(&self, f: &mut Frame, area: Rect, profile: &Profile, flag: Option<&Flag>) {
        let (Some(open), Some(flag)) = (&self.open, flag) else {
            f.render_widget(widgets::panel(" Flag ", false), area);
            return;
        };
        let Some(env) = open.envs.get(self.col) else { return };
        let state = self.env_state(self.col, profile);
        let kw = 12;
        let mut lines = vec![widgets::kv("Flag", format!("{} ({})", flag.name, flag.key), kw)];
        if let Some(d) = flag.description.as_ref().filter(|d| !d.is_empty()) {
            lines.push(widgets::kv("About", d.clone(), kw));
        }
        match (state.deployed.as_ref(), self.doc(profile, state.version())) {
            (Some(dep), Some(Ok(Doc { flags: Some(doc), .. }))) => {
                let when = dep.completed().or(dep.started()).map(|t| format!(" · deployed {}", fmt_ago(t))).unwrap_or_default();
                let on = doc.enabled(&flag.key);
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<kw$} ", env.name), Style::new().fg(env_color(&env.name))),
                    Span::styled(ac::on_off(on), Style::new().fg(if on { theme::OK } else { theme::MUTED }).add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" · v{}{when}", dep.configuration_version), theme::muted()),
                ]));
                for attr in &flag.attributes {
                    let value = doc.value(&flag.key, &attr.name).map_or_else(|| "—".to_string(), ac::value_text);
                    let kind = attr.kind.as_deref().map(|k| format!("  ({k})")).unwrap_or_default();
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:<kw$} ", ellipsize(&attr.name, kw)), theme::muted()),
                        Span::raw(value),
                        Span::styled(kind, theme::muted()),
                    ]));
                }
            }
            (None, _) => lines.push(widgets::kv(&env.name, "this profile is not deployed here".to_string(), kw)),
            _ => {}
        }
        if let Some(a) = &state.active {
            let started = a.started().map(|t| format!(" · started {}", fmt_ago(t))).unwrap_or_default();
            let stop = if a.can_stop() { " · x stops it (rolls back)" } else { "" };
            lines.push(Line::styled(
                format!("Deploying v{} · {}{started}{stop}", a.configuration_version, a.progress()),
                Style::new().fg(theme::RUNNING),
            ));
        }
        lines.push(Line::styled("Enter edit · y copy the document · Esc back", theme::muted()));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(widgets::panel(format!(" {} ", env.name), false)), area);
    }

    fn render_freeform(&self, f: &mut Frame, area: Rect, view: &View, profile: &Profile) {
        let Some(open) = &self.open else { return };
        let focused = self.focus == Focus::Table && self.dialog.is_none();
        let mut tabs = Vec::new();
        for (i, e) in open.envs.iter().enumerate() {
            let state = self.env_state(i, profile);
            let version = state.version().map(|v| format!(" v{v}")).unwrap_or_else(|| " —".into());
            let mut style = Style::new().fg(env_color(&e.name));
            if i == self.col {
                style = style.add_modifier(Modifier::REVERSED);
            }
            tabs.push(Span::styled(format!(" {}{version} ", e.name), style));
            tabs.push(Span::raw(" "));
        }
        let mut lines = vec![Line::from(tabs), Line::raw("")];
        let state = self.env_state(self.col, profile);
        if !profile.is_hosted() {
            lines.push(Line::styled("Stored outside AppConfig (S3, Parameter Store…): not shown here.", theme::muted()));
        } else {
            match (state.version(), self.doc(profile, state.version())) {
                (None, _) => lines.push(Line::styled("Not deployed in this environment.", theme::muted())),
                (_, None) => lines.push(Line::styled(format!("{} loading…", theme::spinner(view.tick)), theme::muted())),
                (_, Some(Err(e))) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
                (_, Some(Ok(doc))) => lines.extend(doc.pretty().lines().map(|l| Line::raw(l.to_string()))),
            }
        }
        let title = format!(" {} · read-only · ←→ environment · y copy ", profile.name);
        f.render_widget(Paragraph::new(Text::from(lines)).scroll((self.scroll, 0)).block(widgets::panel(title, focused)), area);
    }

    fn render_dialog(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let env_names: Vec<String> = self.open.as_ref().map(|o| o.envs.iter().map(|e| e.name.clone()).collect()).unwrap_or_default();
        let profile = self.profile().map(|p| p.name.clone()).unwrap_or_default();
        let Some(dialog) = &mut self.dialog else { return };
        match dialog {
            Dialog::Edit { form, env, .. } => {
                let color = env_names.get(*env).map_or(theme::ACCENT, |n| if is_production(n) { theme::ERR } else { theme::ACCENT });
                form.render(f, area, color);
            }
            Dialog::Strategy { picker, .. } => {
                let rect = widgets::popup_area(area, 84, picker.height(area.height.saturating_sub(4)));
                picker.render(f, rect, theme::ACCENT);
            }
            Dialog::Confirm { plan, strategy, input, typed } => {
                let env = env_names.get(plan.env).cloned().unwrap_or_default();
                let color = if typed.is_some() { theme::ERR } else { theme::ACCENT };
                let mut lines = vec![
                    Line::raw(""),
                    Line::from(vec![
                        Span::raw("Deploy "),
                        Span::styled(profile.clone(), theme::bold()),
                        Span::raw(" to "),
                        Span::styled(env.clone(), Style::new().fg(env_color(&env)).add_modifier(Modifier::BOLD)),
                        Span::raw("?"),
                    ]),
                    Line::raw(""),
                ];
                for d in &plan.diff {
                    lines.push(Line::styled(format!("  {d}"), Style::new().fg(theme::WARN)));
                }
                lines.push(Line::raw(""));
                let base = plan.base.map(|v| format!("v{v}")).unwrap_or_else(|| "an empty document".into());
                lines.push(Line::styled(format!("A new version is created from {base} (what {env} serves now)."), theme::muted()));
                lines.push(Line::from(vec![
                    Span::styled("Strategy  ", theme::muted()),
                    Span::styled(strategy.name.clone(), theme::bold()),
                    Span::styled(format!("  {}", strategy.summary()), theme::muted()),
                ]));
                let mut cursor = None;
                if let Some(name) = typed {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(format!("Type '{name}' to confirm:"), Style::new().fg(theme::WARN)));
                    let (line, col) = input.line("› ", "");
                    cursor = Some((lines.len() as u16, col));
                    lines.push(line);
                }
                lines.push(Line::raw(""));
                lines.push(widgets::hints_line(&[("Enter", "deploy"), ("Esc", "cancel")]));
                let rect = widgets::popup_area(area, 80, widgets::wrapped_height(&lines, 76) + 2);
                widgets::popup(f, rect, "Deploy feature flags", color, Text::from(lines));
                if let Some((row, col)) = cursor {
                    f.set_cursor_position((rect.x + 2 + col, rect.y + 1 + row));
                }
            }
            Dialog::Stop { env, number, version, input, typed } => {
                let env = env_names.get(*env).cloned().unwrap_or_default();
                let mut lines = vec![
                    Line::raw(""),
                    Line::raw(format!("Stop deployment #{number} (v{version} of {profile}) in {env}?")),
                    Line::styled("AppConfig rolls the environment back to the previous version.", theme::muted()),
                ];
                let mut cursor = None;
                if let Some(name) = typed {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(format!("Type '{name}' to confirm:"), Style::new().fg(theme::WARN)));
                    let (line, col) = input.line("› ", "");
                    cursor = Some((lines.len() as u16, col));
                    lines.push(line);
                }
                lines.push(Line::raw(""));
                lines.push(widgets::hints_line(&[("Enter", "stop"), ("Esc", "cancel")]));
                let rect = widgets::popup_area(area, 76, widgets::wrapped_height(&lines, 72) + 2);
                widgets::popup(f, rect, "Stop deployment", theme::WARN, Text::from(lines));
                if let Some((row, col)) = cursor {
                    f.set_cursor_position((rect.x + 2 + col, rect.y + 1 + row));
                }
            }
            Dialog::Busy(text) => {
                let body = Text::from(vec![
                    Line::raw(""),
                    Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw(text.clone())]),
                ]);
                widgets::popup(f, widgets::popup_area(area, 64, 5), "Working", theme::ACCENT, body);
            }
        }
    }

    fn dialog_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(dialog) = self.dialog.take() else { return };
        match dialog {
            Dialog::Edit { mut form, env, flag, base } => match form.handle(key) {
                FormEvent::Cancel => {}
                FormEvent::Pending => self.dialog = Some(Dialog::Edit { form, env, flag, base }),
                FormEvent::Submit => match self.plan(&form, env, &flag, base) {
                    Ok(plan) => self.choose_strategy(plan),
                    Err(e) => {
                        form.fail(e);
                        self.dialog = Some(Dialog::Edit { form, env, flag, base });
                    }
                },
            },
            Dialog::Strategy { mut picker, plan } => match picker.handle(key) {
                Outcome::Cancel => {}
                Outcome::Pending => self.dialog = Some(Dialog::Strategy { picker, plan }),
                Outcome::Done(chosen) => {
                    let strategy = self.strategies[chosen[0]].clone();
                    self.confirm(plan, strategy);
                }
            },
            Dialog::Confirm { plan, strategy, mut input, typed } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if typed.as_ref().is_some_and(|t| input.value.trim() != t) {
                        ctx.toast(Level::Warn, format!("Type '{}' to confirm", typed.as_deref().unwrap_or("")));
                        self.dialog = Some(Dialog::Confirm { plan, strategy, input, typed });
                    } else {
                        self.deploy(plan, strategy, ctx);
                    }
                }
                _ => {
                    if typed.is_some() {
                        input.handle(key);
                    }
                    self.dialog = Some(Dialog::Confirm { plan, strategy, input, typed });
                }
            },
            Dialog::Stop { env, number, version, mut input, typed } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if typed.as_ref().is_some_and(|t| input.value.trim() != t) {
                        ctx.toast(Level::Warn, format!("Type '{}' to confirm", typed.as_deref().unwrap_or("")));
                        self.dialog = Some(Dialog::Stop { env, number, version, input, typed });
                    } else {
                        self.stop(env, number, ctx);
                    }
                }
                _ => {
                    if typed.is_some() {
                        input.handle(key);
                    }
                    self.dialog = Some(Dialog::Stop { env, number, version, input, typed });
                }
            },
            Dialog::Busy(text) => self.dialog = Some(Dialog::Busy(text)),
        }
    }

    fn cell_count(&self) -> (usize, usize) {
        let Some(profile) = self.profile() else { return (0, 0) };
        let envs = self.open.as_ref().map_or(0, |o| o.envs.len());
        let rows = if profile.is_feature_flags() { self.rows(profile).len() } else { 0 };
        (rows, envs)
    }
}

impl Module for AppConfig {
    fn title(&self) -> &'static str {
        "AppConfig"
    }

    fn description(&self) -> &'static str {
        "AppConfig · feature flags per environment, edit and deploy with a strategy"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = AppConfig::new();
        self.env_name = ctx.env().name.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.apps_load, LoadState::Idle) {
            self.load_apps(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() {
            self.dialog_key(key, ctx);
            return Handled::Yes;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        if self.focus == Focus::Table {
            let (rows, cols) = self.cell_count();
            let flags = self.profile().is_some_and(Profile::is_feature_flags);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') if flags => self.row = self.row.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') if flags => self.row = (self.row + 1).min(rows.saturating_sub(1)),
                KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_add(1),
                KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
                KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
                KeyCode::Left | KeyCode::Char('h') => {
                    if self.col == 0 {
                        self.focus = Focus::List;
                    }
                    self.col = self.col.saturating_sub(1);
                    self.scroll = 0;
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.col = (self.col + 1).min(cols.saturating_sub(1));
                    self.scroll = 0;
                }
                KeyCode::Enter if flags => self.open_editor(ctx),
                KeyCode::Enter => ctx.toast(Level::Info, "Freeform configurations are read-only in dcli"),
                KeyCode::Char('x') => self.open_stop(ctx),
                KeyCode::Char('y') => self.copy_document(ctx),
                KeyCode::Char('r') => {
                    let index = self.open.as_ref().and_then(|o| self.apps.iter().position(|a| a.id == o.app.id));
                    let keep = self.list_state.selected();
                    if let Some(i) = index {
                        self.open_app(i, ctx);
                        self.list_state.select(keep);
                        self.focus = Focus::Table;
                    }
                }
                KeyCode::Esc => self.focus = Focus::List,
                _ => return Handled::No,
            }
            return Handled::Yes;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select_list(selected.saturating_sub(1), ctx),
            KeyCode::Down | KeyCode::Char('j') => self.select_list(selected + 1, ctx),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match &self.open {
                None => self.open_app(selected, ctx),
                Some(_) if selected == 0 => self.close_app(),
                Some(_) => {
                    if self.profile().is_some() {
                        self.focus = Focus::Table;
                        self.ensure_docs(ctx);
                    }
                }
            },
            KeyCode::Esc | KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') if self.open.is_some() => self.close_app(),
            KeyCode::Char('y') => self.copy_document(ctx),
            KeyCode::Char('r') => match self.open.as_ref().and_then(|o| self.apps.iter().position(|a| a.id == o.app.id)) {
                Some(i) => self.open_app(i, ctx),
                None => self.load_apps(ctx),
            },
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() || !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select_list(selected.saturating_sub(1), ctx),
            MouseEventKind::ScrollDown => self.select_list(selected + 1, ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Apps(result) => match result {
                Ok(apps) => {
                    self.apps = apps;
                    self.apps_load = LoadState::Loaded;
                    if self.open.is_none() {
                        self.list_state.select(if self.apps.is_empty() { None } else { Some(0) });
                    }
                }
                Err(e) => self.apps_load = LoadState::Failed(e),
            },
            Msg::AppData { app, result } => {
                let Some(open) = self.open.as_mut().filter(|o| o.app.id == app) else { return };
                match result {
                    Ok((envs, profiles, deployments)) => {
                        open.envs = envs;
                        open.profiles = profiles;
                        open.deployments = deployments.into_iter().collect();
                        open.load = LoadState::Loaded;
                        if self.list_state.selected() == Some(0) && !open.profiles.is_empty() {
                            self.list_state.select(Some(1));
                        }
                        self.col = self.col.min(open.envs.len().saturating_sub(1));
                        self.track_active();
                        self.ensure_docs(ctx);
                    }
                    Err(e) => open.load = LoadState::Failed(e),
                }
            }
            Msg::Strategies(result) => match result {
                Ok(mut s) => {
                    s.sort_by_key(|s| (s.deployment_duration_in_minutes.unwrap_or(0.0) as i64, s.name.clone()));
                    self.strategies = s;
                }
                Err(e) => ctx.toast(Level::Warn, format!("Could not list deployment strategies: {}", e.short())),
            },
            Msg::Doc { profile, version, result } => {
                self.docs_loading.remove(&(profile.clone(), version));
                self.docs.insert((profile, version), result.map(Doc::new));
            }
            Msg::Latest { profile, result } => {
                match result {
                    Ok(v) => {
                        self.latest.insert(profile, v);
                    }
                    Err(e) => {
                        self.latest.remove(&profile);
                        ctx.toast(Level::Warn, format!("Could not list versions: {}", e.short()));
                        return;
                    }
                }
                self.ensure_docs(ctx);
            }
            Msg::Started { env, profile, doc, result } => {
                self.dialog = None;
                match result {
                    Ok((version, deployment)) => {
                        let name = self.env_name_of(&env);
                        self.docs.insert((profile.clone(), version), Ok(Doc::new(doc)));
                        self.latest.insert(profile, Some(version));
                        self.tracking.insert((env.clone(), deployment.deployment_number));
                        ctx.toast(Level::Success, format!("Deploying v{version} to {name} · deployment #{}", deployment.deployment_number));
                        self.upsert(&env, deployment);
                        self.last_poll = Some(Instant::now());
                    }
                    Err(e) => ctx.toast(Level::Error, format!("Could not deploy: {}", e.short())),
                }
            }
            Msg::Polled { env, result } => {
                self.polling = self.polling.saturating_sub(1);
                match result {
                    Ok(d) => {
                        let key = (env.clone(), d.deployment_number);
                        if !d.is_active() {
                            self.tracking.remove(&key);
                            let name = self.env_name_of(&env);
                            match d.state.as_str() {
                                "COMPLETE" => ctx.toast(Level::Success, format!("v{} is live in {name}", d.configuration_version)),
                                "ROLLED_BACK" | "REVERTED" => {
                                    ctx.toast(Level::Warn, format!("Deployment #{} in {name} was rolled back", d.deployment_number))
                                }
                                _ => {}
                            }
                        }
                        self.upsert(&env, d);
                        self.ensure_docs(ctx);
                    }
                    Err(e) => {
                        self.tracking.retain(|(e2, _)| *e2 != env);
                        ctx.toast(Level::Warn, format!("Stopped watching a deployment: {}", e.short()));
                    }
                }
            }
            Msg::Stopped { env, number, result } => {
                self.dialog = None;
                match result {
                    Ok(()) => {
                        ctx.toast(Level::Success, format!("Stopped deployment #{number}; rolling back"));
                        self.refresh_env(env, ctx);
                    }
                    Err(e) => ctx.toast(Level::Error, format!("Could not stop: {}", e.short())),
                }
            }
            Msg::Refreshed { env, result } => {
                if let (Ok(list), Some(open)) = (result, self.open.as_mut()) {
                    open.deployments.insert(env, list);
                    self.track_active();
                    self.ensure_docs(ctx);
                }
            }
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, _active: bool) {
        if self.tracking.is_empty() || self.polling > 0 || self.last_poll.is_some_and(|t| t.elapsed() < POLL_EVERY) {
            return;
        }
        let Some(open) = &self.open else { return };
        self.last_poll = Some(Instant::now());
        for (env, number) in self.tracking.clone() {
            self.polling += 1;
            let (aws, app) = (ctx.env().aws.clone(), open.app.id.clone());
            ctx.spawn(move || {
                let result = ac::get_deployment(&aws, &app, &env, number);
                Msg::Polled { env, result }
            });
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [left, right] = if area.width < 100 {
            area.layout(&Layout::vertical([Constraint::Percentage(30), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(26), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        match (self.open.as_ref(), self.profile().cloned()) {
            (None, _) => {
                let text = Line::styled("Enter opens an application", theme::muted());
                f.render_widget(widgets::placeholder(text).block(widgets::panel(" Feature flags ", false)), right);
            }
            (Some(o), _) if matches!(o.load, LoadState::Loading) => {
                let text = Line::styled(format!("{} Loading environments and deployments…", theme::spinner(view.tick)), theme::muted());
                f.render_widget(widgets::placeholder(text).block(widgets::panel(" Feature flags ", false)), right);
            }
            (Some(_), None) => {
                let text = Line::styled("Select a configuration profile", theme::muted());
                f.render_widget(widgets::placeholder(text).block(widgets::panel(" Feature flags ", false)), right);
            }
            (Some(_), Some(p)) if p.is_feature_flags() && p.is_hosted() => self.render_flags(f, right, view, &p),
            (Some(_), Some(p)) => self.render_freeform(f, right, view, &p),
        }
        self.render_dialog(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.dialog {
            Some(Dialog::Edit { .. }) => return vec![("Tab", "next field"), ("←→", "choose"), ("Enter", "next"), ("Esc", "cancel")],
            Some(Dialog::Strategy { .. }) => return vec![("↑↓", "strategy"), ("Enter", "next"), ("Esc", "cancel")],
            Some(Dialog::Confirm { .. }) => return vec![("Enter", "deploy"), ("Esc", "cancel")],
            Some(Dialog::Stop { .. }) => return vec![("Enter", "stop"), ("Esc", "cancel")],
            Some(Dialog::Busy(_)) => return Vec::new(),
            None => {}
        }
        match (&self.open, &self.focus) {
            (None, _) => vec![("↑↓", "application"), ("Enter", "open"), ("r", "reload")],
            (Some(_), Focus::List) => vec![("↑↓", "profile"), ("Enter", "flags"), ("Esc", "applications"), ("y", "copy"), ("r", "reload")],
            (Some(_), Focus::Table) => {
                vec![("↑↓←→", "flag/env"), ("Enter", "edit"), ("x", "stop deployment"), ("y", "copy"), ("r", "reload"), ("Esc", "profiles")]
            }
        }
    }

    fn animating(&self) -> bool {
        matches!(self.apps_load, LoadState::Loading)
            || self.open.as_ref().is_some_and(|o| matches!(o.load, LoadState::Loading))
            || !self.docs_loading.is_empty()
            || !self.tracking.is_empty()
            || matches!(self.dialog, Some(Dialog::Busy(_)))
    }

    fn capturing(&self) -> bool {
        self.dialog.is_some()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.dialog {
            Some(Dialog::Edit { form, .. }) => form.paste(text),
            Some(Dialog::Strategy { picker, .. }) => picker.paste(text),
            Some(Dialog::Confirm { input, .. }) | Some(Dialog::Stop { input, .. }) => input.insert_str(text, false),
            _ => {}
        }
    }
}

//! API Gateway: REST, HTTP and WebSocket APIs. An API's routes (what each calls, how it is
//! protected, its numbers), its stages side by side (which deployment each runs; redeploy,
//! roll back, promote one stage's deployment to another), charts of a stage or route, and
//! the custom domains in front. A route can be tested (REST through API Gateway's test
//! invoke, HTTP APIs with a real request), copied as `curl`, exported as OpenAPI, and
//! written up for an AI agent.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::apigateway::{self as apigw, Api, ApiDetail, ApiMetrics, Auth, Domain, Kind, Route, RouteStats, Stage, TestRequest, TestResult};
use crate::aws::lambda::Range;
use crate::aws;
use crate::config::is_production;
use crate::tui::chart::{Agg, Chart, fmt_count, fmt_ms};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, now_secs};

const DEBOUNCE: Duration = Duration::from_millis(200);
/// Metrics older than this are read again when the module is opened again.
const METRICS_FRESH: Duration = Duration::from_secs(120);
/// Template lines shown in a route's detail.
const TEMPLATE_LINES: usize = 8;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Routes,
    Stages,
    Metrics,
    Domains,
}

const TABS: [Tab; 4] = [Tab::Routes, Tab::Stages, Tab::Metrics, Tab::Domains];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Routes => "Routes",
            Tab::Stages => "Stages",
            Tab::Metrics => "Metrics",
            Tab::Domains => "Domains",
        }
    }
}

/// A stage's metrics, or one route's: (API id, stage, route key, range).
type MetricKey = (String, String, Option<String>, Range);
/// Per-route numbers of a stage: (API id, stage, range).
type StatsKey = (String, String, Range);

enum Msg {
    Apis(aws::Result<Vec<Api>>),
    Detail { api: String, result: Box<aws::Result<ApiDetail>> },
    Metrics { key: MetricKey, result: Box<aws::Result<ApiMetrics>> },
    Stats { key: StatsKey, result: aws::Result<HashMap<String, RouteStats>> },
    Domains(aws::Result<Vec<Domain>>),
    Tested { title: String, result: Box<aws::Result<TestResult>> },
    Exported { api: String, result: aws::Result<String> },
    Report { what: String, result: aws::Result<String> },
    /// A stage was redeployed or pointed at another deployment.
    Changed { api: String, result: aws::Result<String> },
}

#[derive(Clone)]
enum Action {
    /// Point `stage` at `deployment`; `what` says it for the toast.
    Point { api: Api, stage: String, deployment: String, what: String },
}

enum Dialog {
    Test { form: Form, api: Box<Api>, route: Box<Route>, stage: Box<Stage>, busy: bool },
    Redeploy { form: Form, api: Api, stage: String, busy: bool },
    /// The deployment a stage goes back to: (picker, API, stage, deployment ids by item).
    Rollback(Picker, Api, String, Vec<String>),
    /// The stage that gets another's deployment: (picker, API, source stage, targets by item).
    Promote(Picker, Api, String, Vec<String>),
    /// OpenAPI as YAML or JSON.
    Export(Picker, Api, String),
    Confirm { title: String, lines: Vec<String>, typed: Option<(String, TextInput)>, action: Action, busy: bool, error: Option<String> },
}

pub struct ApiGateway {
    apis: Vec<Api>,
    load: LoadState,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    list_state: ListState,
    details: HashMap<String, aws::Result<ApiDetail>>,
    /// Jobs running: ("detail" | "metrics" | "stats" | "domains" | …, what they are for).
    in_flight: HashSet<(&'static str, String)>,
    pending: Option<(String, Instant)>,
    tab: Tab,
    /// The keys move through the rows of the tab instead of the APIs.
    focused: bool,
    cursor: usize,
    scroll: u16,
    /// The stage each API shows (metrics, tests, `curl`), by API id.
    stages: HashMap<String, String>,
    /// The route last chosen in each API, by API id.
    routes: HashMap<String, String>,
    range: Range,
    metrics: HashMap<MetricKey, (Instant, aws::Result<ApiMetrics>)>,
    stats: HashMap<StatsKey, (Instant, aws::Result<HashMap<String, RouteStats>>)>,
    domains: Option<aws::Result<Vec<Domain>>>,
    reveal: bool,
    dialog: Option<Dialog>,
    /// A test result or an export, full screen.
    zoom: Option<(String, LogView)>,
    reporting: bool,
    /// An API another module asked to show, selected once the list loads.
    pending_open: Option<String>,
    env_name: String,
    region: String,
    list_area: Rect,
}

fn copy(ctx: &mut Ctx, label: &str, text: &str, what: &str) {
    if crate::platform::copy_to_clipboard(text) {
        return ctx.toast(Level::Success, format!("Copied {what}"));
    }
    match crate::platform::write_temp_file(label, text) {
        Ok(path) => ctx.toast(Level::Success, format!("No clipboard: saved {what} to {}", path.display())),
        Err(e) => ctx.toast(Level::Error, format!("Could not save {what}: {e}")),
    }
}

/// Short label and colour of a route's protection; unprotected routes are flagged.
fn auth_look(kind: Kind, route: &Route) -> (String, Style) {
    if apigw::unprotected(kind, route) {
        return ("NONE ⚠".into(), Style::new().fg(theme::WARN));
    }
    let mut label = route.auth.label();
    if route.api_key_required {
        label = if route.auth == Auth::None { "API key".into() } else { format!("{label} + key") };
    }
    (label, theme::muted())
}

fn method_color(method: &str) -> Color {
    match method {
        "GET" | "HEAD" => theme::OK,
        "POST" => theme::RUNNING,
        "PUT" | "PATCH" => theme::WARN,
        "DELETE" => theme::ERR,
        _ => theme::APPROVAL,
    }
}

fn describe(d: &apigw::Deployment) -> String {
    let mut text = d.id.clone();
    if let Some(t) = d.created {
        text.push_str(&format!(" ({})", fmt_ago(t)));
    }
    if let Some(desc) = &d.description {
        text.push_str(&format!(" \"{}\"", ellipsize(desc, 40)));
    }
    text
}

impl ApiGateway {
    pub fn new() -> ApiGateway {
        ApiGateway {
            apis: Vec::new(),
            load: LoadState::Idle,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            details: HashMap::new(),
            in_flight: HashSet::new(),
            pending: None,
            tab: Tab::Routes,
            focused: false,
            cursor: 0,
            scroll: 0,
            stages: HashMap::new(),
            routes: HashMap::new(),
            range: Range::Day,
            metrics: HashMap::new(),
            stats: HashMap::new(),
            domains: None,
            reveal: false,
            dialog: None,
            zoom: None,
            reporting: false,
            pending_open: None,
            env_name: String::new(),
            region: String::new(),
            list_area: Rect::default(),
        }
    }

    // ---- state -----------------------------------------------------------------------------

    fn selected(&self) -> Option<&Api> {
        self.apis.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn detail(&self) -> Option<&ApiDetail> {
        self.details.get(&self.selected()?.id)?.as_ref().ok()
    }

    /// The stage shown for `api`: the one chosen, else the last (the most production-like).
    fn stage_of<'a>(&self, api: &Api, detail: &'a ApiDetail) -> Option<&'a Stage> {
        self.stages.get(&api.id).and_then(|s| detail.stage(s)).or(detail.stages.last())
    }

    fn current_stage(&self) -> Option<Stage> {
        self.stage_of(self.selected()?, self.detail()?).cloned()
    }

    /// The stage a stage action works on: the row on the Stages view, else the current one.
    fn acted_stage(&self) -> Option<Stage> {
        if self.tab == Tab::Stages && self.focused {
            return self.detail()?.stages.get(self.cursor).cloned();
        }
        self.current_stage()
    }

    /// The route on the Routes view, or the one last chosen for this API.
    fn current_route(&self) -> Option<Route> {
        let detail = self.detail()?;
        if self.tab == Tab::Routes && self.focused {
            return detail.routes.get(self.cursor).cloned();
        }
        let key = self.routes.get(&self.selected()?.id)?;
        detail.route(key).cloned()
    }

    /// Rows of the Domains view: (domain, mapping) indexes, the domains in front of this API
    /// first, and in each its mappings first.
    fn domain_rows(&self) -> Vec<(usize, usize)> {
        let (Some(api), Some(Ok(domains))) = (self.selected(), &self.domains) else { return Vec::new() };
        let ours = |d: usize, m: usize| domains[d].mappings[m].api_id == api.id;
        let mut rows: Vec<(usize, usize)> = domains.iter().enumerate().flat_map(|(d, dom)| (0..dom.mappings.len()).map(move |m| (d, m))).collect();
        rows.sort_by_key(|&(d, m)| (!(0..domains[d].mappings.len()).any(|x| ours(d, x)), d, !ours(d, m), m));
        rows
    }

    fn row_count(&self) -> usize {
        match self.tab {
            Tab::Routes => self.detail().map_or(0, |d| d.routes.len()),
            Tab::Stages => self.detail().map_or(0, |d| d.stages.len()),
            Tab::Metrics => 0,
            Tab::Domains => self.domain_rows().len(),
        }
    }

    /// Who must be named before a write or a test: the environment when it looks like
    /// production, else the stage when it does.
    fn typed_name(&self, stage: &str) -> Option<String> {
        if is_production(&self.env_name) {
            Some(self.env_name.clone())
        } else {
            is_production(stage).then(|| stage.to_string())
        }
    }

    fn dialog_color(&self, stage: Option<&str>) -> Color {
        if stage.map_or(is_production(&self.env_name), |s| self.typed_name(s).is_some()) { theme::ERR } else { theme::ACCENT }
    }

    // ---- loading ---------------------------------------------------------------------------

    fn load_apis(&mut self, ctx: &mut Ctx) {
        self.load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Apis(apigw::list_apis(&aws)));
    }

    fn fetch_detail(&mut self, api: Api, ctx: &mut Ctx) {
        if !self.in_flight.insert(("detail", api.id.clone())) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Detail { result: Box::new(apigw::detail(&aws, &api)), api: api.id });
    }

    fn fetch_domains(&mut self, ctx: &mut Ctx) {
        if self.domains.is_some() || !self.in_flight.insert(("domains", String::new())) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Domains(apigw::domains(&aws)));
    }

    /// Reads what the current view shows and is missing: per-route numbers on Routes,
    /// charts on Metrics.
    fn fetch_view(&mut self, ctx: &mut Ctx) {
        let (Some(api), Some(detail)) = (self.selected().cloned(), self.detail().cloned()) else { return };
        let Some(stage) = self.stage_of(&api, &detail).cloned() else { return };
        let aws = ctx.env().aws.clone();
        let range = self.range;
        match self.tab {
            Tab::Routes if stage.detailed_metrics && !detail.routes.is_empty() => {
                let key = (api.id.clone(), stage.name.clone(), range);
                if self.stats.contains_key(&key) || !self.in_flight.insert(("stats", format!("{key:?}"))) {
                    return;
                }
                ctx.spawn(move || Msg::Stats { result: apigw::route_stats(&aws, &api, &stage.name, &detail.routes, range, now_secs()), key });
            }
            Tab::Metrics => {
                let route = self.current_route().filter(|_| stage.detailed_metrics);
                for route in [None, route] {
                    let key = (api.id.clone(), stage.name.clone(), route.as_ref().map(Route::key), range);
                    if key.2.is_none() && route.is_some() {
                        continue;
                    }
                    if self.metrics.contains_key(&key) || !self.in_flight.insert(("metrics", format!("{key:?}"))) {
                        continue;
                    }
                    let (aws, api, stage, route) = (aws.clone(), api.clone(), stage.name.clone(), route.clone());
                    ctx.spawn(move || Msg::Metrics { result: Box::new(apigw::metrics(&aws, &api, &stage, route.as_ref(), range, now_secs())), key });
                }
            }
            Tab::Domains => self.fetch_domains(ctx),
            _ => {}
        }
    }

    fn refilter(&mut self) {
        let keep = self.selected().map(|a| a.id.clone());
        let query = self.filter.value.trim().to_lowercase();
        self.visible = (0..self.apis.len())
            .filter(|&i| {
                let a = &self.apis[i];
                query.is_empty() || matches_filter(&a.name, &query) || matches_filter(&a.id, &query) || matches_filter(a.kind.label(), &query)
            })
            .collect();
        let index = keep.and_then(|id| self.visible.iter().position(|&i| self.apis[i].id == id)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    fn select(&mut self, index: usize) {
        if self.visible.is_empty() {
            return;
        }
        let index = index.min(self.visible.len() - 1);
        if self.list_state.selected() != Some(index) {
            self.scroll = 0;
            self.cursor = 0;
            self.focused = false;
        }
        self.list_state.select(Some(index));
        if let Some(a) = self.selected()
            && !self.details.contains_key(&a.id)
        {
            self.pending = Some((a.id.clone(), Instant::now()));
        }
    }

    /// Selects the API in `pending_open` (by id or name) once the list has it.
    fn try_open(&mut self) -> bool {
        let Some(target) = self.pending_open.clone() else { return false };
        let found = |s: &Self| s.visible.iter().position(|&i| s.apis[i].id == target || s.apis[i].name == target);
        if found(self).is_none() && self.apis.iter().any(|a| a.id == target || a.name == target) {
            self.filter.clear();
            self.filtering = false;
            self.refilter();
        }
        let Some(pos) = found(self) else { return false };
        self.pending_open = None;
        self.zoom = None;
        self.dialog = None;
        self.select(pos);
        true
    }

    fn set_tab(&mut self, tab: Tab, ctx: &mut Ctx) {
        // The route under the cursor stays chosen when leaving the Routes view.
        self.remember_route();
        if self.tab != tab {
            self.tab = tab;
            self.cursor = 0;
            self.scroll = 0;
            self.focused = false;
        }
        self.fetch_view(ctx);
    }

    fn step_tab(&mut self, forward: bool, ctx: &mut Ctx) {
        let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
        let next = if forward { (i + 1) % TABS.len() } else { (i + TABS.len() - 1) % TABS.len() };
        self.set_tab(TABS[next], ctx);
    }

    fn remember_route(&mut self) {
        if self.tab == Tab::Routes
            && self.focused
            && let (Some(api), Some(route)) = (self.selected().map(|a| a.id.clone()), self.current_route())
        {
            self.routes.insert(api, route.key());
        }
    }

    /// `s`: the next stage of the API.
    fn next_stage(&mut self, ctx: &mut Ctx) {
        let (Some(api), Some(detail)) = (self.selected().cloned(), self.detail()) else { return };
        if detail.stages.is_empty() {
            return ctx.toast(Level::Info, format!("{} has no stages: it was never deployed", api.name));
        }
        let at = self.stage_of(&api, detail).and_then(|s| detail.stages.iter().position(|x| x.name == s.name)).unwrap_or(0);
        let next = detail.stages[(at + 1) % detail.stages.len()].name.clone();
        self.stages.insert(api.id.clone(), next);
        self.fetch_view(ctx);
    }

    // ---- actions on rows -----------------------------------------------------------------------

    fn activate_row(&mut self, ctx: &mut Ctx) {
        let Some(api) = self.selected().cloned() else { return };
        match self.tab {
            Tab::Routes => {
                let Some(route) = self.current_route() else { return };
                self.routes.insert(api.id.clone(), route.key());
                match route.integration.as_ref().map(apigw::Integration::target) {
                    Some(target) => match target.jump() {
                        Some((module, what)) => ctx.jump(module, what),
                        None => ctx.toast(Level::Info, format!("{} is not something dcli opens", target.label())),
                    },
                    None => ctx.toast(Level::Info, "This route has no integration"),
                }
            }
            Tab::Stages => {
                if let Some(stage) = self.detail().and_then(|d| d.stages.get(self.cursor)).map(|s| s.name.clone()) {
                    ctx.toast(Level::Info, format!("Showing stage {stage}"));
                    self.stages.insert(api.id, stage);
                    self.fetch_view(ctx);
                }
            }
            Tab::Domains => {
                let rows = self.domain_rows();
                let Some(Ok(domains)) = &self.domains else { return };
                let Some(&(d, m)) = rows.get(self.cursor) else { return };
                let mapping = domains[d].mappings[m].clone();
                if mapping.api_id != api.id {
                    self.pending_open = Some(mapping.api_id.clone());
                    if !self.try_open() {
                        ctx.toast(Level::Warn, format!("API {} is not in this account", mapping.api_id));
                        return;
                    }
                    self.tab = Tab::Domains;
                }
                if let Some(id) = self.selected().map(|a| a.id.clone()) {
                    self.stages.insert(id, mapping.stage.clone());
                }
            }
            Tab::Metrics => {}
        }
    }

    fn copy_row(&mut self, ctx: &mut Ctx) {
        let (Some(api), Some(detail)) = (self.selected().cloned(), self.detail().cloned()) else { return };
        match (self.tab, self.focused) {
            (Tab::Domains, true) => {
                let rows = self.domain_rows();
                if let (Some(Ok(domains)), Some(&(d, m))) = (&self.domains, rows.get(self.cursor)) {
                    let url = domains[d].url(&domains[d].mappings[m]);
                    copy(ctx, "apigw-url", &url, &url);
                }
            }
            (Tab::Stages, true) => {
                if let Some(s) = self.acted_stage() {
                    let url = api.invoke_url(&s.name, &self.region);
                    copy(ctx, "apigw-url", &url, &url);
                }
            }
            _ => {
                let Some(stage) = self.current_stage() else { return ctx.toast(Level::Info, "No stage to call: the API was never deployed") };
                if api.kind == Kind::WebSocket {
                    return copy(ctx, "apigw-wscat", &apigw::wscat(&api, &stage.name, &self.region, &detail), "the wscat command");
                }
                match self.current_route() {
                    Some(r) => copy(ctx, "apigw-curl", &apigw::curl(&api, &stage.name, &r, &self.region), &format!("curl for {} ({})", r.key(), stage.name)),
                    None => {
                        let url = api.invoke_url(&stage.name, &self.region);
                        copy(ctx, "apigw-url", &url, &url);
                    }
                }
            }
        }
    }

    fn open_logs(&mut self, ctx: &mut Ctx) {
        let (Some(api), Some(stage)) = (self.selected().cloned(), self.acted_stage()) else { return };
        if let Some(group) = stage.access_log_group() {
            return ctx.jump("Logs", group);
        }
        match apigw::execution_log_group(&api, &stage.name).filter(|_| stage.logging_level.is_some()) {
            Some(group) => ctx.jump("Logs", group),
            None => ctx.toast(Level::Info, format!("{} writes no logs to CloudWatch (access logs and execution logs are off)", stage.name)),
        }
    }

    fn copy_report(&mut self, ctx: &mut Ctx) {
        if self.reporting {
            return ctx.toast(Level::Info, "Still writing the report");
        }
        self.remember_route();
        let (Some(api), Some(stage)) = (self.selected().cloned(), self.current_stage()) else {
            return ctx.toast(Level::Info, "Nothing to report: the API has no stages");
        };
        let route = self.current_route().map(|r| r.key());
        let what = match &route {
            Some(r) => format!("the report of {r} on {}", stage.name),
            None => format!("the report of {} {}", api.name, stage.name),
        };
        self.reporting = true;
        ctx.toast(Level::Info, format!("Writing {what}: settings, 24 h of metrics and recent errors…"));
        let env = ctx.env();
        let (aws, env_name, region) = (env.aws.clone(), env.name.clone(), env.region.clone());
        ctx.spawn(move || {
            let result = apigw::gather_report(&aws, &env_name, &region, &api, &stage.name, route.as_deref(), now_secs());
            Msg::Report { what, result }
        });
    }

    fn open_export(&mut self, ctx: &mut Ctx) {
        let (Some(api), Some(stage)) = (self.selected().cloned(), self.current_stage()) else { return };
        if api.kind == Kind::WebSocket {
            return ctx.toast(Level::Info, "WebSocket APIs have no OpenAPI definition");
        }
        let items = vec![Item::new("YAML", format!("OpenAPI 3 of {} {}", api.name, stage.name)), Item::new("JSON", "the same, as JSON")];
        self.dialog = Some(Dialog::Export(Picker::new(format!("Export {} · {}", api.name, stage.name), "copy", items, false), api, stage.name));
    }

    // ---- testing a route -------------------------------------------------------------------------

    fn open_test(&mut self, ctx: &mut Ctx) {
        self.remember_route();
        let (Some(api), Some(detail)) = (self.selected().cloned(), self.detail().cloned()) else { return };
        let Some(stage) = self.current_stage() else { return ctx.toast(Level::Info, "No stage to test: the API was never deployed") };
        if api.kind == Kind::WebSocket {
            return copy(ctx, "apigw-wscat", &apigw::wscat(&api, &stage.name, &self.region, &detail), "a wscat command to connect (WebSocket APIs are not tested here)");
        }
        let Some(route) = self.current_route() else {
            return ctx.toast(Level::Info, "Choose a route first: Enter on Routes, move to one, then t");
        };
        let path = if route.path.starts_with('/') { route.path.clone() } else { "/".into() };
        let mut fields = vec![
            Field::fixed("Route", route.key()),
            Field::fixed("Stage", stage.name.clone()),
            Field::text("Path", path).hint("with its {parameters} filled in"),
            Field::text("Query", "").optional("a=1&b=2"),
            Field::text("Headers", "").optional("Name: value; Other: value"),
        ];
        let needs_auth = api.kind == Kind::Http && matches!(route.auth, Auth::Jwt | Auth::Lambda);
        if needs_auth {
            fields.push(Field::secret("Authorization", "").optional("Bearer …  (sent as the Authorization header)"));
        }
        fields.push(Field::text("Body", "").optional("paste works"));
        let mut form = Form::new(format!("Test · {} · {}", route.key(), stage.name), fields).submit_label("send").focus_on("Path");
        form.note = Some(match api.kind {
            Kind::Rest => format!(
                "Runs the integration for real with {}'s stage variables, through API Gateway's test invoke; the authorizer is skipped.",
                stage.name
            ),
            _ => format!("Sends a real request to {}: whatever the route does happens.", api.invoke_url(&stage.name, &self.region)),
        });
        if let Some(name) = self.typed_name(&stage.name) {
            form = form.confirm_with(&name);
        }
        self.dialog = Some(Dialog::Test { form, api: Box::new(api), route: Box::new(route), stage: Box::new(stage), busy: false });
    }

    fn submit_test(&mut self, ctx: &mut Ctx) {
        let region = self.region.clone();
        let Some(Dialog::Test { form, api, route, stage, busy }) = &mut self.dialog else { return };
        let path = form.get("Path").trim().to_string();
        let missing = apigw::unfilled(&path);
        if !path.starts_with('/') {
            return form.fail("The path starts with /");
        }
        if !missing.is_empty() {
            return form.fail(format!("Fill in {} in the path", missing.iter().map(|m| format!("{{{m}}}")).collect::<Vec<_>>().join(", ")));
        }
        let mut headers = match TestRequest::parse_headers(form.get("Headers")) {
            Ok(h) => h,
            Err(e) => return form.fail(e),
        };
        let auth = form.get("Authorization").trim();
        if !auth.is_empty() {
            headers.push(("Authorization".into(), auth.to_string()));
        }
        let req = TestRequest { path, query: form.get("Query").trim().to_string(), headers, body: form.get("Body").to_string() };
        *busy = true;
        let (api, route, stage) = (api.clone(), route.clone(), stage.clone());
        let title = format!("Test · {} {} · {}", route.method, req.path_and_query(), stage.name);
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = match api.kind {
                Kind::Rest => apigw::test_invoke(&aws, &api, &route, &req, &stage.variables),
                _ => {
                    let method = if route.method == "ANY" || route.method.is_empty() { "GET" } else { route.method.as_str() };
                    apigw::test_url(&api, &stage.name, &region, &req).and_then(|url| apigw::call(method, &url, &req.headers, &req.body))
                }
            };
            Msg::Tested { title, result: Box::new(result) }
        });
    }

    fn on_tested(&mut self, title: String, result: aws::Result<TestResult>, ctx: &mut Ctx) {
        let Some(Dialog::Test { form, busy, .. }) = &mut self.dialog else { return };
        *busy = false;
        let r = match result {
            Ok(r) => r,
            Err(e) => return form.fail(e.short()),
        };
        self.dialog = None;
        let mut view = LogView::default();
        view.extend(apigw::result_lines(&r));
        view.zoomed = true;
        view.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        self.zoom = Some((title, view));
        match r.status {
            200..=399 => ctx.toast(Level::Success, format!("Status {}", r.status)),
            s => ctx.toast(Level::Warn, format!("Status {s}")),
        }
    }

    // ---- changing stages --------------------------------------------------------------------------

    fn open_redeploy(&mut self, ctx: &mut Ctx) {
        let (Some(api), Some(detail), Some(stage)) = (self.selected().cloned(), self.detail(), self.acted_stage()) else { return };
        if stage.auto_deploy {
            return ctx.toast(Level::Info, format!("{} deploys by itself on every change (auto-deploy): nothing to redeploy", stage.name));
        }
        let now = detail.deployment(stage.deployment_id.as_deref()).map(describe).unwrap_or_else(|| "no deployment".into());
        let mut form = Form::new(
            format!("Redeploy · {} · {}", api.title(), stage.name),
            vec![Field::fixed("Stage", stage.name.clone()), Field::fixed("Runs now", now), Field::text("Description", "").optional("what changed")],
        )
        .submit_label("deploy")
        .focus_on("Description");
        form.note = Some(format!(
            "Deploys {} as it is now to {}: every change made since its deployment goes live.",
            api.name, stage.name
        ));
        if let Some(name) = self.typed_name(&stage.name) {
            form = form.confirm_with(&name);
        }
        self.dialog = Some(Dialog::Redeploy { form, api, stage: stage.name, busy: false });
    }

    fn submit_redeploy(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Redeploy { form, api, stage, busy }) = &mut self.dialog else { return };
        *busy = true;
        let (api, stage, description) = (api.clone(), stage.clone(), form.get("Description").to_string());
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = apigw::create_deployment(&aws, &api, &stage, &description).map(|d| format!("{stage} now runs deployment {}", d.id));
            Msg::Changed { api: api.id, result }
        });
    }

    fn open_rollback(&mut self, ctx: &mut Ctx) {
        let (Some(api), Some(detail), Some(stage)) = (self.selected().cloned(), self.detail(), self.acted_stage()) else { return };
        if let Some(why) = apigw::stage_locked(&stage) {
            return ctx.toast(Level::Info, why);
        }
        let previous = apigw::previous_deployment(detail, &stage).map(|d| d.id.clone());
        let mut items = Vec::new();
        let mut ids = Vec::new();
        for d in detail.deployments.iter().filter(|d| Some(&d.id) != stage.deployment_id.as_ref()) {
            let on: Vec<&str> = detail.stages.iter().filter(|s| s.deployment_id.as_ref() == Some(&d.id)).map(|s| s.name.as_str()).collect();
            let mut detail_text = d.description.clone().unwrap_or_default();
            if !on.is_empty() {
                detail_text = format!("{detail_text}  ← {}", on.join(", ")).trim().to_string();
            }
            if d.auto {
                detail_text = format!("{detail_text} (auto-deploy)").trim().to_string();
            }
            let label = format!("{}  {}", d.id, d.created.map(fmt_ago).unwrap_or_default());
            items.push(Item::new(label, detail_text).current(Some(&d.id) == previous.as_ref()));
            ids.push(d.id.clone());
        }
        if items.is_empty() {
            return ctx.toast(Level::Info, format!("{} has no other deployment to go back to", api.name));
        }
        let cursor = ids.iter().position(|id| Some(id) == previous.as_ref()).unwrap_or(0);
        let picker = Picker::new(format!("Roll {} back to", stage.name), "choose", items, false).with_cursor(cursor);
        self.dialog = Some(Dialog::Rollback(picker, api, stage.name, ids));
    }

    fn open_promote(&mut self, ctx: &mut Ctx) {
        let (Some(api), Some(detail), Some(source)) = (self.selected().cloned(), self.detail(), self.acted_stage()) else { return };
        let Some(id) = source.deployment_id.clone() else { return ctx.toast(Level::Info, format!("{} runs no deployment", source.name)) };
        let mut items = Vec::new();
        let mut names = Vec::new();
        for s in detail.stages.iter().filter(|s| s.name != source.name) {
            let now = detail.deployment(s.deployment_id.as_deref()).map(describe).unwrap_or_else(|| "no deployment".into());
            let note = if s.deployment_id.as_ref() == Some(&id) {
                "already runs it".to_string()
            } else if s.auto_deploy {
                "auto-deploy: cannot be pointed by hand".to_string()
            } else {
                format!("runs {now}")
            };
            items.push(Item::new(s.name.clone(), note));
            names.push(s.name.clone());
        }
        if items.is_empty() {
            return ctx.toast(Level::Info, format!("{} has no other stage", api.name));
        }
        // The stage after the source is the usual target (dev → qa → prod).
        let cursor = detail.stages.iter().position(|s| s.name == source.name).map_or(0, |i| i.min(names.len() - 1));
        let picker = Picker::new(format!("Give {}'s deployment {id} to", source.name), "choose", items, false).with_cursor(cursor);
        self.dialog = Some(Dialog::Promote(picker, api, source.name, names));
    }

    fn confirm_point(&mut self, api: Api, stage: String, deployment: String, why: String, ctx: &mut Ctx) {
        let Some(detail) = self.details.get(&api.id).and_then(|d| d.as_ref().ok()) else { return };
        let Some(s) = detail.stage(&stage) else { return };
        if let Some(locked) = apigw::stage_locked(s) {
            self.dialog = None;
            return ctx.toast(Level::Info, locked);
        }
        if s.deployment_id.as_deref() == Some(deployment.as_str()) {
            self.dialog = None;
            return ctx.toast(Level::Info, format!("{stage} already runs {deployment}"));
        }
        let now = detail.deployment(s.deployment_id.as_deref()).map(describe).unwrap_or_else(|| "no deployment".into());
        let to = detail.deployment(Some(&deployment)).map(describe).unwrap_or_else(|| deployment.clone());
        let lines = vec![
            format!("Point {} {stage} from {now}", api.title()),
            format!("to {to}{why}?"),
            "Requests use that deployment's routes and integrations right away.".to_string(),
        ];
        let typed = self.typed_name(&stage).map(|n| (n, TextInput::default()));
        let action = Action::Point { what: format!("{stage} now runs deployment {deployment}"), api: api.clone(), stage: stage.clone(), deployment };
        self.dialog = Some(Dialog::Confirm { title: format!("{} · {stage}", api.title()), lines, typed, action, busy: false, error: None });
    }

    fn run(&mut self, action: Action, ctx: &mut Ctx) {
        let aws = ctx.env().aws.clone();
        match action {
            Action::Point { api, stage, deployment, what } => ctx.spawn(move || {
                let result = apigw::set_stage_deployment(&aws, &api, &stage, &deployment).map(|_| what);
                Msg::Changed { api: api.id, result }
            }),
        }
    }

    fn on_changed(&mut self, api: String, result: aws::Result<String>, ctx: &mut Ctx) {
        match result {
            Ok(text) => {
                self.dialog = None;
                ctx.toast(Level::Success, text);
                self.details.remove(&api);
                if let Some(a) = self.apis.iter().find(|a| a.id == api).cloned() {
                    self.fetch_detail(a, ctx);
                }
            }
            Err(e) => match &mut self.dialog {
                Some(Dialog::Confirm { busy, error, .. }) => {
                    *busy = false;
                    *error = Some(e.short());
                }
                Some(Dialog::Redeploy { form, busy, .. }) => {
                    *busy = false;
                    form.fail(e.short());
                }
                _ => ctx.toast(Level::Error, e.short()),
            },
        }
    }

    fn dialog_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(dialog) = &mut self.dialog else { return };
        match dialog {
            Dialog::Test { busy: true, .. } | Dialog::Redeploy { busy: true, .. } | Dialog::Confirm { busy: true, .. } => {}
            Dialog::Test { form, .. } => match form.handle(key) {
                FormEvent::Submit => self.submit_test(ctx),
                FormEvent::Cancel => self.dialog = None,
                FormEvent::Pending => {}
            },
            Dialog::Redeploy { form, .. } => match form.handle(key) {
                FormEvent::Submit => self.submit_redeploy(ctx),
                FormEvent::Cancel => self.dialog = None,
                FormEvent::Pending => {}
            },
            Dialog::Rollback(picker, api, stage, ids) => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.dialog = None,
                Outcome::Done(chosen) => {
                    let Some(id) = chosen.first().and_then(|&i| ids.get(i)).cloned() else { return };
                    let (api, stage) = (api.clone(), stage.clone());
                    self.confirm_point(api, stage, id, String::new(), ctx);
                }
            },
            Dialog::Promote(picker, api, source, names) => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.dialog = None,
                Outcome::Done(chosen) => {
                    let Some(target) = chosen.first().and_then(|&i| names.get(i)).cloned() else { return };
                    let (api, source) = (api.clone(), source.clone());
                    let id = self.details.get(&api.id).and_then(|d| d.as_ref().ok()).and_then(|d| d.stage(&source)).and_then(|s| s.deployment_id.clone());
                    match id {
                        Some(id) => self.confirm_point(api, target, id, format!(", the deployment {source} runs"), ctx),
                        None => self.dialog = None,
                    }
                }
            },
            Dialog::Export(picker, api, stage) => match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.dialog = None,
                Outcome::Done(chosen) => {
                    let yaml = chosen.first() == Some(&0);
                    let (api, stage) = (api.clone(), stage.clone());
                    self.dialog = None;
                    ctx.toast(Level::Info, format!("Exporting {} {stage}…", api.name));
                    let aws = ctx.env().aws.clone();
                    ctx.spawn(move || Msg::Exported { result: apigw::export(&aws, &api, &stage, yaml), api: api.name });
                }
            },
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

    // ---- rendering ------------------------------------------------------------------------------

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search APIs");
        f.render_widget(Paragraph::new(line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = match &self.load {
            LoadState::Loading => format!(" {} Loading APIs… ", theme::spinner(view.tick)),
            _ if !self.filter.value.is_empty() => format!(" {} of {} APIs ", self.visible.len(), self.apis.len()),
            _ => format!(" {} APIs ", self.apis.len()),
        };
        if self.visible.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading | LoadState::Idle => Line::styled("Loading APIs…", theme::muted()),
                LoadState::Loaded => Line::styled("No APIs", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|&i| {
                let a = &self.apis[i];
                let tag = format!("{:>4}", a.kind.short());
                let name = ellipsize(&a.name, width.saturating_sub(6));
                let pad = width.saturating_sub(name.chars().count() + tag.chars().count());
                let color = match a.kind {
                    Kind::Rest => theme::ACCENT,
                    Kind::Http => theme::OK,
                    Kind::WebSocket => theme::APPROVAL,
                };
                ListItem::new(Line::from(vec![Span::raw(name), Span::raw(" ".repeat(pad)), Span::styled(tag, Style::new().fg(color))]))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, !self.focused)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    /// The lines over the tabs: what the API is, its stage and endpoint.
    fn header_lines(&self, api: &Api, detail: Option<&ApiDetail>) -> Vec<Line<'static>> {
        let mut info = vec![api.kind.label().to_string(), api.endpoint_type.to_lowercase(), api.id.clone()];
        if let Some(t) = api.created {
            info.push(format!("created {}", fmt_ago(t)));
        }
        let mut lines = vec![Line::styled(info.join(" · "), theme::muted())];
        if let Some(d) = &api.description {
            lines[0].spans.push(Span::raw(format!("  {}", ellipsize(d, 60))));
        }
        let Some(detail) = detail else { return lines };
        match self.stage_of(api, detail) {
            Some(stage) => {
                let mut spans = vec![Span::styled("Stage ", theme::muted()), Span::styled(stage.name.clone(), theme::accent().bold())];
                if detail.stages.len() > 1 {
                    spans.push(Span::styled(" (s changes)", theme::muted()));
                }
                if let Some(d) = detail.deployment(stage.deployment_id.as_deref()) {
                    spans.push(Span::styled(format!(" · deployment {}", describe(d)), theme::muted()));
                }
                if stage.auto_deploy {
                    spans.push(Span::styled(" · auto-deploy", theme::muted()));
                }
                lines.push(Line::from(spans));
                let mut endpoint = vec![Span::styled(api.invoke_url(&stage.name, &self.region), theme::muted())];
                if api.default_endpoint_disabled {
                    endpoint.push(Span::styled("  disabled: only custom domains reach it", Style::new().fg(theme::WARN)));
                }
                lines.push(Line::from(endpoint));
            }
            None => lines.push(Line::styled("No stages: the API was never deployed", Style::new().fg(theme::WARN))),
        }
        lines
    }

    fn tabs_line(&self, detail: Option<&ApiDetail>) -> Line<'static> {
        let count = |n: Option<usize>| n.map(|n| format!(" {n}")).unwrap_or_default();
        let mut spans = Vec::new();
        for tab in TABS {
            let extra = match tab {
                Tab::Routes => {
                    let open = detail.map(|d| d.routes.iter().filter(|r| apigw::unprotected(self.selected().map_or(Kind::Rest, |a| a.kind), r)).count());
                    format!("{}{}", count(detail.map(|d| d.routes.len())), if open.unwrap_or(0) > 0 { " ⚠" } else { "" })
                }
                Tab::Stages => count(detail.map(|d| d.stages.len())),
                Tab::Domains => {
                    let id = self.selected().map(|a| a.id.clone()).unwrap_or_default();
                    match &self.domains {
                        Some(Ok(domains)) => count(Some(domains.iter().flat_map(|d| &d.mappings).filter(|m| m.api_id == id).count())),
                        _ => String::new(),
                    }
                }
                Tab::Metrics => String::new(),
            };
            let style = if tab == self.tab { Style::new().fg(Color::Black).bg(theme::ACCENT).bold() } else { theme::muted() };
            spans.push(Span::styled(format!(" {}{extra} ", tab.label()), style));
            spans.push(Span::raw(" "));
        }
        Line::from(spans)
    }

    fn render_detail(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let Some(api) = self.selected().cloned() else {
            f.render_widget(widgets::placeholder(Line::styled("Select an API", theme::muted())).block(widgets::panel(" API ", false)), area);
            return;
        };
        let mut block = widgets::panel(Line::from(format!(" {} ", api.name)).bold(), self.focused);
        if self.reporting {
            block = block.title_top(
                Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::styled(" writing the report ", theme::muted())])
                    .right_aligned(),
            );
        }
        let inner = block.inner(area);
        f.render_widget(block, area);
        let detail = match self.details.get(&api.id) {
            None => {
                let text = Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" loading routes and stages…")]);
                let mut lines = self.header_lines(&api, None);
                lines.push(Line::raw(""));
                lines.push(text);
                f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
                return;
            }
            Some(Err(e)) => {
                let mut lines = self.header_lines(&api, None);
                lines.push(Line::raw(""));
                lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR)));
                f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
                return;
            }
            Some(Ok(d)) => d.clone(),
        };
        let header = self.header_lines(&api, Some(&detail));
        let [head, tabs_area, body] =
            inner.layout(&Layout::vertical([Constraint::Length(header.len() as u16), Constraint::Length(2), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(header), head);
        f.render_widget(Paragraph::new(self.tabs_line(Some(&detail))), tabs_area);

        // The Routes view shows the chosen route's detail under the list.
        let route_detail = (self.tab == Tab::Routes && self.focused).then(|| self.current_route()).flatten();
        let (list_area, detail_area) = match &route_detail {
            Some(_) if body.height >= 16 => {
                let [top, bottom] = body.layout(&Layout::vertical([Constraint::Percentage(50), Constraint::Fill(1)]));
                (top, Some(bottom))
            }
            _ => (body, None),
        };
        let (lines, rows) = self.tab_lines(&api, &detail, list_area.width as usize, view);
        if self.focused
            && let Some(&at) = rows.get(self.cursor)
        {
            let at = at as u16;
            let height = list_area.height.max(1);
            if at < self.scroll {
                self.scroll = at;
            } else if at >= self.scroll + height {
                self.scroll = at + 1 - height;
            }
        }
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.scroll, 0)), list_area);
        if let (Some(route), Some(area)) = (route_detail, detail_area) {
            let lines = self.route_lines(&api, &detail, &route);
            let text = [vec![Line::styled("─".repeat(area.width as usize), theme::muted())], lines].concat();
            f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
        }
    }

    /// The lines of the current view, and the line where each of its rows starts.
    fn tab_lines(&self, api: &Api, detail: &ApiDetail, width: usize, view: &View) -> (Vec<Line<'static>>, Vec<usize>) {
        match self.tab {
            Tab::Routes => self.routes_view(api, detail, width, view),
            Tab::Stages => self.stages_view(api, detail),
            Tab::Metrics => (self.metrics_view(api, detail, width, view), Vec::new()),
            Tab::Domains => self.domains_view(api, view),
        }
    }

    fn routes_view(&self, api: &Api, detail: &ApiDetail, width: usize, view: &View) -> (Vec<Line<'static>>, Vec<usize>) {
        let mut lines = Vec::new();
        let mut rows = Vec::new();
        let stage = self.stage_of(api, detail);
        let stats = stage.and_then(|s| self.stats.get(&(api.id.clone(), s.name.clone(), self.range)));
        match (stage, stats) {
            (Some(s), _) if !s.detailed_metrics => lines.push(Line::styled(
                format!("Per-route numbers need detailed metrics on {} · m 1h/24h/7d", s.name),
                theme::muted(),
            )),
            (Some(s), None) => lines.push(Line::from(vec![
                Span::styled(theme::spinner(view.tick), theme::accent()),
                Span::styled(format!(" reading {}'s numbers per route ({})…", s.name, self.range.label()), theme::muted()),
            ])),
            (Some(s), Some((_, Err(e)))) => lines.push(Line::styled(format!("{}: {}", s.name, e.short()), Style::new().fg(theme::ERR))),
            (Some(s), Some((_, Ok(_)))) => lines.push(Line::styled(format!("{} · last {} · m changes", s.name, self.range.label()), theme::muted())),
            (None, _) => {}
        }
        if detail.routes.is_empty() {
            lines.push(Line::styled("No routes", theme::muted()));
            return (lines, rows);
        }
        let stats = stats.and_then(|(_, r)| r.as_ref().ok());
        let ws = api.kind == Kind::WebSocket;
        let method_w = if ws { 0 } else { detail.routes.iter().map(|r| r.method.chars().count()).max().unwrap_or(3).max(3) + 1 };
        let target_w = (width / 3).clamp(14, 34);
        let mut last_path: Option<&str> = None;
        for (i, r) in detail.routes.iter().enumerate() {
            if !ws && last_path != Some(r.path.as_str()) {
                lines.push(Line::styled(r.path.clone(), theme::bold()));
                last_path = Some(&r.path);
            }
            let selected = self.focused && i == self.cursor;
            let chosen = !self.focused && self.routes.get(&api.id).is_some_and(|k| *k == r.key());
            let marker = if selected { "▶ " } else if chosen { "• " } else { "  " };
            let mut spans = vec![Span::styled(marker, theme::accent())];
            if ws {
                spans.push(Span::styled(format!("{:<18}", ellipsize(&r.path, 18)), theme::bold()));
            } else {
                // An HTTP API's `$default` route takes every method.
                let method = if r.method.is_empty() { "*" } else { r.method.as_str() };
                spans.push(Span::styled(format!("{method:<method_w$}"), Style::new().fg(method_color(method)).bold()));
            }
            let target = r.integration.as_ref().map_or_else(|| "—".to_string(), |i| i.target().label());
            spans.push(Span::raw(format!(" {:<target_w$}", ellipsize(&target, target_w))));
            let (auth, style) = auth_look(api.kind, r);
            spans.push(Span::styled(format!(" {:<10}", ellipsize(&auth, 10)), style));
            if let Some(s) = stats.and_then(|s| s.get(&r.key())) {
                spans.push(Span::styled(format!(" {:>6} req", fmt_count(s.count)), theme::muted()));
                if s.count > 0.0 {
                    let rate = s.errors / s.count * 100.0;
                    let style = if rate >= 1.0 { Style::new().fg(theme::ERR) } else { theme::muted() };
                    spans.push(Span::styled(format!("  5xx {rate:.1}%"), style));
                }
                if let Some(p) = s.p99 {
                    spans.push(Span::styled(format!("  p99 {}", fmt_ms(p)), theme::muted()));
                }
            }
            rows.push(lines.len());
            let line = Line::from(spans);
            lines.push(if selected { line.patch_style(theme::selected()) } else { line });
        }
        (lines, rows)
    }

    fn route_lines(&self, api: &Api, detail: &ApiDetail, r: &Route) -> Vec<Line<'static>> {
        let kw = 13;
        let mut lines = vec![Line::from(vec![
            Span::styled(r.key(), theme::accent().bold()),
            Span::styled(r.operation_name.as_ref().map(|o| format!("  · {o}")).unwrap_or_default(), theme::muted()),
        ])];
        let (label, style) = auth_look(api.kind, r);
        let mut auth = vec![Span::styled(format!("{:<kw$} ", "Authorization"), theme::muted()), Span::styled(label, style)];
        if let Some(a) = detail.authorizer(r.authorizer_id.as_deref()) {
            let mut what = format!(" · {} ({})", a.name, a.kind.to_lowercase().replace('_', " "));
            if let Some(f) = &a.function {
                what.push_str(&format!(" · λ {f}"));
            }
            if let Some(p) = a.providers.first() {
                what.push_str(&format!(" · {}", p.rsplit('/').next().unwrap_or(p)));
            }
            if let Some(i) = &a.issuer {
                what.push_str(&format!(" · {i}"));
            }
            if !a.audience.is_empty() {
                what.push_str(&format!(" · audience {}", a.audience.join(", ")));
            }
            if let Some(ttl) = a.ttl.filter(|t| *t > 0) {
                what.push_str(&format!(" · cached {ttl} s"));
            }
            if !a.identity_source.is_empty() {
                what.push_str(&format!(" · from {}", a.identity_source.join(", ")));
            }
            auth.push(Span::styled(what, theme::muted()));
        }
        if apigw::unprotected(api.kind, r) {
            auth.push(Span::styled("  anyone can call it", Style::new().fg(theme::WARN)));
        }
        lines.push(Line::from(auth));
        if r.api_key_required {
            lines.push(widgets::kv("API key", "required", kw));
        }
        if let Some(v) = detail.validator(r.request_validator_id.as_deref()) {
            let what = [v.body.then_some("body"), v.parameters.then_some("parameters")].into_iter().flatten().collect::<Vec<_>>().join(" and ");
            lines.push(widgets::kv("Validator", format!("{} ({})", v.name, if what.is_empty() { "nothing" } else { &what }), kw));
        }
        if !r.request_models.is_empty() {
            let models: Vec<String> = r.request_models.iter().map(|(t, m)| format!("{m} ({t})")).collect();
            lines.push(widgets::kv("Models", models.join(", "), kw));
        }
        if !r.request_parameters.is_empty() {
            lines.push(widgets::kv("Required", r.request_parameters.join(", "), kw));
        }
        match &r.integration {
            Some(i) => {
                let target = i.target();
                let open = if target.jump().is_some() { "  Enter opens it" } else { "" };
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<kw$} ", "Integration"), theme::muted()),
                    Span::raw(format!("{} → ", i.kind)),
                    Span::styled(target.label(), theme::bold()),
                    Span::styled(open, theme::muted()),
                ]));
                let mut more = Vec::new();
                if let Some(m) = i.http_method.as_ref().filter(|_| matches!(i.kind.as_str(), "AWS" | "HTTP")) {
                    more.push(format!("calls with {m}"));
                }
                if let Some(t) = i.timeout_ms {
                    more.push(format!("timeout {}", fmt_ms(t as f64)));
                }
                if let Some(p) = &i.payload_format {
                    more.push(format!("payload {p}"));
                }
                if let Some(c) = &i.connection_type.as_ref().filter(|c| *c != "INTERNET") {
                    more.push(format!("{} {}", c.to_lowercase().replace('_', " "), i.connection_id.clone().unwrap_or_default()));
                }
                if let Some(role) = &i.credentials {
                    more.push(format!("role {}", role.rsplit('/').next().unwrap_or(role)));
                }
                if let Some(p) = &i.passthrough {
                    more.push(format!("passthrough {}", p.to_lowercase().replace('_', " ")));
                }
                if !more.is_empty() {
                    lines.push(widgets::kv("", more.join(" · "), kw));
                }
                if let Some(uri) = i.uri.as_ref().filter(|_| matches!(target, apigw::Target::Http(_) | apigw::Target::Service(_))) {
                    lines.push(widgets::kv("URI", uri.clone(), kw));
                }
                for (k, v) in &i.request_parameters {
                    lines.push(widgets::kv("Parameter", format!("{k} = {v}"), kw));
                }
                for (content_type, template) in &i.templates {
                    lines.push(widgets::kv("Template", Span::styled(content_type.clone(), theme::accent()), kw));
                    let all: Vec<&str> = template.lines().collect();
                    for l in all.iter().take(TEMPLATE_LINES) {
                        lines.push(Line::styled(format!("{:kw$}  {}", "", l.replace('\t', "  ")), theme::muted()));
                    }
                    if all.len() > TEMPLATE_LINES {
                        lines.push(Line::styled(format!("{:kw$}  … {} more lines", "", all.len() - TEMPLATE_LINES), theme::muted()));
                    }
                }
            }
            None => lines.push(widgets::kv("Integration", "none", kw)),
        }
        lines
    }

    fn stages_view(&self, api: &Api, detail: &ApiDetail) -> (Vec<Line<'static>>, Vec<usize>) {
        let mut lines = Vec::new();
        let mut rows = Vec::new();
        if detail.stages.is_empty() {
            lines.push(Line::styled("No stages: the API was never deployed", theme::muted()));
            return (lines, rows);
        }
        let current = self.stage_of(api, detail).map(|s| s.name.clone());
        let name_w = detail.stages.iter().map(|s| s.name.chars().count()).max().unwrap_or(5).max(5) + 2;
        lines.push(Line::styled(format!("  {:<name_w$}{:<12}{:<10}{}", "stage", "deployment", "deployed", "description"), theme::muted()));
        for (i, s) in detail.stages.iter().enumerate() {
            let selected = self.focused && i == self.cursor;
            let d = detail.deployment(s.deployment_id.as_deref());
            let same = detail.stages[..i].iter().rev().find(|o| o.deployment_id.is_some() && o.deployment_id == s.deployment_id);
            let mut spans = vec![
                Span::styled(if selected { "▶ " } else { "  " }, theme::accent()),
                Span::styled(
                    format!("{:<name_w$}", format!("{}{}", s.name, if current.as_deref() == Some(&s.name) { " ●" } else { "" })),
                    theme::bold(),
                ),
                Span::raw(format!("{:<12}", s.deployment_id.as_deref().unwrap_or("—"))),
                Span::styled(format!("{:<10}", d.and_then(|d| d.created).map(fmt_ago).unwrap_or_default()), theme::muted()),
            ];
            match same {
                Some(o) => spans.push(Span::styled(format!("= {}", o.name), theme::muted())),
                None => spans.push(Span::raw(ellipsize(&d.and_then(|d| d.description.clone()).unwrap_or_default(), 30))),
            }
            if let Some(prev) = i.checked_sub(1).map(|p| &detail.stages[p])
                && let Some(n) = detail.behind(s, prev).filter(|n| *n > 0)
            {
                spans.push(Span::styled(format!("  {n} behind {}", prev.name), Style::new().fg(theme::WARN)));
            }
            if s.auto_deploy {
                spans.push(Span::styled("  auto-deploy", theme::muted()));
            }
            rows.push(lines.len());
            let line = Line::from(spans);
            lines.push(if selected { line.patch_style(theme::selected()) } else { line });
        }
        let shown = if self.focused { detail.stages.get(self.cursor) } else { self.stage_of(api, detail) };
        if let Some(s) = shown {
            lines.push(Line::raw(""));
            lines.extend(self.stage_lines(api, detail, s));
        }
        (lines, rows)
    }

    fn stage_lines(&self, api: &Api, detail: &ApiDetail, s: &Stage) -> Vec<Line<'static>> {
        let kw = 13;
        let mut lines = vec![widgets::section(&format!("Stage {}", s.name))];
        if let Some(d) = &s.description {
            lines.push(widgets::kv("Description", d.clone(), kw));
        }
        match detail.deployment(s.deployment_id.as_deref()) {
            Some(d) => lines.push(widgets::kv("Deployment", describe(d), kw)),
            None => lines.push(widgets::kv("Deployment", s.deployment_id.clone().unwrap_or_else(|| "—".into()), kw)),
        }
        if let Some(msg) = &s.last_deployment_message {
            lines.push(widgets::kv("", Span::styled(msg.clone(), theme::muted()), kw));
        }
        let dates = [s.created.map(|t| format!("created {}", fmt_ago(t))), s.updated.map(|t| format!("changed {}", fmt_ago(t)))];
        let dates: Vec<String> = dates.into_iter().flatten().collect();
        if !dates.is_empty() {
            lines.push(widgets::kv("Stage", dates.join(" · "), kw));
        }
        if let (Some(rate), Some(burst)) = (s.throttle_rate, s.throttle_burst) {
            lines.push(widgets::kv("Throttling", format!("{rate} requests/s · burst {burst}"), kw));
        }
        if let Some((size, status)) = &s.cache {
            let methods = if s.cached_methods.is_empty() { "no method caches".to_string() } else { format!("methods {}", s.cached_methods.join(", ")) };
            lines.push(widgets::kv("Cache", format!("{size} GB · {} · {methods}", status.to_lowercase()), kw));
        }
        let mut logs = Vec::new();
        if let Some(level) = &s.logging_level {
            logs.push(format!("execution {}{}", level.to_lowercase(), if s.data_trace { " + full requests" } else { "" }));
        }
        if let Some(g) = s.access_log_group() {
            logs.push(format!("access {g}"));
        } else if s.access_log.is_some() {
            logs.push("access to Firehose".into());
        }
        lines.push(widgets::kv("Logs", if logs.is_empty() { "off".to_string() } else { format!("{}  l opens", logs.join(" · ")) }, kw));
        if let Some(format) = &s.access_log_format {
            lines.push(widgets::kv("", Span::styled(format!("format {}", ellipsize(format, 90)), theme::muted()), kw));
        }
        lines.push(widgets::kv("Route metrics", if s.detailed_metrics { "on" } else { "off" }, kw));
        if s.tracing {
            lines.push(widgets::kv("Tracing", "X-Ray", kw));
        }
        if let Some(waf) = &s.web_acl {
            lines.push(widgets::kv("WAF", waf.rsplit('/').nth(1).unwrap_or(waf).to_string(), kw));
        }
        if let Some((pct, dep)) = &s.canary {
            lines.push(widgets::kv("Canary", format!("{pct}% of requests → deployment {}", dep.clone().unwrap_or_default()), kw));
        }
        lines.push(widgets::kv("URL", api.invoke_url(&s.name, &self.region), kw));
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            widgets::section(&format!("Variables ({})", s.variables.len())).spans.remove(0),
            Span::styled(if self.reveal { "   v hide" } else { "   v reveal" }, theme::muted()),
        ]));
        if s.variables.is_empty() {
            lines.push(Line::styled("none", theme::muted()));
        }
        let key_w = s.variables.keys().map(|k| k.chars().count()).max().unwrap_or(0).min(32);
        for (k, v) in &s.variables {
            let value = if self.reveal { Span::raw(v.clone()) } else { Span::styled("••••••••", theme::muted()) };
            lines.push(Line::from(vec![Span::styled(format!("{k:<key_w$}  "), theme::accent()), value]));
        }
        lines.push(Line::raw(""));
        lines.push(widgets::section(&format!("Deployments ({})", detail.deployments.len())));
        for d in detail.deployments.iter().take(12) {
            let on: Vec<&str> = detail.stages.iter().filter(|x| x.deployment_id.as_ref() == Some(&d.id)).map(|x| x.name.as_str()).collect();
            let mut spans = vec![
                Span::raw(format!("{:<10}", d.id)),
                Span::styled(format!("{:<10}", d.created.map(fmt_ago).unwrap_or_default()), theme::muted()),
                Span::raw(ellipsize(&d.description.clone().unwrap_or_default(), 34)),
            ];
            if d.auto {
                spans.push(Span::styled(" auto", theme::muted()));
            }
            if !on.is_empty() {
                spans.push(Span::styled(format!("  ← {}", on.join(", ")), theme::accent()));
            }
            lines.push(Line::from(spans));
        }
        if detail.deployments.len() > 12 {
            lines.push(Line::styled(format!("… {} older", detail.deployments.len() - 12), theme::muted()));
        }
        lines.push(Line::raw(""));
        lines.push(widgets::hints_line(&[("d", "redeploy"), ("b", "roll back"), ("p", "promote to another stage"), ("y", "copy URL")]));
        lines
    }

    fn metrics_view(&self, api: &Api, detail: &ApiDetail, width: usize, view: &View) -> Vec<Line<'static>> {
        let mut ranges = vec![widgets::section("Metrics").spans.remove(0), Span::raw("   ")];
        for r in [Range::Hour, Range::Day, Range::Week] {
            let style = if r == self.range { theme::accent().bold() } else { theme::muted() };
            ranges.push(Span::styled(format!("{} ", r.label()), style));
        }
        ranges.push(Span::styled("  m changes · s stage", theme::muted()));
        let mut lines = vec![Line::from(ranges)];
        let Some(stage) = self.stage_of(api, detail) else {
            lines.push(Line::styled("No stages: the API was never deployed", theme::muted()));
            return lines;
        };
        lines.push(Line::raw(""));
        lines.push(Line::styled(format!("Stage {}", stage.name), theme::bold()));
        lines.extend(self.charts(api, stage, None, width, view));
        match self.current_route() {
            Some(r) if stage.detailed_metrics => {
                lines.push(Line::raw(""));
                lines.push(Line::styled(format!("Route {}", r.key()), theme::bold()));
                lines.extend(self.charts(api, stage, Some(&r), width, view));
            }
            Some(r) => lines.push(Line::styled(format!("{}: per-route metrics are off on {}", r.key(), stage.name), theme::muted())),
            None => lines.push(Line::styled("Choose a route on Routes to see its own numbers here", theme::muted())),
        }
        lines
    }

    fn charts(&self, api: &Api, stage: &Stage, route: Option<&Route>, width: usize, view: &View) -> Vec<Line<'static>> {
        let key = (api.id.clone(), stage.name.clone(), route.map(Route::key), self.range);
        let m = match self.metrics.get(&key) {
            Some((_, Ok(m))) => m,
            Some((_, Err(e))) => return vec![Line::styled(e.short(), Style::new().fg(theme::ERR))],
            None => return vec![Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" loading metrics…")])],
        };
        if m.points("count").is_empty() {
            return vec![Line::styled(format!("No requests in the last {}.", self.range.label()), theme::muted())];
        }
        let mut lines = Vec::new();
        let err = Style::new().fg(theme::ERR);
        let pct = |id: &str| m.rate(id).map(|r| format!(" ({:.2}%)", r * 100.0)).unwrap_or_default();
        let errors = m.total("5xx");
        // WebSocket APIs count integration errors, not status codes.
        let what = if api.kind == Kind::WebSocket { "integration errors" } else { "5xx" };
        let mut summary = vec![Span::styled(format!("{} total", fmt_count(m.total("count"))), theme::muted())];
        if errors > 0.0 {
            summary.push(Span::styled(format!(" · {} {what}{}", fmt_count(errors), pct("5xx")), err));
        } else {
            summary.push(Span::styled(format!(" · no {what}"), Style::new().fg(theme::OK)));
        }
        let title = if api.kind == Kind::WebSocket { "Messages" } else { "Requests" };
        let mut requests = Chart::new(title, m.points("count"), m.window);
        requests.summary = summary;
        requests.stacked = Some(m.points("5xx"));
        requests.axis = false;
        // Per period, not per column: a column holds two or three periods, and sums would zigzag.
        requests.agg = Agg::Mean;
        lines.extend(requests.lines(width));
        let mut client = Chart::new(if api.kind == Kind::WebSocket { "Client errors" } else { "4xx" }, m.points("4xx"), m.window);
        client.color = theme::WARN;
        client.rows = 2;
        client.axis = false;
        client.agg = Agg::Mean;
        client.summary = vec![Span::styled(format!("{}{}", fmt_count(m.total("4xx")), pct("4xx")), theme::muted())];
        lines.extend(client.lines(width));
        let (latency_id, latency_title) = if api.kind == Kind::WebSocket { ("integration_p99", "Integration p99") } else { ("p99", "Latency p99") };
        let mut latency = Chart::new(latency_title, m.points(latency_id), m.window);
        latency.agg = Agg::Max;
        latency.color = theme::RUNNING;
        latency.fmt = fmt_ms;
        latency.axis = stage.cache.is_none() || route.is_some();
        let mut parts = Vec::new();
        if let Some(p50) = m.points("p50").iter().map(|p| p.1).reduce(f64::max) {
            parts.push(format!("p50 up to {}", fmt_ms(p50)));
        }
        if let Some(i) = m.mean("integration") {
            parts.push(format!("integration avg {}", fmt_ms(i)));
        }
        if let Some(o) = m.overhead() {
            parts.push(format!("API Gateway adds {}", fmt_ms(o)));
        }
        latency.summary = vec![Span::styled(parts.join(" · "), theme::muted())];
        lines.extend(latency.lines(width));
        if stage.cache.is_some() && route.is_none() {
            let lookups = m.total("hits") + m.total("misses");
            let mut cache = Chart::new("Cache hits", m.points("hits"), m.window);
            cache.color = theme::OK;
            cache.rows = 2;
            cache.agg = Agg::Mean;
            let rate = if lookups > 0.0 { m.total("hits") / lookups } else { 0.0 };
            let style = if lookups > 0.0 && rate < 0.2 { Style::new().fg(theme::WARN) } else { theme::muted() };
            cache.summary = vec![Span::styled(
                if lookups > 0.0 { format!("{:.0}% of lookups", rate * 100.0) } else { "no lookups".into() },
                style,
            )];
            lines.extend(cache.lines(width));
        }
        lines
    }

    fn domains_view(&self, api: &Api, view: &View) -> (Vec<Line<'static>>, Vec<usize>) {
        let mut lines = Vec::new();
        let mut rows = Vec::new();
        let domains = match &self.domains {
            None => {
                lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" loading custom domains…")]));
                return (lines, rows);
            }
            Some(Err(e)) => {
                lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR)));
                return (lines, rows);
            }
            Some(Ok(d)) => d,
        };
        if domains.is_empty() {
            lines.push(Line::styled("No custom domain names in this account", theme::muted()));
            return (lines, rows);
        }
        let name_of = |id: &str| self.apis.iter().find(|a| a.id == id).map_or_else(|| id.to_string(), |a| a.name.clone());
        let order = self.domain_rows();
        let mut last: Option<usize> = None;
        for (row, &(d, m)) in order.iter().enumerate() {
            let dom = &domains[d];
            if last != Some(d) {
                if last.is_some() {
                    lines.push(Line::raw(""));
                }
                let info = [dom.endpoint_type.as_deref().map(str::to_lowercase), dom.security_policy.clone(), dom.status.as_deref().map(str::to_lowercase)]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ");
                lines.push(Line::from(vec![Span::styled(dom.name.clone(), theme::bold()), Span::styled(format!("  {info}"), theme::muted())]));
                if let Some(t) = &dom.target {
                    lines.push(Line::styled(format!("  DNS → {t}"), theme::muted()));
                }
                if let Some(e) = &dom.error {
                    lines.push(Line::styled(format!("  mappings could not be read: {e}"), Style::new().fg(theme::WARN)));
                }
                last = Some(d);
            }
            let mapping = &dom.mappings[m];
            let selected = self.focused && row == self.cursor;
            let ours = mapping.api_id == api.id;
            let path = if mapping.key.is_empty() { "/".to_string() } else { format!("/{}", mapping.key) };
            let style = if ours { theme::accent() } else { theme::muted() };
            let line = Line::from(vec![
                Span::styled(if selected { "▶ " } else { "  " }, theme::accent()),
                Span::styled(format!("{path:<14}"), style.bold()),
                Span::styled(format!("→ {} · {}", name_of(&mapping.api_id), mapping.stage), style),
                Span::styled(format!("   {}", dom.url(mapping)), theme::muted()),
            ]);
            rows.push(lines.len());
            lines.push(if selected { line.patch_style(theme::selected()) } else { line });
        }
        (lines, rows)
    }

    fn render_dialog(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let color = match &self.dialog {
            Some(Dialog::Test { stage, .. }) => self.dialog_color(Some(&stage.name)),
            Some(Dialog::Redeploy { stage, .. } | Dialog::Rollback(_, _, stage, _)) => self.dialog_color(Some(stage)),
            Some(Dialog::Confirm { typed, .. }) => if typed.is_some() { theme::ERR } else { theme::ACCENT },
            _ => theme::ACCENT,
        };
        let busy_popup = |f: &mut Frame, what: &str| {
            let body = Text::from(vec![
                Line::raw(""),
                Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw(what.to_string())]),
            ]);
            widgets::popup(f, widgets::popup_area(area, 34, 5), "API Gateway", theme::ACCENT, body);
        };
        match &mut self.dialog {
            None => {}
            Some(Dialog::Test { form, busy, .. }) => {
                form.render(f, area, color);
                if *busy {
                    busy_popup(f, "Calling…");
                }
            }
            Some(Dialog::Redeploy { form, busy, .. }) => {
                form.render(f, area, color);
                if *busy {
                    busy_popup(f, "Deploying…");
                }
            }
            Some(Dialog::Rollback(picker, ..) | Dialog::Promote(picker, ..) | Dialog::Export(picker, ..)) => {
                let rect = widgets::popup_area(area, 76, picker.height(area.height.saturating_sub(4)));
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
                let rect = widgets::popup_area(area, 80, widgets::wrapped_height(&text, 76) + 2);
                widgets::popup(f, rect, title, color, Text::from(text));
                if let Some((row, col)) = cursor {
                    f.set_cursor_position((rect.x + 2 + col, rect.y + 1 + row));
                }
            }
        }
    }
}

impl Module for ApiGateway {
    fn title(&self) -> &'static str {
        "API Gateway"
    }

    fn description(&self) -> &'static str {
        "REST, HTTP and WebSocket APIs · routes, stages, metrics, test a route"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = ApiGateway::new();
        self.env_name = ctx.env().name.clone();
        self.region = ctx.env().region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load_apis(ctx);
        }
        // Coming back later shows current numbers.
        self.metrics.retain(|_, (at, _)| at.elapsed() < METRICS_FRESH);
        self.stats.retain(|_, (at, _)| at.elapsed() < METRICS_FRESH);
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some((_, view)) = &mut self.zoom {
            if let LogKey::Copy = view.key(key) {
                let ok = crate::platform::copy_to_clipboard(&view.lines().join("\n"));
                ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Copied" } else { "Clipboard not available" });
            }
            if !view.zoomed {
                self.zoom = None;
            }
            return Handled::Yes;
        }
        if self.dialog.is_some() {
            self.dialog_key(key, ctx);
            return Handled::Yes;
        }
        if self.filtering {
            let selected = self.list_state.selected().unwrap_or(0);
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
        // Keys that work the same with or without the rows focused.
        match key.code {
            KeyCode::Left => self.step_tab(false, ctx),
            KeyCode::Right => self.step_tab(true, ctx),
            KeyCode::Char('c') => self.copy_report(ctx),
            KeyCode::Char('y') => self.copy_row(ctx),
            KeyCode::Char('x') => self.open_export(ctx),
            KeyCode::Char('t') => self.open_test(ctx),
            KeyCode::Char('s') => self.next_stage(ctx),
            KeyCode::Char('m') => {
                self.range = self.range.next();
                self.fetch_view(ctx);
            }
            KeyCode::Char('l') => self.open_logs(ctx),
            KeyCode::Char('d') => self.open_redeploy(ctx),
            KeyCode::Char('b') => self.open_rollback(ctx),
            KeyCode::Char('p') => self.open_promote(ctx),
            KeyCode::Char('v') => self.reveal = !self.reveal,
            KeyCode::Char('o') => {
                if let Some(api) = self.selected() {
                    let url = apigw::console_url(api, &self.region);
                    if !crate::platform::open_url(&url) {
                        ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                    }
                }
            }
            KeyCode::Char('r') => {
                if let Some(api) = self.selected().cloned() {
                    self.details.remove(&api.id);
                    self.metrics.retain(|k, _| k.0 != api.id);
                    self.stats.retain(|k, _| k.0 != api.id);
                    self.domains = None;
                    self.fetch_detail(api, ctx);
                } else {
                    self.load_apis(ctx);
                }
            }
            KeyCode::Char('R') => {
                *self = ApiGateway { env_name: self.env_name.clone(), region: self.region.clone(), ..ApiGateway::new() };
                self.load_apis(ctx);
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            _ => return self.key_rows(key, ctx),
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if self.zoom.is_some() || self.dialog.is_some() || !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                // Rows start below the filter bar and the panel border.
                let row = event.row.saturating_sub(self.list_area.y + 2) as usize;
                self.focused = false;
                self.select(self.list_state.offset() + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    /// Selects API `target` (an id or a name), now or once the list is read.
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        self.pending_open = Some(target.to_string());
        if matches!(self.load, LoadState::Idle) {
            self.load_apis(ctx);
        } else if matches!(self.load, LoadState::Loaded) && !self.try_open() {
            self.pending_open = None;
            ctx.toast(Level::Warn, format!("API not found here: {target}"));
        }
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Apis(result) => match result {
                Ok(apis) => {
                    self.apis = apis;
                    self.load = LoadState::Loaded;
                    self.refilter();
                    if !self.try_open() {
                        if let Some(target) = self.pending_open.take() {
                            ctx.toast(Level::Warn, format!("API not found here: {target}"));
                        }
                        self.select(self.list_state.selected().unwrap_or(0));
                    }
                }
                Err(e) => self.load = LoadState::Failed(e),
            },
            Msg::Detail { api, result } => {
                self.in_flight.remove(&("detail", api.clone()));
                self.details.insert(api, *result);
                self.cursor = self.cursor.min(self.row_count().saturating_sub(1));
                self.fetch_view(ctx);
            }
            Msg::Metrics { key, result } => {
                self.in_flight.remove(&("metrics", format!("{key:?}")));
                self.metrics.insert(key, (Instant::now(), *result));
            }
            Msg::Stats { key, result } => {
                self.in_flight.remove(&("stats", format!("{key:?}")));
                self.stats.insert(key, (Instant::now(), result));
            }
            Msg::Domains(result) => {
                self.in_flight.remove(&("domains", String::new()));
                self.domains = Some(result);
            }
            Msg::Tested { title, result } => self.on_tested(title, *result, ctx),
            Msg::Exported { api, result } => match result {
                Ok(text) => copy(ctx, &format!("apigw-{api}-openapi"), &text, &format!("the OpenAPI definition of {api}")),
                Err(e) => ctx.toast(Level::Error, format!("Could not export: {}", e.short())),
            },
            Msg::Report { what, result } => {
                self.reporting = false;
                match result {
                    Ok(md) => copy(ctx, "apigw-report", &md, &format!("{what}. Paste it into your AI agent.")),
                    Err(e) => ctx.toast(Level::Error, format!("Could not write the report: {}", e.short())),
                }
            }
            Msg::Changed { api, result } => self.on_changed(api, result, ctx),
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if let Some((id, at)) = &self.pending
            && at.elapsed() >= DEBOUNCE
        {
            let id = id.clone();
            self.pending = None;
            if let Some(api) = self.selected().filter(|a| a.id == id).cloned()
                && !self.details.contains_key(&id)
            {
                self.fetch_detail(api, ctx);
            }
        }
        // Views read what they show once the selection settles.
        if active && self.pending.is_none() && self.detail().is_some() {
            self.fetch_view(ctx);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some((title, zoomed)) = &mut self.zoom {
            let title = title.clone();
            zoomed.render_zoomed(f, area, &title);
            return;
        }
        let [left, right] = if area.width < 100 {
            area.layout(&Layout::vertical([Constraint::Percentage(30), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(26), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        self.render_detail(f, right, view);
        self.render_dialog(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.zoom.is_some() {
            return vec![("/", "search"), ("y", "copy"), ("Esc", "back")];
        }
        match &self.dialog {
            Some(Dialog::Test { .. }) => return vec![("Enter", "send"), ("Tab", "next field"), ("Esc", "cancel")],
            Some(Dialog::Redeploy { .. }) => return vec![("Enter", "deploy"), ("Esc", "cancel")],
            Some(Dialog::Confirm { .. }) => return vec![("Enter", "confirm"), ("Esc", "cancel")],
            Some(_) => return vec![("↑↓", "move"), ("Enter", "choose"), ("Esc", "cancel")],
            None => {}
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        if self.focused {
            let mut hints = vec![("↑↓", "row"), ("←→", "view")];
            match self.tab {
                Tab::Routes => hints.extend([("Enter", "open integration"), ("t", "test"), ("y", "copy curl"), ("c", "copy for AI")]),
                Tab::Stages => hints.extend([("Enter", "show this stage"), ("d", "redeploy"), ("b", "roll back"), ("p", "promote"), ("v", "variables"), ("l", "logs")]),
                Tab::Domains => hints.extend([("Enter", "open API"), ("y", "copy URL")]),
                Tab::Metrics => {}
            }
            hints.push(("Esc", "APIs"));
            return hints;
        }
        vec![
            ("↑↓", "API"),
            ("←→", "view"),
            ("Enter", "rows"),
            ("s", "stage"),
            ("m", "1h/24h/7d"),
            ("t", "test route"),
            ("y", "copy curl"),
            ("c", "copy for AI"),
            ("x", "export OpenAPI"),
            ("l", "logs"),
            ("/", "search"),
            ("o", "console"),
            ("r", "reload"),
        ]
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || !self.in_flight.is_empty()
            || self.pending.is_some()
            || self.reporting
            || matches!(self.dialog, Some(Dialog::Test { busy: true, .. } | Dialog::Redeploy { busy: true, .. } | Dialog::Confirm { busy: true, .. }))
    }

    fn capturing(&self) -> bool {
        self.filtering || self.dialog.is_some() || self.zoom.is_some()
    }

    fn wants_mouse(&self) -> bool {
        self.zoom.is_none()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.dialog {
            Some(Dialog::Test { form, busy: false, .. } | Dialog::Redeploy { form, busy: false, .. }) => form.paste(text),
            Some(Dialog::Rollback(picker, ..) | Dialog::Promote(picker, ..) | Dialog::Export(picker, ..)) => picker.paste(text),
            Some(Dialog::Confirm { typed: Some((_, input)), busy: false, .. }) => input.insert_str(text, false),
            Some(_) => {}
            None if self.filtering => {
                self.filter.insert_str(text, false);
                self.refilter();
            }
            None => {}
        }
    }
}

impl ApiGateway {
    /// Moving through the APIs, or through the rows of the view.
    fn key_rows(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.focused {
            let count = self.row_count();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(count.saturating_sub(1)),
                KeyCode::Enter => self.activate_row(ctx),
                KeyCode::Esc | KeyCode::Backspace => {
                    self.remember_route();
                    self.focused = false;
                }
                _ => return Handled::No,
            }
            if self.tab == Tab::Routes {
                self.remember_route();
            }
            return Handled::Yes;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::Enter => {
                let count = self.row_count();
                if count > 0 {
                    self.focused = true;
                    // On Routes, start on the route chosen before.
                    let chosen = self.selected().and_then(|a| self.routes.get(&a.id)).cloned();
                    let at = match (self.tab, chosen, self.detail()) {
                        (Tab::Routes, Some(k), Some(d)) => d.routes.iter().position(|r| r.key() == k),
                        _ => None,
                    };
                    self.cursor = at.unwrap_or(self.cursor).min(count - 1);
                } else if self.tab == Tab::Metrics {
                    self.set_tab(Tab::Routes, ctx);
                    self.focused = self.row_count() > 0;
                }
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }
}

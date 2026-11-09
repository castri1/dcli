//! Lambda explorer: list functions, search, and inspect configuration, environment
//! variables (masked until revealed), layers, VPC, tags; charts of their metrics; invoke
//! them with a test payload; read the memory they use from their logs; and copy a report
//! for an AI agent.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use super::savings::{chosen_estimate, memory_form, update_memory_form};
use crate::aws::lambda::{Invocation, Metrics, Range};
use crate::aws::{self, Aws, AwsError, lambda};
use crate::config::is_production;
use crate::savings::DEFAULT_DAYS;
use crate::savings::lambda::{self as memory, Estimate, MemoryRun};
use crate::tui::chart::{Agg, Chart, fmt_count, fmt_ms};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::logview::{LogKey, LogView};
use crate::testevents::{self, Event, Place};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, fmt_bytes, now_secs, parse_rfc3339, pretty_json};

const DEBOUNCE: Duration = Duration::from_millis(200);
/// Metrics older than this are read again when the module is opened again.
const METRICS_FRESH: Duration = Duration::from_secs(120);

enum Msg {
    Page(Vec<lambda::FunctionConfiguration>),
    Done,
    Failed(AwsError),
    Detail { name: String, result: Box<aws::Result<lambda::FunctionDetail>> },
    Metrics { name: String, range: Range, result: Box<aws::Result<Metrics>> },
    Invoked { name: String, payload: String, result: Box<aws::Result<Invocation>> },
    SharedEvents { name: String, result: aws::Result<Vec<Event>> },
    EventSaved { name: String, event: Event, result: aws::Result<()> },
    EventDeleted { name: String, event: String, result: aws::Result<bool> },
    Report { name: String, result: aws::Result<String> },
    MemoryEstimate { name: String, result: aws::Result<Estimate> },
    Memory { name: String, result: aws::Result<MemoryRun> },
}

/// Confirming a memory analysis: what Logs Insights would scan.
struct MemoryCheck {
    name: String,
    estimate: Estimate,
    form: Form,
}

/// The invoke form, while it is open.
struct Invoke {
    name: String,
    form: Form,
    /// What is running: invoking, or saving a shared event.
    busy: Option<&'static str>,
}

/// A function's shared test events (the console's), as far as they are known.
enum Shared {
    Loading,
    Loaded(Vec<Event>),
    Failed,
}

/// The saved test events of a function, to pick one. Item 0 is a new event; item `i` is
/// `events[i - 1]`.
struct EventList {
    name: String,
    picker: Picker,
    events: Vec<Event>,
}

/// Confirming the removal of a shared test event.
struct DeleteEvent {
    name: String,
    event: String,
    form: Form,
    busy: bool,
}

const PLACES: &[&str] = &["this machine", "shared in AWS"];
const INVOKE_NOTE: &str = "Enter runs the function now, for real: whatever it does (writes, messages, charges) happens. \
    Ctrl+S keeps this payload as a test event, under the name in Save as, without invoking. Ctrl+O lists the saved ones.";

fn event_items(events: &[Event]) -> Vec<Item> {
    let mut items = vec![Item::new("New event", "write a payload, starting from the last one sent")];
    items.extend(events.iter().map(|e| {
        let item = Item::new(e.name.clone(), format!("{} · {}", e.place.label(), ellipsize(&e.compact(), 90)));
        if e.place == Place::Shared { item.color(theme::APPROVAL) } else { item }
    }));
    items
}

pub struct Lambda {
    functions: Vec<lambda::FunctionConfiguration>,
    load: LoadState,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    list_state: ListState,
    details: HashMap<String, aws::Result<lambda::FunctionDetail>>,
    in_flight: HashSet<String>,
    pending: Option<(String, Instant)>,
    reveal_env: bool,
    detail_scroll: u16,
    region: String,
    env_name: String,
    list_area: Rect,
    /// Function another module asked to show, selected once the list has it.
    open_target: Option<String>,
    range: Range,
    metrics: HashMap<(String, Range), (Instant, aws::Result<Metrics>)>,
    metrics_in_flight: HashSet<(String, Range)>,
    invoke: Option<Invoke>,
    /// The last payload sent to each function, for the next invoke.
    payloads: HashMap<String, String>,
    events: Option<EventList>,
    shared: HashMap<String, Shared>,
    deleting: Option<DeleteEvent>,
    /// An invocation's response and log, full screen.
    result: Option<(String, LogView)>,
    /// Functions whose report is being written.
    reporting: HashSet<String>,
    memory: Option<MemoryCheck>,
    /// Functions whose memory use is being read.
    reading_memory: HashSet<String>,
    /// The environment's client, to find memory analyses already made.
    aws: Option<Arc<Aws>>,
}

impl Lambda {
    pub fn new() -> Lambda {
        Lambda {
            functions: Vec::new(),
            load: LoadState::Idle,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            details: HashMap::new(),
            in_flight: HashSet::new(),
            pending: None,
            reveal_env: false,
            detail_scroll: 0,
            region: String::new(),
            env_name: String::new(),
            list_area: Rect::default(),
            open_target: None,
            range: Range::Day,
            metrics: HashMap::new(),
            metrics_in_flight: HashSet::new(),
            invoke: None,
            payloads: HashMap::new(),
            events: None,
            shared: HashMap::new(),
            deleting: None,
            result: None,
            reporting: HashSet::new(),
            memory: None,
            reading_memory: HashSet::new(),
            aws: None,
        }
    }

    /// Selects the function in `open_target` if it is loaded (clearing a filter that
    /// hides it). True when done.
    fn try_open(&mut self) -> bool {
        let Some(name) = self.open_target.clone() else { return false };
        if !self.functions.iter().any(|f| f.function_name == name) {
            return false;
        }
        if !self.visible.iter().any(|&i| self.functions[i].function_name == name) {
            self.filter.clear();
            self.filtering = false;
            self.rebuild();
        }
        if let Some(pos) = self.visible.iter().position(|&i| self.functions[i].function_name == name) {
            self.select(pos);
        }
        self.open_target = None;
        true
    }

    fn load(&mut self, ctx: &mut Ctx) {
        let env = ctx.env();
        *self = Lambda::new();
        self.region = env.region.clone();
        self.env_name = env.name.clone();
        self.aws = Some(env.aws.clone());
        self.load = LoadState::Loading;
        let aws = env.aws.clone();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let mut marker: Option<String> = None;
            loop {
                match lambda::list_functions(&aws, marker.as_deref()) {
                    Ok((page, next)) => {
                        if !em.emit(Msg::Page(page)) {
                            return;
                        }
                        match next {
                            Some(m) => marker = Some(m),
                            None => break,
                        }
                    }
                    Err(e) => {
                        em.emit(Msg::Failed(e));
                        return;
                    }
                }
            }
            em.emit(Msg::Done);
        });
    }

    fn selected(&self) -> Option<&lambda::FunctionConfiguration> {
        self.functions.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn rebuild(&mut self) {
        let keep = self.selected().map(|f| f.function_name.clone());
        let query = self.filter.value.trim().to_lowercase();
        self.visible = (0..self.functions.len())
            .filter(|&i| {
                let f = &self.functions[i];
                query.is_empty()
                    || matches_filter(&f.function_name, &query)
                    || f.runtime.as_deref().is_some_and(|r| matches_filter(r, &query))
            })
            .collect();
        self.visible.sort_by(|&a, &b| self.functions[a].function_name.cmp(&self.functions[b].function_name));
        let index = keep
            .and_then(|n| self.visible.iter().position(|&i| self.functions[i].function_name == n))
            .unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    fn select(&mut self, index: usize) {
        if self.visible.is_empty() {
            return;
        }
        let index = index.min(self.visible.len() - 1);
        if self.list_state.selected() != Some(index) {
            self.detail_scroll = 0;
        }
        self.list_state.select(Some(index));
        if let Some(f) = self.selected()
            && !self.details.contains_key(&f.function_name)
        {
            self.pending = Some((f.function_name.clone(), Instant::now()));
        }
    }

    fn fetch(&mut self, name: String, ctx: &mut Ctx) {
        if !self.in_flight.insert(name.clone()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = Box::new(lambda::get_function(&aws, &name));
            Msg::Detail { name, result }
        });
    }

    /// Reads the selected function's metrics for the current range, unless they are here.
    fn fetch_metrics(&mut self, ctx: &mut Ctx) {
        let Some(name) = self.selected().map(|f| f.function_name.clone()) else { return };
        let key = (name.clone(), self.range);
        if self.metrics.contains_key(&key) || !self.metrics_in_flight.insert(key) {
            return;
        }
        let (aws, range) = (ctx.env().aws.clone(), self.range);
        ctx.spawn(move || {
            let result = Box::new(lambda::metrics(&aws, &name, range, now_secs()));
            Msg::Metrics { name, range, result }
        });
    }

    /// `i`: the saved test events when the function has any, else the invoke form.
    fn open_invoke(&mut self, ctx: &mut Ctx) {
        let Some(name) = self.selected().map(|f| f.function_name.clone()) else { return };
        self.fetch_shared(&name, ctx);
        if self.saved(&name).is_empty() {
            self.open_form(name, None);
        } else {
            self.open_events(name);
        }
    }

    /// The invoke form, with a saved event or the last payload sent.
    fn open_form(&mut self, name: String, event: Option<Event>) {
        let payload = match &event {
            Some(e) => e.compact(),
            None => self.payloads.get(&name).cloned().unwrap_or_else(|| "{}".into()),
        };
        let (save_as, place) = event.map_or((String::new(), Place::Local), |e| (e.name, e.place));
        let fields = vec![
            Field::fixed("Function", name.clone()),
            Field::text("Payload", payload).hint("JSON; paste works"),
            Field::text("Save as", save_as).optional("a name, to keep this event with Ctrl+S"),
            Field::choice("Keep in", PLACES, place.label()),
        ];
        let mut form = Form::new(format!("Invoke · {name}"), fields).submit_label("invoke").focus_on("Payload").enter_submits();
        form.note = Some(INVOKE_NOTE.into());
        if is_production(&self.env_name) {
            form = form.confirm_with(&self.env_name);
        }
        self.invoke = Some(Invoke { name, form, busy: None });
    }

    // ---- saved test events ------------------------------------------------------------

    /// The saved events of `name`: this machine's, then the shared ones read so far.
    fn saved(&self, name: &str) -> Vec<Event> {
        let mut events = testevents::local(name);
        if let Some(Shared::Loaded(shared)) = self.shared.get(name) {
            events.extend(shared.iter().cloned());
        }
        events
    }

    /// Reads the function's shared events (again: someone may have saved one meanwhile).
    fn fetch_shared(&mut self, name: &str, ctx: &mut Ctx) {
        match self.shared.get(name) {
            Some(Shared::Loading) => return,
            Some(Shared::Loaded(_)) => {}
            _ => {
                self.shared.insert(name.to_string(), Shared::Loading);
            }
        }
        let (aws, name) = (ctx.env().aws.clone(), name.to_string());
        ctx.spawn(move || {
            let result = testevents::shared(&aws, &name);
            Msg::SharedEvents { name, result }
        });
    }

    fn open_events(&mut self, name: String) {
        let events = self.saved(&name);
        let picker = Picker::new(format!("Invoke · {name} · test events · Ctrl+D deletes"), "use", event_items(&events), false);
        self.events = Some(EventList { name, picker, events });
    }

    /// The open list again after its events changed; it closes with the last one.
    fn refresh_events(&mut self, name: &str) {
        if self.events.as_ref().is_none_or(|l| l.name != name) {
            return;
        }
        let events = self.saved(name);
        match &mut self.events {
            Some(list) if !events.is_empty() => {
                list.picker.set_items(event_items(&events));
                list.events = events;
            }
            _ => self.events = None,
        }
    }

    fn on_shared_events(&mut self, name: String, result: aws::Result<Vec<Event>>, ctx: &mut Ctx) {
        let in_form = self.invoke.as_ref().is_some_and(|i| i.name == name);
        let watching = in_form || self.events.as_ref().is_some_and(|l| l.name == name);
        let known = matches!(self.shared.get(&name), Some(Shared::Loaded(_)));
        match result {
            Ok(events) => {
                if in_form && !known && !events.is_empty() {
                    let n = events.len();
                    ctx.toast(Level::Info, format!("{name} has {n} shared test event{} · Ctrl+O lists them", if n == 1 { "" } else { "s" }));
                }
                self.shared.insert(name.clone(), Shared::Loaded(events));
            }
            // What was read before stays.
            Err(_) if known => {}
            Err(e) => {
                self.shared.insert(name.clone(), Shared::Failed);
                if watching {
                    ctx.toast(Level::Warn, format!("The shared test events of {name} could not be read: {}", e.short()));
                }
            }
        }
        self.refresh_events(&name);
    }

    /// Ctrl+S in the invoke form: keeps the payload under the name in Save as.
    fn save_event(&mut self, ctx: &mut Ctx) {
        let Some(invoke) = &mut self.invoke else { return };
        let name = invoke.form.get("Save as").trim().to_string();
        if let Err(e) = testevents::check_name(&name) {
            return invoke.form.fail(e);
        }
        let text = match invoke.form.get("Payload").trim() {
            "" => "{}",
            text => text,
        };
        let payload: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => return invoke.form.fail(format!("The payload is not valid JSON: {e}")),
        };
        let function = invoke.name.clone();
        if invoke.form.get("Keep in") == Place::Local.label() {
            let existed = testevents::local(&function).iter().any(|e| e.name == name);
            return match testevents::save_local(&function, &name, &payload) {
                Ok(()) => ctx.toast(Level::Success, format!("{} the test event {name} on this machine", if existed { "Updated" } else { "Saved" })),
                Err(e) => invoke.form.fail(format!("Not saved: {e}")),
            };
        }
        // A shared event is a write in the account: production asks for its name first.
        if !invoke.form.confirmed() {
            return;
        }
        invoke.busy = Some("Saving…");
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = aws::schemas::save(&aws, &function, &name, &payload);
            Msg::EventSaved { name: function, event: Event { name, place: Place::Shared, payload }, result }
        });
    }

    fn on_event_saved(&mut self, name: String, event: Event, result: aws::Result<()>, ctx: &mut Ctx) {
        if let Some(invoke) = self.invoke.as_mut().filter(|i| i.name == name) {
            invoke.busy = None;
            if let Err(e) = &result {
                return invoke.form.fail(format!("Not saved: {}", e.short()));
            }
        }
        if let Err(e) = result {
            return ctx.toast(Level::Error, format!("The test event {} was not saved: {}", event.name, e.short()));
        }
        ctx.toast(Level::Success, format!("Saved the shared test event {} · the Lambda console lists it too", event.name));
        match self.shared.get_mut(&name) {
            Some(Shared::Loaded(events)) => {
                events.retain(|e| e.name != event.name);
                events.push(event);
                events.sort_by(|a, b| a.name.cmp(&b.name));
            }
            _ => {
                self.shared.remove(&name);
                self.fetch_shared(&name, ctx);
            }
        }
    }

    /// Ctrl+D in the list: removes the event under the cursor; a shared one asks first.
    fn delete_event(&mut self, ctx: &mut Ctx) {
        let Some(list) = &self.events else { return };
        let Some(event) = list.picker.selected().and_then(|i| i.checked_sub(1)).and_then(|i| list.events.get(i)).cloned() else {
            return;
        };
        let name = list.name.clone();
        if event.place == Place::Local {
            match testevents::delete_local(&name, &event.name) {
                Ok(_) => ctx.toast(Level::Success, format!("Deleted the test event {} from this machine", event.name)),
                Err(e) => ctx.toast(Level::Error, format!("Not deleted: {e}")),
            }
            return self.refresh_events(&name);
        }
        let fields = vec![Field::fixed("Function", name.clone()), Field::fixed("Event", event.name.clone())];
        let mut form = Form::new("Delete shared test event", fields).submit_label("delete").enter_submits();
        form.note = Some("It is removed from AWS: nobody sees it any more, in dcli or in the Lambda console.".into());
        if is_production(&self.env_name) {
            form = form.confirm_with(&self.env_name);
        }
        self.deleting = Some(DeleteEvent { name, event: event.name, form, busy: false });
    }

    fn submit_delete(&mut self, ctx: &mut Ctx) {
        let Some(deleting) = &mut self.deleting else { return };
        deleting.busy = true;
        let (aws, name, event) = (ctx.env().aws.clone(), deleting.name.clone(), deleting.event.clone());
        ctx.spawn(move || {
            let result = aws::schemas::delete(&aws, &name, &event);
            Msg::EventDeleted { name, event, result }
        });
    }

    fn on_event_deleted(&mut self, name: String, event: String, result: aws::Result<bool>, ctx: &mut Ctx) {
        self.deleting = None;
        match result {
            Ok(found) => {
                if let Some(Shared::Loaded(events)) = self.shared.get_mut(&name) {
                    events.retain(|e| e.name != event);
                }
                let text = if found { format!("Deleted the shared test event {event}") } else { format!("{event} was already gone") };
                ctx.toast(Level::Success, text);
            }
            Err(e) => ctx.toast(Level::Error, format!("Not deleted: {}", e.short())),
        }
        self.refresh_events(&name);
    }

    fn submit_invoke(&mut self, ctx: &mut Ctx) {
        let Some(invoke) = &mut self.invoke else { return };
        let payload = match invoke.form.get("Payload").trim() {
            "" => "{}".to_string(),
            text => text.to_string(),
        };
        if let Err(e) = serde_json::from_str::<serde_json::Value>(&payload) {
            return invoke.form.fail(format!("The payload is not valid JSON: {e}"));
        }
        invoke.busy = Some("Invoking…");
        let (aws, name) = (ctx.env().aws.clone(), invoke.name.clone());
        ctx.spawn(move || {
            let result = Box::new(lambda::invoke(&aws, &name, &payload));
            Msg::Invoked { name, payload, result }
        });
    }

    fn on_invoked(&mut self, name: String, payload: String, result: aws::Result<Invocation>, ctx: &mut Ctx) {
        let Some(invoke) = &mut self.invoke else { return };
        invoke.busy = None;
        let inv = match result {
            Ok(inv) => inv,
            Err(e) => return invoke.form.fail(e.short()),
        };
        self.invoke = None;
        self.payloads.insert(name.clone(), payload);
        let outcome = match &inv.function_error {
            Some(kind) => format!("✗ function error ({kind})"),
            None => "✓ succeeded".to_string(),
        };
        let mut lines = vec![format!("Status {} · {outcome} · version {}", inv.status, inv.version.as_deref().unwrap_or("?")), String::new()];
        lines.push("RESPONSE".into());
        let response = pretty_json(&inv.payload).unwrap_or_else(|| inv.payload.clone());
        lines.extend(response.lines().map(str::to_string));
        if inv.payload.is_empty() {
            lines.push("(empty)".into());
        }
        lines.push(String::new());
        lines.push("LOG (the last 4 KB)".into());
        // Runtimes separate the timestamp, request id and level with tabs, which terminals
        // show as nothing in a line of text.
        lines.extend(inv.log_tail.as_deref().unwrap_or("(none)").lines().map(|l| l.replace('\t', "  ")));
        let mut view = LogView::default();
        view.extend(lines);
        view.zoomed = true;
        view.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        self.result = Some((format!("Invoke · {name}"), view));
        match inv.function_error {
            Some(_) => ctx.toast(Level::Warn, format!("{name} returned an error")),
            None => ctx.toast(Level::Success, format!("Invoked {name}")),
        }
    }

    fn copy_report(&mut self, ctx: &mut Ctx) {
        let Some(name) = self.selected().map(|f| f.function_name.clone()) else { return };
        if !self.reporting.insert(name.clone()) {
            return ctx.toast(Level::Info, format!("Still writing the report of {name}"));
        }
        ctx.toast(Level::Info, format!("Writing the report of {name}: metrics and 24 h of logs…"));
        let env = ctx.env();
        let (aws, env_name, region) = (env.aws.clone(), env.name.clone(), env.region.clone());
        ctx.spawn(move || {
            let result = lambda::gather_report(&aws, &env_name, &region, &name, now_secs());
            Msg::Report { name, result }
        });
    }

    fn on_report(&mut self, name: String, result: aws::Result<String>, ctx: &mut Ctx) {
        self.reporting.remove(&name);
        let md = match result {
            Ok(md) => md,
            Err(e) => return ctx.toast(Level::Error, format!("Could not write the report: {}", e.short())),
        };
        if crate::platform::copy_to_clipboard(&md) {
            return ctx.toast(Level::Success, format!("Copied the report of {name}. Paste it into your AI agent."));
        }
        match crate::platform::write_temp_file(&format!("lambda-{name}"), &md) {
            Ok(path) => ctx.toast(Level::Success, format!("Saved the report to {}", path.display())),
            Err(e) => ctx.toast(Level::Error, format!("Could not save the report: {e}")),
        }
    }

    /// Measures what reading the selected function's memory use would scan, then asks.
    fn check_memory(&mut self, ctx: &mut Ctx) {
        let Some(func) = self.selected().cloned() else { return };
        let name = func.function_name.clone();
        if !self.reading_memory.insert(name.clone()) {
            return ctx.toast(Level::Info, format!("Still reading the memory use of {name}"));
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = memory::memory_estimate(&aws, Some(vec![func]), DEFAULT_DAYS);
            Msg::MemoryEstimate { name, result }
        });
    }

    fn on_memory_estimate(&mut self, name: String, result: aws::Result<Estimate>, ctx: &mut Ctx) {
        match result {
            Ok(estimate) if estimate.functions.is_empty() => {
                self.reading_memory.remove(&name);
                ctx.toast(Level::Warn, format!("{name} shares its log group with other functions: their REPORT lines cannot be told apart"));
            }
            Ok(estimate) => {
                let form = memory_form(&estimate, format!("Memory use · {name}"));
                self.memory = Some(MemoryCheck { name, estimate, form });
            }
            Err(e) => {
                self.reading_memory.remove(&name);
                ctx.toast(Level::Error, format!("Could not measure the logs of {name}: {}", e.short()));
            }
        }
    }

    fn submit_memory(&mut self, ctx: &mut Ctx) {
        let Some(check) = self.memory.take() else { return };
        let (name, estimate) = (check.name, chosen_estimate(&check.form, &check.estimate));
        let (aws, env) = (ctx.env().aws.clone(), self.env_name.clone());
        ctx.toast(Level::Info, format!("Reading the memory use of {name} ({})…", estimate.describe()));
        ctx.spawn(move || {
            let result = memory::memory_analysis(&aws, &env, &estimate, &|_, _| {});
            Msg::Memory { name, result }
        });
    }

    fn on_memory(&mut self, name: String, result: aws::Result<MemoryRun>, ctx: &mut Ctx) {
        self.reading_memory.remove(&name);
        match result {
            Ok(run) => match run.uses.iter().find(|u| u.function == name) {
                Some(used) => ctx.toast(Level::Success, format!("{name}: {} · {} scanned", used.hint(), crate::savings::size(run.bytes_scanned))),
                None => ctx.toast(Level::Info, format!("{name} has no invocations in the window")),
            },
            Err(e) => ctx.toast(Level::Error, format!("Could not read the memory use of {name}: {}", e.short())),
        }
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|&i| {
                let func = &self.functions[i];
                let runtime = func.runtime.clone().unwrap_or_else(|| func.package_type.clone().unwrap_or_default());
                let right = format!("{:>9}  {:>6}", ellipsize(&runtime, 12), func.memory_size.map(|m| format!("{m}MB")).unwrap_or_default());
                let name = ellipsize(&func.function_name, width.saturating_sub(right.chars().count() + 2));
                let pad = width.saturating_sub(name.chars().count() + right.chars().count());
                ListItem::new(Line::from(vec![
                    Span::raw(name),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(right, theme::muted()),
                ]))
            })
            .collect();
        let title = match &self.load {
            LoadState::Loading => format!(" {} Loading… {} ", theme::spinner(view.tick), self.functions.len()),
            _ if !self.filter.value.is_empty() => format!(" {} of {} functions ", self.visible.len(), self.functions.len()),
            _ => format!(" {} functions ", self.functions.len()),
        };
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search functions or runtimes");
        f.render_widget(Paragraph::new(line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        if self.visible.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading functions…", theme::muted()),
                _ => Line::styled("No functions", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    /// The metrics section: a header with the range, then the charts.
    fn metric_lines(&self, func: &lambda::FunctionConfiguration, width: usize, view: &View) -> Vec<Line<'static>> {
        let mut ranges = vec![widgets::section("Metrics").spans.remove(0), Span::raw("   ")];
        for r in [Range::Hour, Range::Day, Range::Week] {
            let style = if r == self.range { theme::accent().bold() } else { theme::muted() };
            ranges.push(Span::styled(format!("{} ", r.label()), style));
        }
        ranges.push(Span::styled("  m changes", theme::muted()));
        let mut lines = vec![Line::from(ranges)];
        let m = match self.metrics.get(&(func.function_name.clone(), self.range)) {
            Some((_, Ok(m))) => m,
            Some((_, Err(e))) => {
                lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR)));
                return lines;
            }
            None => {
                lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" loading metrics…")]));
                return lines;
            }
        };
        if m.points("invocations").is_empty() {
            lines.push(Line::styled(format!("No invocations in the last {}.", self.range.label()), theme::muted()));
            return lines;
        }
        let err = Style::new().fg(theme::ERR);
        let errors = m.total("errors");
        let mut summary = vec![Span::styled(format!("{} total", fmt_count(m.total("invocations"))), theme::muted())];
        if errors > 0.0 {
            let rate = m.error_rate().map(|r| format!(" ({:.1}%)", r * 100.0)).unwrap_or_default();
            summary.push(Span::styled(format!(" · {} errors{rate}", fmt_count(errors)), err));
        } else {
            summary.push(Span::styled(" · no errors", Style::new().fg(theme::OK)));
        }
        let mut invocations = Chart::new("Invocations", m.points("invocations"), m.window);
        invocations.summary = summary;
        invocations.stacked = Some(m.points("errors"));
        invocations.axis = false;
        lines.extend(invocations.lines(width));

        let timeout_ms = func.timeout.map(|t| t as f64 * 1000.0);
        let mut duration = Chart::new("Duration p95", m.points("p95"), m.window);
        duration.agg = Agg::Max;
        duration.color = theme::RUNNING;
        duration.fmt = fmt_ms;
        duration.threshold = timeout_ms;
        duration.axis = false;
        let mut summary = Vec::new();
        if let Some(avg) = m.mean_duration() {
            summary.push(format!("avg {}", fmt_ms(avg)));
        }
        if let Some(p95) = m.max("p95") {
            summary.push(format!("worst {}", fmt_ms(p95)));
        }
        if let Some(t) = timeout_ms {
            summary.push(format!("timeout {}", fmt_ms(t)));
        }
        let near_timeout = timeout_ms.zip(m.max("p95")).is_some_and(|(t, p)| p >= t * 0.8);
        duration.summary = vec![Span::styled(summary.join(" · "), if near_timeout { err } else { theme::muted() })];
        lines.extend(duration.lines(width));

        let throttles = m.total("throttles");
        let mut concurrency = Chart::new("Concurrency", m.points("concurrency"), m.window);
        concurrency.agg = Agg::Max;
        concurrency.color = theme::OK;
        concurrency.rows = 2;
        concurrency.axis = throttles == 0.0;
        concurrency.summary = vec![Span::styled(format!("max {}", fmt_count(m.max("concurrency").unwrap_or_default())), theme::muted())];
        lines.extend(concurrency.lines(width));
        if throttles > 0.0 {
            let mut chart = Chart::new("Throttles", m.points("throttles"), m.window);
            chart.color = theme::WARN;
            chart.rows = 2;
            chart.summary = vec![Span::styled(format!("{} total", fmt_count(throttles)), Style::new().fg(theme::WARN))];
            lines.extend(chart.lines(width));
        } else {
            lines.push(Line::from(vec![Span::styled("Throttles", Style::new().fg(theme::WARN).bold()), Span::styled("  none", theme::muted())]));
        }
        lines
    }

    fn render_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(func) = self.selected() else {
            f.render_widget(
                widgets::placeholder(Line::styled("Select a function", theme::muted())).block(widgets::panel(" Function ", false)),
                area,
            );
            return;
        };
        let detail = self.details.get(&func.function_name);
        // GetFunction has the full configuration; fall back to the list entry meanwhile.
        let (c, tags, concurrency) = match detail {
            Some(Ok(d)) => (&d.configuration, Some(&d.tags), d.concurrency.as_ref()),
            _ => (func, None, None),
        };
        let kw = 14;
        let mut lines = self.metric_lines(c, area.width.saturating_sub(4) as usize, view);
        lines.push(Line::raw(""));
        lines.push(widgets::section("General"));
        if let Some(d) = c.description.as_ref().filter(|d| !d.is_empty()) {
            lines.push(widgets::kv("Description", d.clone(), kw));
        }
        lines.push(widgets::kv("ARN", Span::styled(c.function_arn.clone(), theme::muted()), kw));
        lines.push(widgets::kv("Runtime", c.runtime.clone().unwrap_or_else(|| "—".into()), kw));
        if let Some(h) = &c.handler {
            lines.push(widgets::kv("Handler", h.clone(), kw));
        }
        lines.push(widgets::kv("Architecture", c.architectures.join(", "), kw));
        lines.push(widgets::kv("Package", c.package_type.clone().unwrap_or_default(), kw));
        lines.push(widgets::kv(
            "Memory",
            Span::styled(c.memory_size.map(|m| format!("{m} MB")).unwrap_or_default(), theme::bold()),
            kw,
        ));
        let used = self.aws.as_ref().and_then(|aws| memory::remembered(aws, &c.function_name));
        let memory_use = match used {
            Some(u) => {
                let color = if u.smaller_mb().is_some() { theme::WARN } else { theme::OK };
                Span::styled(u.hint(), Style::new().fg(color))
            }
            None if self.reading_memory.contains(&c.function_name) => {
                Span::styled(format!("{} reading the logs…", theme::spinner(view.tick)), theme::accent())
            }
            None => Span::styled("M reads it from the logs (Logs Insights, billed per GB scanned)", theme::muted()),
        };
        lines.push(widgets::kv("Memory use", memory_use, kw));
        lines.push(widgets::kv("Timeout", c.timeout.map(|t| format!("{t} s")).unwrap_or_default(), kw));
        if let Some(e) = &c.ephemeral_storage {
            lines.push(widgets::kv("Ephemeral /tmp", format!("{} MB", e.size), kw));
        }
        lines.push(widgets::kv("Code size", fmt_bytes(c.code_size), kw));
        if let Some(state) = &c.state {
            let color = match state.as_str() {
                "Active" => theme::OK,
                "Failed" => theme::ERR,
                _ => theme::WARN,
            };
            let mut value = state.clone();
            if let Some(r) = &c.state_reason {
                value.push_str(&format!(" — {r}"));
            }
            lines.push(widgets::kv("State", Span::styled(value, Style::new().fg(color)), kw));
        }
        if let Some(u) = &c.last_update_status {
            lines.push(widgets::kv("Last update", u.clone(), kw));
        }
        if let Some(m) = &c.last_modified {
            let ago = parse_rfc3339(m).map(fmt_ago).unwrap_or_default();
            lines.push(widgets::kv("Modified", format!("{ago} · {m}"), kw));
        }
        if let Some(conc) = concurrency {
            let value = conc
                .reserved_concurrent_executions
                .map(|n| format!("{n} reserved"))
                .unwrap_or_else(|| "unreserved".into());
            lines.push(widgets::kv("Concurrency", value, kw));
        }

        lines.push(Line::raw(""));
        lines.push(widgets::section("Execution"));
        if let Some(role) = &c.role {
            lines.push(widgets::kv("Role", role.rsplit('/').next().unwrap_or(role).to_string(), kw));
        }
        lines.push(widgets::kv("Log group", c.log_group(), kw));
        if let Some(fmt) = c.logging_config.as_ref().and_then(|l| l.log_format.clone()) {
            lines.push(widgets::kv("Log format", fmt, kw));
        }
        if let Some(mode) = c.tracing_config.as_ref().and_then(|t| t.mode.clone()) {
            lines.push(widgets::kv("Tracing", mode, kw));
        }
        if let Some(vpc) = c.vpc_config.as_ref().filter(|_| c.in_vpc()) {
            lines.push(widgets::kv(
                "VPC",
                format!(
                    "{} · {} subnets · {} security group{}",
                    vpc.vpc_id.clone().unwrap_or_default(),
                    vpc.subnet_ids.len(),
                    vpc.security_group_ids.len(),
                    if vpc.security_group_ids.len() == 1 { "" } else { "s" }
                ),
                kw,
            ));
        }
        if let Some(dlq) = c.dead_letter_config.as_ref().and_then(|d| d.target_arn.clone()).filter(|a| !a.is_empty()) {
            lines.push(widgets::kv("Dead letters", dlq.rsplit(':').next().unwrap_or(&dlq).to_string(), kw));
        }
        for layer in &c.layers {
            let name = layer.arn.rsplit(":layer:").next().unwrap_or(&layer.arn);
            lines.push(widgets::kv("Layer", format!("{name} ({})", fmt_bytes(layer.code_size)), kw));
        }

        let vars = c.environment.as_ref().map(|e| &e.variables);
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            widgets::section(&format!("Environment ({})", vars.map_or(0, |v| v.len()))).spans.remove(0),
            Span::styled(if self.reveal_env { "   v hide" } else { "   v reveal" }, theme::muted()),
        ]));
        match vars {
            Some(vars) if !vars.is_empty() => {
                let key_w = vars.keys().map(|k| k.len()).max().unwrap_or(0).min(32);
                for (k, v) in vars {
                    let value = if self.reveal_env {
                        Span::raw(v.clone())
                    } else {
                        Span::styled("••••••••", theme::muted())
                    };
                    lines.push(Line::from(vec![Span::styled(format!("{k:<key_w$}  "), theme::accent()), value]));
                }
            }
            _ => lines.push(Line::styled("none", theme::muted())),
        }

        lines.push(Line::raw(""));
        match (detail, tags) {
            (None, _) => lines.push(Line::from(vec![
                Span::styled(theme::spinner(view.tick), theme::accent()),
                Span::raw(" loading details…"),
            ])),
            (Some(Err(e)), _) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            (Some(Ok(_)), Some(tags)) => {
                lines.push(widgets::section(&format!("Tags ({})", tags.len())));
                let key_w = tags.keys().map(|k| k.len()).max().unwrap_or(0).min(32);
                for (k, v) in tags {
                    lines.push(Line::from(vec![Span::styled(format!("{k:<key_w$}  "), theme::accent()), Span::raw(v.clone())]));
                }
            }
            _ => {}
        }

        let mut block = widgets::panel(Line::from(format!(" {} ", c.function_name)).bold(), false);
        if self.reporting.contains(&c.function_name) {
            block = block.title_top(
                Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::styled(" writing the report ", theme::muted())])
                    .right_aligned(),
            );
        }
        f.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block),
            area,
        );
    }

    fn render_invoke(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(invoke) = &self.invoke else { return };
        let color = if is_production(&self.env_name) { theme::ERR } else { theme::ACCENT };
        invoke.form.render(f, area, color);
        if let Some(what) = invoke.busy {
            busy_popup(f, area, view, what);
        }
    }

    fn render_events(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some(list) = &mut self.events {
            let rect = widgets::popup_area(area, 110, list.picker.height(area.height.saturating_sub(4)));
            list.picker.render(f, rect, theme::ACCENT);
        }
        if let Some(deleting) = &self.deleting {
            deleting.form.render(f, area, theme::ERR);
            if deleting.busy {
                busy_popup(f, area, view, "Deleting…");
            }
        }
    }
}

fn busy_popup(f: &mut Frame, area: Rect, view: &View, what: &str) {
    let body = Text::from(vec![
        Line::raw(""),
        Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw(what.to_string())]),
    ]);
    widgets::popup(f, widgets::popup_area(area, 30, 5), "Lambda", theme::ACCENT, body);
}

impl Lambda {
    fn invoke_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(invoke) = &mut self.invoke else { return };
        if invoke.busy.is_some() {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('s') if ctrl => self.save_event(ctx),
            KeyCode::Char('o') if ctrl => {
                let name = invoke.name.clone();
                if self.saved(&name).is_empty() {
                    if let Some(invoke) = &mut self.invoke {
                        invoke.form.fail("No saved test events yet: name this one in Save as and press Ctrl+S");
                    }
                } else {
                    self.invoke = None;
                    self.open_events(name);
                }
            }
            _ => match invoke.form.handle(key) {
                FormEvent::Submit => self.submit_invoke(ctx),
                FormEvent::Cancel => self.invoke = None,
                FormEvent::Pending => {}
            },
        }
    }

    fn events_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        if let Some(deleting) = &mut self.deleting {
            if !deleting.busy {
                match deleting.form.handle(key) {
                    FormEvent::Submit => self.submit_delete(ctx),
                    FormEvent::Cancel => self.deleting = None,
                    FormEvent::Pending => {}
                }
            }
            return;
        }
        let Some(list) = &mut self.events else { return };
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            return self.delete_event(ctx);
        }
        match list.picker.handle(key) {
            Outcome::Pending => {}
            Outcome::Cancel => self.events = None,
            Outcome::Done(chosen) => {
                let Some(list) = self.events.take() else { return };
                let event = chosen.first().and_then(|i| i.checked_sub(1)).and_then(|i| list.events.get(i)).cloned();
                self.open_form(list.name, event);
            }
        }
    }
}

impl Module for Lambda {
    fn title(&self) -> &'static str {
        "Lambda"
    }

    fn description(&self) -> &'static str {
        "Lambda functions · configuration, metrics, invoke, reports for AI"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Lambda::new();
        self.region = ctx.env().region.clone();
        self.env_name = ctx.env().name.clone();
        self.aws = Some(ctx.env().aws.clone());
        // Memory analyses kept from earlier runs show in the function's details.
        memory::preload(&ctx.env().aws, &self.env_name);
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load(ctx);
        }
        // Coming back later shows current numbers.
        self.metrics.retain(|_, (at, _)| at.elapsed() < METRICS_FRESH);
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some((_, view)) = &mut self.result {
            if let LogKey::Copy = view.key(key) {
                let ok = crate::platform::copy_to_clipboard(&view.lines().join("\n"));
                ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Copied" } else { "Clipboard not available" });
            }
            if !view.zoomed {
                self.result = None;
            }
            return Handled::Yes;
        }
        if let Some(check) = &mut self.memory {
            match check.form.handle(key) {
                FormEvent::Submit => self.submit_memory(ctx),
                FormEvent::Cancel => {
                    self.reading_memory.remove(&check.name);
                    self.memory = None;
                }
                FormEvent::Pending => update_memory_form(&mut check.form, &check.estimate),
            }
            return Handled::Yes;
        }
        if self.events.is_some() || self.deleting.is_some() {
            self.events_key(key, ctx);
            return Handled::Yes;
        }
        if self.invoke.is_some() {
            self.invoke_key(key, ctx);
            return Handled::Yes;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        if self.filtering {
            match self.filter.handle(key) {
                InputEvent::Changed => {
                    self.rebuild();
                    self.select(0);
                }
                InputEvent::Submit => self.filtering = false,
                InputEvent::Cancel => {
                    self.filtering = false;
                    self.filter.clear();
                    self.rebuild();
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
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.rebuild();
            }
            KeyCode::Char('v') => self.reveal_env = !self.reveal_env,
            KeyCode::Char('m') => {
                self.range = self.range.next();
                self.fetch_metrics(ctx);
            }
            KeyCode::Char('i') => self.open_invoke(ctx),
            KeyCode::Char('M') => self.check_memory(ctx),
            KeyCode::Char('c') => self.copy_report(ctx),
            KeyCode::Char('y') => {
                if let Some(func) = self.selected() {
                    let ok = crate::platform::copy_to_clipboard(&func.function_arn);
                    ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "ARN copied" } else { "Clipboard not available" });
                }
            }
            KeyCode::Char('l') => {
                if let Some(func) = self.selected() {
                    ctx.jump("Logs", func.log_group());
                }
            }
            KeyCode::Char('o') => {
                if let Some(func) = self.selected() {
                    let url = aws::console_url(
                        &self.region,
                        &format!("lambda/home?region={}#/functions/{}", self.region, func.function_name),
                    );
                    if !crate::platform::open_url(&url) {
                        ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                    }
                }
            }
            KeyCode::Char('r') => {
                if let Some(func) = self.selected() {
                    let name = func.function_name.clone();
                    self.details.remove(&name);
                    self.metrics.retain(|(n, _), _| *n != name);
                    self.fetch(name, ctx);
                    self.fetch_metrics(ctx);
                } else {
                    self.load(ctx);
                }
            }
            KeyCode::Char('R') => self.load(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        let pos = (event.column, event.row).into();
        if self.invoke.is_some() || self.events.is_some() || self.memory.is_some() || !self.list_area.contains(pos) {
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

    /// Selects function `target` (from an EventBridge target, say), now or once loaded.
    fn open(&mut self, target: &str, _ctx: &mut Ctx) {
        self.open_target = Some(target.to_string());
        self.try_open();
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Page(page) => {
                let first = self.functions.is_empty();
                self.functions.extend(page);
                self.rebuild();
                if !self.try_open() && first {
                    self.select(0);
                }
            }
            Msg::Done => self.load = LoadState::Loaded,
            Msg::Failed(e) => self.load = LoadState::Failed(e),
            Msg::Detail { name, result } => {
                self.in_flight.remove(&name);
                self.details.insert(name, *result);
            }
            Msg::Metrics { name, range, result } => {
                self.metrics_in_flight.remove(&(name.clone(), range));
                self.metrics.insert((name, range), (Instant::now(), *result));
            }
            Msg::Invoked { name, payload, result } => self.on_invoked(name, payload, *result, ctx),
            Msg::SharedEvents { name, result } => self.on_shared_events(name, result, ctx),
            Msg::EventSaved { name, event, result } => self.on_event_saved(name, event, result, ctx),
            Msg::EventDeleted { name, event, result } => self.on_event_deleted(name, event, result, ctx),
            Msg::Report { name, result } => self.on_report(name, result, ctx),
            Msg::MemoryEstimate { name, result } => self.on_memory_estimate(name, result, ctx),
            Msg::Memory { name, result } => self.on_memory(name, result, ctx),
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if let Some((name, at)) = &self.pending
            && at.elapsed() >= DEBOUNCE
        {
            let name = name.clone();
            self.pending = None;
            if self.selected().is_some_and(|f| f.function_name == name) && !self.details.contains_key(&name) {
                self.fetch(name, ctx);
            }
        }
        // The charts follow the selection once it settles.
        if active && self.pending.is_none() {
            self.fetch_metrics(ctx);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some((title, result)) = &mut self.result {
            let title = title.clone();
            result.render_zoomed(f, area, &title);
            return;
        }
        let [left, right] = if area.width < 90 {
            area.layout(&Layout::vertical([Constraint::Percentage(45), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(38), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        self.render_detail(f, right, view);
        self.render_invoke(f, area, view);
        self.render_events(f, area, view);
        if let Some(check) = &self.memory {
            check.form.render(f, area, theme::ACCENT);
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.result.is_some() {
            return vec![("/", "search"), ("y", "copy"), ("Esc", "back")];
        }
        if self.deleting.is_some() {
            return vec![("Enter", "delete"), ("Esc", "cancel")];
        }
        if self.events.is_some() {
            return vec![("↑↓", "event"), ("Enter", "use"), ("Ctrl+D", "delete"), ("Esc", "cancel")];
        }
        if self.invoke.is_some() {
            return vec![("Enter", "invoke"), ("Ctrl+S", "save event"), ("Ctrl+O", "saved events"), ("Tab", "next field"), ("Esc", "cancel")];
        }
        if self.memory.is_some() {
            return vec![("←→", "window"), ("Enter", "analyse"), ("Esc", "cancel")];
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        vec![
            ("↑↓", "move"),
            ("/", "search"),
            ("m", "1h/24h/7d"),
            ("i", "invoke"),
            ("M", "memory use"),
            ("c", "copy for AI"),
            ("y", "copy ARN"),
            ("v", "env values"),
            ("l", "logs"),
            ("o", "console"),
            ("r", "refresh"),
            ("PgUp/PgDn", "scroll"),
        ]
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || self.pending.is_some()
            || !self.in_flight.is_empty()
            || !self.metrics_in_flight.is_empty()
            || !self.reporting.is_empty()
            || !self.reading_memory.is_empty()
            || self.invoke.as_ref().is_some_and(|i| i.busy.is_some())
            || self.deleting.as_ref().is_some_and(|d| d.busy)
    }

    fn capturing(&self) -> bool {
        self.filtering || self.invoke.is_some() || self.events.is_some() || self.deleting.is_some() || self.memory.is_some() || self.result.is_some()
    }

    fn wants_mouse(&self) -> bool {
        self.result.is_none()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if let Some(deleting) = &mut self.deleting {
            return deleting.form.paste(text);
        }
        if let Some(list) = &mut self.events {
            return list.picker.paste(text);
        }
        match &mut self.invoke {
            Some(invoke) if invoke.busy.is_none() => invoke.form.paste(text),
            Some(_) => {}
            None if self.filtering => {
                self.filter.insert_str(text, false);
                self.rebuild();
            }
            None => {}
        }
    }
}

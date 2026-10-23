//! Savings: what could cost less in the current environment, from its resources and their
//! usage over the last days, measured in the units AWS bills (dcli keeps no prices). The
//! scan runs when the module opens, every service in parallel; each finding shows its
//! evidence and caveats and opens its resource, and two kinds can be applied here: a log
//! group's retention and a queue's long polling. The Lambda memory analysis runs on demand
//! because Logs Insights bills the data it scans: it says how much first.
//!
//! With the account's costs read (the Costs module), findings get dollars at the bill's own
//! rates; "By cost" splits the most expensive usage types over the resources behind them,
//! and "Verified" follows the changes made (applied here, marked done with `d`, or made by
//! other modules) and compares the bill before and after.

mod views;

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{filter_bar, matches_filter};
use crate::aws::{self, AwsError, logs, sqs};
use crate::config::is_production;
use crate::costs::attribution::{self, Split};
use crate::costs::ledger::{self, Entry, Source};
use crate::costs::{self, Snapshot};
use crate::savings::lambda::{self as memory, Estimate, MemoryRun};
use crate::savings::price::{self, money};
use crate::savings::{self, CHECKS, DEFAULT_DAYS, Finding, Fix, Impact, Outcome, find_service, short_name};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, now_secs};

/// Retention choices offered for a log group (CloudWatch accepts only some numbers).
const RETENTIONS: [&str; 6] = ["7 days", "30 days", "90 days", "180 days", "1 year", "2 years"];
/// Long polling wait times offered.
const WAITS: [&str; 3] = ["20 seconds", "10 seconds", "5 seconds"];
/// Windows offered for the memory analysis.
const WINDOWS: [&str; 4] = ["14 days", "7 days", "3 days", "1 day"];

enum Msg {
    Checked { service: &'static str, result: aws::Result<Vec<Finding>> },
    Scanned,
    Estimated(aws::Result<Estimate>),
    MemoryProgress(usize, usize),
    Memory(aws::Result<MemoryRun>),
    Applied { key: String, result: aws::Result<String> },
    SplitProgress(usize, usize),
    Split(Vec<Split>),
    Verified { id: String, result: aws::Result<ledger::Verification> },
}

enum Check {
    Running,
    Done(usize),
    Failed(AwsError),
}

enum MemoryState {
    Idle,
    Estimating,
    Running(usize, usize),
    Done { functions: usize, bytes: f64, at: f64, silent: usize },
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Findings,
    ByCost,
    Verified,
}

const TABS: [Tab; 3] = [Tab::Findings, Tab::ByCost, Tab::Verified];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Findings => "Findings",
            Tab::ByCost => "By cost",
            Tab::Verified => "Verified",
        }
    }
}

/// The split of the bill's most expensive usage types over their resources.
enum Splits {
    Idle,
    Running(usize, usize),
    Done { splits: Vec<Split>, at: f64 },
}

/// A row of the By cost tab: a usage type, or one of its resources.
#[derive(Clone, Copy, PartialEq)]
enum CostRow {
    Usage(usize),
    Resource(usize, usize),
}

enum Dialog {
    Fix { key: String, fix: Fix, form: Form, busy: bool },
    Memory { estimate: Estimate, form: Form },
    Done { key: String, form: Form },
    Verify { id: String, form: Form, busy: bool },
    Forget { id: String, form: Form },
}

pub struct Savings {
    env_name: String,
    region: String,
    days: u32,
    tab: Tab,
    /// Each service of the scan and how it went, in the order of [`CHECKS`].
    checks: Vec<(&'static str, Check)>,
    scanning: bool,
    /// When the last scan ended (Unix seconds).
    scanned_at: Option<f64>,
    found: Vec<Finding>,
    /// Findings of the Lambda memory analysis.
    memory_found: Vec<Finding>,
    memory: MemoryState,
    /// The account's costs, as last read by the Costs module: they price the findings.
    costs: Option<Snapshot>,
    /// Everything found, most important first; `rows` are the shown ones.
    all: Vec<Finding>,
    rows: Vec<usize>,
    list_state: ListState,
    filter: TextInput,
    filtering: bool,
    service: Option<&'static str>,
    min_impact: Impact,
    detail_scroll: u16,
    dialog: Option<Dialog>,
    /// Findings whose fix was applied, and those marked done (see [`key_of`]).
    applied: HashSet<String>,
    marked: HashSet<String>,
    splits: Splits,
    cost_rows: Vec<CostRow>,
    cost_state: ListState,
    /// The changes followed in this environment, newest first.
    ledger: Vec<Entry>,
    ledger_state: ListState,
    list_area: Rect,
    /// The user moved through the list during the scan: findings arriving keep the selection.
    touched: bool,
}

/// Names a finding across scans.
fn key_of(f: &Finding) -> String {
    format!("{}\u{0}{}\u{0}{}", f.service, f.target, f.summary)
}

fn impact_color(impact: Impact) -> Color {
    match impact {
        Impact::High => theme::ERR,
        Impact::Medium => theme::WARN,
        Impact::Low => theme::ACCENT,
    }
}

/// `30 days` → 30, `1 year` → 365 (the retention values CloudWatch accepts).
fn retention_days(choice: &str) -> u32 {
    match choice {
        "1 year" => 365,
        "2 years" => 731,
        c => c.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(30),
    }
}

fn leading_number(choice: &str) -> u32 {
    choice.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// The confirmation of a memory analysis: what it reads and how much Insights would scan.
/// Shared with the Lambda module.
pub(super) fn memory_form(estimate: &Estimate, title: String) -> Form {
    let window = format!("{} days", estimate.days);
    let selected = if WINDOWS.contains(&window.as_str()) { window } else { WINDOWS[0].to_string() };
    let mut fields = vec![
        Field::fixed("Functions", estimate.functions.len().to_string()),
        Field::choice("Window", &WINDOWS, &selected),
        Field::fixed("Logs to scan", String::new()),
    ];
    if !estimate.shared.is_empty() {
        fields.insert(1, Field::fixed("Left out", format!("{} sharing a log group", estimate.shared.len())));
    }
    let mut form = Form::new(title, fields).submit_label("analyse").enter_submits().focus_on("Window");
    form.note = Some(
        "Logs Insights reads the whole log groups over the window and bills the data it scans; the query keeps the \
         REPORT line of each invocation (memory used, billed time)."
            .into(),
    );
    update_memory_form(&mut form, estimate);
    form
}

/// The estimate for the window chosen in the form.
pub(super) fn chosen_estimate(form: &Form, estimate: &Estimate) -> Estimate {
    estimate.with_days(leading_number(form.get("Window")).max(1))
}

/// Shows the bytes to scan for the window chosen.
pub(super) fn update_memory_form(form: &mut Form, estimate: &Estimate) {
    let chosen = chosen_estimate(form, estimate);
    if let Some(field) = form.field_mut("Logs to scan") {
        field.input = TextInput::with_value(format!("≈ {} (what the groups received)", savings::size(chosen.bytes)));
    }
}

impl Savings {
    pub fn new() -> Savings {
        Savings {
            env_name: String::new(),
            region: String::new(),
            days: DEFAULT_DAYS,
            tab: Tab::Findings,
            checks: Vec::new(),
            scanning: false,
            scanned_at: None,
            found: Vec::new(),
            memory_found: Vec::new(),
            memory: MemoryState::Idle,
            costs: None,
            all: Vec::new(),
            rows: Vec::new(),
            list_state: ListState::default(),
            filter: TextInput::default(),
            filtering: false,
            service: None,
            min_impact: Impact::Low,
            detail_scroll: 0,
            dialog: None,
            applied: HashSet::new(),
            marked: HashSet::new(),
            splits: Splits::Idle,
            cost_rows: Vec::new(),
            cost_state: ListState::default(),
            ledger: Vec::new(),
            ledger_state: ListState::default(),
            list_area: Rect::default(),
            touched: false,
        }
    }

    fn scan(&mut self, ctx: &mut Ctx) {
        if self.scanning {
            return ctx.toast(Level::Info, "Still scanning");
        }
        self.scanning = true;
        self.touched = false;
        self.found.clear();
        self.checks = CHECKS.iter().map(|(service, _)| (*service, Check::Running)).collect();
        self.rebuild();
        let (aws, env, days) = (ctx.env().aws.clone(), ctx.env().name.clone(), self.days);
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            savings::run(&aws, &env, days, &[], &|service, result| {
                em.emit(Msg::Checked { service, result: result.clone() });
            });
            em.emit(Msg::Scanned);
        });
    }

    /// Reads again the costs the Costs module keeps (it may have read the bill meanwhile)
    /// and the changes followed, and prices the findings when the costs changed.
    fn load_costs(&mut self) {
        let costs = costs::cached(&self.env_name);
        let changed = costs.as_ref().map(|c| c.read_at) != self.costs.as_ref().map(|c| c.read_at);
        self.costs = costs;
        if changed {
            self.reprice();
            if !matches!(self.splits, Splits::Running(..)) {
                self.splits = Splits::Idle;
                self.cost_rows.clear();
            }
        }
        self.load_ledger();
    }

    fn load_ledger(&mut self) {
        let keep = self.selected_entry().map(|e| e.id.clone());
        self.ledger = ledger::of_env(&self.env_name);
        let index = keep.and_then(|id| self.ledger.iter().position(|e| e.id == id)).unwrap_or(0);
        self.ledger_state.select(if self.ledger.is_empty() { None } else { Some(index.min(self.ledger.len() - 1)) });
    }

    fn reprice(&mut self) {
        price::price_all(&mut self.found, self.costs.as_ref(), &self.region);
        price::price_all(&mut self.memory_found, self.costs.as_ref(), &self.region);
        self.rebuild();
    }

    fn selected(&self) -> Option<&Finding> {
        self.all.get(*self.rows.get(self.list_state.selected()?)?)
    }

    /// Whether `f` was marked done, now or in an earlier run (the ledger remembers).
    fn marked_done(&self, f: &Finding) -> bool {
        self.marked.contains(&key_of(f))
            || self.ledger.iter().any(|e| {
                e.source == Source::Done && e.module == f.module && e.target == f.target && e.resource == f.resource && e.what.ends_with(&f.recommendation)
            })
    }

    /// Whether a change for `f` was made: applied here or marked done.
    fn followed(&self, f: &Finding) -> bool {
        self.applied.contains(&key_of(f)) || self.marked_done(f)
    }

    /// Merges scan and memory findings and applies the filters, keeping the selection.
    fn rebuild(&mut self) {
        let keep = self.selected().map(key_of);
        self.all = self.found.iter().chain(&self.memory_found).cloned().collect();
        savings::sort(&mut self.all);
        let query = self.filter.value.trim().to_lowercase();
        self.rows = (0..self.all.len())
            .filter(|&i| {
                let f = &self.all[i];
                f.impact >= self.min_impact
                    && self.service.is_none_or(|s| s == f.service)
                    && (query.is_empty()
                        || matches_filter(&f.resource, &query)
                        || matches_filter(&f.summary, &query)
                        || matches_filter(f.service, &query))
            })
            .collect();
        let index = keep.and_then(|k| self.rows.iter().position(|&i| key_of(&self.all[i]) == k)).unwrap_or(0);
        self.list_state.select(if self.rows.is_empty() { None } else { Some(index.min(self.rows.len() - 1)) });
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
    }

    /// Services found so far, for the service filter.
    fn next_service(&mut self) {
        let mut present: Vec<&'static str> = Vec::new();
        for (service, _) in CHECKS {
            if self.all.iter().any(|f| f.service == *service) {
                present.push(service);
            }
        }
        self.service = match self.service.and_then(|s| present.iter().position(|p| *p == s)) {
            None => present.first().copied(),
            Some(i) => present.get(i + 1).copied(),
        };
        self.rebuild();
        self.select(0);
    }

    fn next_impact(&mut self) {
        self.min_impact = match self.min_impact {
            Impact::Low => Impact::Medium,
            Impact::Medium => Impact::High,
            Impact::High => Impact::Low,
        };
        self.rebuild();
        self.select(0);
    }

    fn switch_tab(&mut self, forward: bool, ctx: &mut Ctx) {
        let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
        self.tab = TABS[if forward { (i + 1) % TABS.len() } else { (i + TABS.len() - 1) % TABS.len() }];
        self.detail_scroll = 0;
        self.on_tab(ctx);
    }

    /// What a tab needs when it shows: By cost splits the bill the first time.
    fn on_tab(&mut self, ctx: &mut Ctx) {
        match self.tab {
            Tab::ByCost if matches!(self.splits, Splits::Idle) && self.costs.is_some() => self.start_split(ctx),
            Tab::Verified => self.load_ledger(),
            _ => {}
        }
    }

    fn open_fix(&mut self, ctx: &mut Ctx) {
        let Some(f) = self.selected() else { return };
        let Some(fix) = f.fix.clone() else {
            return ctx.toast(Level::Info, "Nothing dcli can apply for this one: see the recommendation (d marks it done)");
        };
        let key = key_of(f);
        if self.applied.contains(&key) {
            return ctx.toast(Level::Info, "Already applied");
        }
        let mut form = match &fix {
            Fix::LogRetention { group } => {
                let mut form = Form::new(
                    "Log retention",
                    vec![Field::fixed("Log group", group.clone()), Field::choice("Keep", &RETENTIONS, "30 days")],
                );
                form.note = Some("Events older than this are deleted, for good. Export to S3 first what must be kept.".into());
                form
            }
            Fix::LongPolling { queue_url } => {
                let mut form = Form::new(
                    "Long polling",
                    vec![Field::fixed("Queue", sqs::queue_name(queue_url).to_string()), Field::choice("Wait up to", &WAITS, "20 seconds")],
                );
                form.note = Some(
                    "Receives that do not set their own WaitTimeSeconds wait for messages this long instead of answering \
                     empty at once."
                        .into(),
                );
                form
            }
        }
        .submit_label("apply");
        if is_production(&self.env_name) {
            form = form.confirm_with(&self.env_name);
        }
        self.dialog = Some(Dialog::Fix { key, fix, form, busy: false });
    }

    fn submit_fix(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Fix { key, fix, form, busy }) = &mut self.dialog else { return };
        *busy = true;
        let (aws, key, fix) = (ctx.env().aws.clone(), key.clone(), fix.clone());
        let (days, wait) = (retention_days(form.get("Keep")), leading_number(form.get("Wait up to")));
        ctx.spawn(move || {
            let result = match &fix {
                Fix::LogRetention { group } => logs::put_retention_policy(&aws, group, days)
                    .map(|()| format!("{group} keeps {days} days of events")),
                Fix::LongPolling { queue_url } => {
                    sqs::set_attributes(&aws, queue_url, &[("ReceiveMessageWaitTimeSeconds", &wait.to_string())])
                        .map(|()| format!("{} waits up to {wait} s for messages", sqs::queue_name(queue_url)))
                }
            };
            Msg::Applied { key, result }
        });
    }

    /// Keeps a change in the ledger, to compare the bill before and after it later.
    fn record(&mut self, key: &str, source: Source, what: String, ctx: &mut Ctx) {
        let Some(f) = self.all.iter().find(|f| key_of(f) == key) else { return };
        if let Err(e) = ledger::put(ledger::entry_for(f, &self.env_name, &self.region, source, what)) {
            ctx.toast(Level::Warn, format!("Could not record the change to verify it later: {e}"));
        }
        self.load_ledger();
    }

    fn open_done(&mut self, ctx: &mut Ctx) {
        let Some(f) = self.selected() else { return };
        let key = key_of(f);
        if self.followed(f) {
            return ctx.toast(Level::Info, "Already recorded: see Verified");
        }
        let mut form = Form::new(
            "Mark as done",
            vec![Field::fixed("Resource", f.resource.clone()), Field::fixed("Change", f.recommendation.clone())],
        )
        .submit_label("mark done")
        .enter_submits();
        form.note = Some(
            "For a change made outside dcli (in code, in the console). dcli records it and, a week later, the Verified tab \
             compares what the bill shows before and after (v there)."
                .into(),
        );
        self.dialog = Some(Dialog::Done { key, form });
    }

    fn submit_done(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Done { key, .. }) = self.dialog.take() else { return };
        let what = self.all.iter().find(|f| key_of(f) == key).map(|f| format!("{}: {}", f.resource, f.recommendation)).unwrap_or_default();
        self.record(&key, Source::Done, what, ctx);
        self.marked.insert(key);
        ctx.toast(Level::Success, "Marked done: Verified compares the bill in a week");
    }

    fn start_estimate(&mut self, ctx: &mut Ctx) {
        if matches!(self.memory, MemoryState::Estimating | MemoryState::Running(..)) {
            return ctx.toast(Level::Info, "The memory analysis is already running");
        }
        self.memory = MemoryState::Estimating;
        let (aws, days) = (ctx.env().aws.clone(), self.days);
        ctx.spawn(move || Msg::Estimated(memory::memory_estimate(&aws, None, days)));
    }

    fn submit_memory(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Memory { estimate, form }) = self.dialog.take() else { return };
        let estimate = chosen_estimate(&form, &estimate);
        self.memory = MemoryState::Running(0, estimate.functions.len());
        let aws = ctx.env().aws.clone();
        ctx.toast(Level::Info, format!("Analysing memory: {}", estimate.describe()));
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let result = memory::memory_analysis(&aws, &estimate, &|done, total| {
                em.emit(Msg::MemoryProgress(done, total));
            });
            em.emit(Msg::Memory(result));
        });
    }

    fn on_memory(&mut self, result: aws::Result<MemoryRun>, ctx: &mut Ctx) {
        let run = match result {
            Ok(run) => run,
            Err(e) => {
                ctx.toast(Level::Error, format!("Memory analysis failed: {}", e.short()));
                self.memory = MemoryState::Failed(e.short());
                return;
            }
        };
        self.memory_found = run.uses.iter().filter_map(memory::memory_finding).map(|f| Finding { service: "Lambda", ..f }).collect();
        price::price_all(&mut self.memory_found, self.costs.as_ref(), &self.region);
        ctx.toast(
            Level::Success,
            format!(
                "Memory of {} functions read · {} scanned · {} could use less",
                run.uses.len(),
                savings::size(run.bytes_scanned),
                self.memory_found.len()
            ),
        );
        self.memory = MemoryState::Done { functions: run.uses.len(), bytes: run.bytes_scanned, at: now_secs(), silent: run.silent.len() };
        self.rebuild();
    }

    // ---- by cost ---------------------------------------------------------------------------

    fn start_split(&mut self, ctx: &mut Ctx) {
        let Some(costs) = self.costs.clone() else {
            return ctx.toast(Level::Info, "Read the bill first: Costs module, r there");
        };
        if matches!(self.splits, Splits::Running(..)) {
            return ctx.toast(Level::Info, "Still splitting the costs");
        }
        self.splits = Splits::Running(0, attribution::candidates(&costs, &self.region).len());
        let aws = ctx.env().aws.clone();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let splits = attribution::split(&aws, &costs, &|done, total| {
                em.emit(Msg::SplitProgress(done, total));
            });
            em.emit(Msg::Split(splits));
        });
    }

    fn rebuild_cost_rows(&mut self) {
        self.cost_rows.clear();
        if let Splits::Done { splits, .. } = &self.splits {
            for (i, s) in splits.iter().enumerate() {
                self.cost_rows.push(CostRow::Usage(i));
                self.cost_rows.extend((0..s.shares.len()).map(|j| CostRow::Resource(i, j)));
            }
        }
        let n = self.cost_rows.len();
        self.cost_state.select(if n == 0 { None } else { Some(self.cost_state.selected().unwrap_or(0).min(n - 1)) });
    }

    fn selected_share(&self) -> Option<(&Split, Option<&attribution::Share>)> {
        let Splits::Done { splits, .. } = &self.splits else { return None };
        match *self.cost_rows.get(self.cost_state.selected()?)? {
            CostRow::Usage(i) => Some((splits.get(i)?, None)),
            CostRow::Resource(i, j) => {
                let s = splits.get(i)?;
                Some((s, Some(s.shares.get(j)?)))
            }
        }
    }

    /// The findings on the resource of a share: those naming it, then those on what holds it
    /// (an instance's cluster), not those on its siblings (another instance of the cluster).
    fn findings_on(&self, share: &attribution::Share) -> Vec<&Finding> {
        if share.target.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<&Finding> = self
            .all
            .iter()
            .filter(|f| f.module == share.module && f.target == share.target)
            .filter(|f| f.resource == share.resource || !f.resource.contains(" / "))
            .collect();
        found.sort_by_key(|f| f.resource != share.resource);
        found
    }

    /// Enter on a resource: its first finding in Findings, or the resource in its module.
    fn open_share(&mut self, ctx: &mut Ctx) {
        let Some((_, Some(share))) = self.selected_share() else { return };
        let (module, target) = (share.module, share.target.clone());
        let Some(key) = self.findings_on(share).first().map(|f| key_of(f)) else {
            return ctx.jump(module, target);
        };
        self.tab = Tab::Findings;
        self.filter.clear();
        self.service = None;
        self.min_impact = Impact::Low;
        self.rebuild();
        if let Some(pos) = self.rows.iter().position(|&i| key_of(&self.all[i]) == key) {
            self.select(pos);
        }
    }

    // ---- verified ---------------------------------------------------------------------------

    fn selected_entry(&self) -> Option<&Entry> {
        self.ledger.get(self.ledger_state.selected()?)
    }

    fn open_verify(&mut self, ctx: &mut Ctx) {
        let Some(e) = self.selected_entry() else { return };
        if let ledger::Status::Waiting(ready) = ledger::status(e, now_secs()) {
            return ctx.toast(Level::Info, format!("Too soon: the bill needs a week after the change ({})", ledger::Status::Waiting(ready).label()));
        }
        let mut form = Form::new(
            "Verify on the bill",
            vec![
                Field::fixed("Change", e.what.clone()),
                Field::fixed("Usage types", e.usage.join(", ")),
                Field::fixed("Requests", format!("1 to Cost Explorer ({} this month so far)", costs::settings::requests_this_month())),
            ],
        )
        .submit_label("read the bill")
        .enter_submits();
        form.note = Some("Cost Explorer bills every request. dcli reads the daily cost of these usage types up to two weeks before and after the change.".into());
        self.dialog = Some(Dialog::Verify { id: e.id.clone(), form, busy: false });
    }

    fn submit_verify(&mut self, ctx: &mut Ctx) {
        let Some(Dialog::Verify { id, busy, .. }) = &mut self.dialog else { return };
        let Some(entry) = self.ledger.iter().find(|e| e.id == *id).cloned() else { return };
        *busy = true;
        let (aws, id) = (ctx.env().aws.clone(), id.clone());
        ctx.spawn(move || Msg::Verified { result: ledger::verify(&aws, &entry, now_secs()), id });
    }

    fn open_forget(&mut self) {
        let Some(e) = self.selected_entry() else { return };
        let mut form = Form::new("Stop following", vec![Field::fixed("Change", e.what.clone())]).submit_label("forget").enter_submits();
        form.note = Some("Removes it from the changes dcli follows; nothing changes in AWS.".into());
        self.dialog = Some(Dialog::Forget { id: e.id.clone(), form });
    }

    // ---- copying ------------------------------------------------------------------------------

    fn outcome(&self) -> Outcome {
        let errors = self
            .checks
            .iter()
            .filter_map(|(s, c)| match c {
                Check::Failed(e) => Some((*s, e.clone())),
                _ => None,
            })
            .collect();
        Outcome {
            findings: self.all.clone(),
            errors,
            days: self.days,
            at: self.scanned_at.unwrap_or_else(now_secs),
            priced_at: self.costs.as_ref().map(|c| c.read_at),
        }
    }

    fn copy_report(&self, ctx: &mut Ctx) {
        if self.scanning {
            return ctx.toast(Level::Info, "Wait for the scan to finish");
        }
        let md = savings::report(&self.env_name, &self.region, &self.outcome());
        if crate::platform::copy_to_clipboard(&md) {
            return ctx.toast(Level::Success, "Copied the savings plan. Paste it into your AI agent.");
        }
        match crate::platform::write_temp_file(&format!("savings-{}", self.env_name), &md) {
            Ok(path) => ctx.toast(Level::Success, format!("Saved the savings plan to {}", path.display())),
            Err(e) => ctx.toast(Level::Error, format!("Could not save the plan: {e}")),
        }
    }

    fn copy_finding(&self, ctx: &mut Ctx) {
        let Some(f) = self.selected() else { return };
        let mut text = format!(
            "{} ({}, {} impact)\nFound: {}\nChange: {}\nReduction: {}\n",
            f.resource,
            f.service,
            f.impact.label(),
            f.summary,
            f.recommendation,
            f.reduction
        );
        if let Some(p) = &f.price {
            text.push_str(&format!("Saves: {} (rates of the account's bill)\n", p.describe()));
        }
        for (title, lines) in [("Evidence", &f.evidence), ("Caveats", &f.caveats)] {
            if !lines.is_empty() {
                text.push_str(&format!("{title}:\n"));
                for l in lines {
                    text.push_str(&format!("  - {l}\n"));
                }
            }
        }
        let ok = crate::platform::copy_to_clipboard(&text);
        ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Finding copied" } else { "Clipboard not available" });
    }

    // ---- rendering ----------------------------------------------------------------------------

    /// The scan's summary, the dollars, each service's state, the memory analysis, the tabs.
    fn status_lines(&self, view: &View) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let (high, medium, low) = self.all.iter().fold((0, 0, 0), |(h, m, l), f| match f.impact {
            Impact::High => (h + 1, m, l),
            Impact::Medium => (h, m + 1, l),
            Impact::Low => (h, m, l + 1),
        });
        let mut head = vec![Span::styled(format!(" Last {} days", self.days), theme::bold())];
        match (self.scanning, self.scanned_at) {
            (true, _) => head.push(Span::styled(format!(" · {} scanning", theme::spinner(view.tick)), theme::accent())),
            (false, Some(at)) => head.push(Span::styled(format!(" · scanned {}", fmt_ago(at)), theme::muted())),
            _ => {}
        }
        head.push(Span::raw(format!("   {} findings  ", self.all.len())));
        head.push(Span::styled(format!("●●● {high}  "), Style::new().fg(theme::ERR)));
        head.push(Span::styled(format!("●●○ {medium}  "), Style::new().fg(theme::WARN)));
        head.push(Span::styled(format!("●○○ {low}"), Style::new().fg(theme::ACCENT)));
        lines.push(Line::from(head));
        lines.push(self.dollars_line());

        let mut chips = vec![Span::raw(" ")];
        for (service, check) in &self.checks {
            let (symbol, style, count) = match check {
                Check::Running => (theme::spinner(view.tick).to_string(), theme::accent(), String::new()),
                Check::Done(n) => ("✓".to_string(), Style::new().fg(theme::OK), format!(" {n}")),
                Check::Failed(_) => ("✗".to_string(), Style::new().fg(theme::ERR), String::new()),
            };
            chips.push(Span::styled(symbol, style));
            chips.push(Span::raw(format!(" {}{count}   ", short_name(service))));
        }
        lines.push(Line::from(chips));

        let memory = match &self.memory {
            MemoryState::Idle => Span::styled("Lambda memory: not read · m reads it with Logs Insights (billed per GB scanned)", theme::muted()),
            MemoryState::Estimating => Span::styled(format!("Lambda memory: {} measuring the logs to scan…", theme::spinner(view.tick)), theme::accent()),
            MemoryState::Running(done, total) => Span::styled(
                format!("Lambda memory: {} reading the REPORT lines ({done} of {total} log groups)…", theme::spinner(view.tick)),
                theme::accent(),
            ),
            MemoryState::Done { functions, bytes, at, silent } => {
                let silent = if *silent > 0 { format!(", {silent} without invocations") } else { String::new() };
                Span::styled(
                    format!("Lambda memory: {functions} functions read{silent} · {} scanned · {}", savings::size(*bytes), fmt_ago(*at)),
                    theme::muted(),
                )
            }
            MemoryState::Failed(e) => Span::styled(format!("Lambda memory: {e}"), Style::new().fg(theme::ERR)),
        };
        lines.push(Line::from(vec![Span::raw(" "), memory]));
        for (service, check) in &self.checks {
            if let Check::Failed(e) = check {
                lines.push(Line::from(vec![
                    Span::styled(format!(" ✗ {service}: "), Style::new().fg(theme::ERR)),
                    Span::styled(e.short(), theme::muted()),
                ]));
            }
        }
        lines.push(self.tabs_line());
        lines
    }

    /// What the findings save together, at the bill's rates; or how to get dollars.
    fn dollars_line(&self) -> Line<'static> {
        let Some(costs) = &self.costs else {
            return Line::styled(" No dollars: reductions are in the units AWS bills · the Costs module (r there) reads your bill to price them", theme::muted());
        };
        let open = self.all.iter().filter(|f| !self.followed(f));
        match price::total(open) {
            Some(t) if t.priced > 0 => Line::from(vec![
                Span::styled(format!(" ≈ {} a month", money(t.monthly, &t.currency)), Style::new().fg(theme::OK).bold()),
                Span::raw(format!(" with your bill's rates · {} of it right away", money(t.now, &t.currency))),
                Span::styled(
                    format!(
                        " · {} priced{} · costs read {}",
                        t.priced,
                        if t.at_most > 0 { format!(", {} at most", t.at_most) } else { String::new() },
                        fmt_ago(costs.read_at)
                    ),
                    theme::muted(),
                ),
            ]),
            _ => Line::styled(format!(" No finding found on this bill's usage types · costs read {}", fmt_ago(costs.read_at)), theme::muted()),
        }
    }

    fn tabs_line(&self) -> Line<'static> {
        let mut spans = vec![Span::raw(" ")];
        for tab in TABS {
            let extra = match tab {
                Tab::Findings => format!(" {}", self.all.len()),
                Tab::Verified if !self.ledger.is_empty() => format!(" {}", self.ledger.len()),
                _ => String::new(),
            };
            let style = if tab == self.tab { Style::new().fg(Color::Black).bg(theme::ACCENT).bold() } else { theme::muted() };
            spans.push(Span::styled(format!(" {}{extra} ", tab.label()), style));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled("  ←→", theme::key()));
        Line::from(spans)
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search resources and findings");
        let mut bar_line = line;
        let service = self.service.map_or("all".to_string(), |s| short_name(s).to_string());
        let impact = match self.min_impact {
            Impact::Low => "all",
            Impact::Medium => "medium and high",
            Impact::High => "high",
        };
        bar_line.spans.push(Span::styled(format!("   s service: {service} · i impact: {impact}"), theme::muted()));
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = if self.rows.len() == self.all.len() {
            format!(" {} findings ", self.all.len())
        } else {
            format!(" {} of {} findings ", self.rows.len(), self.all.len())
        };
        if self.rows.is_empty() {
            let text = if self.scanning {
                Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" looking at the resources…")])
            } else if self.all.is_empty() && self.scanned_at.is_some() {
                Line::styled("Nothing found to change", Style::new().fg(theme::OK))
            } else {
                Line::styled("No finding matches the filters", theme::muted())
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let priced = self.costs.is_some();
        let price_w = if priced { self.rows.iter().filter_map(|&i| self.all[i].price.as_ref()).map(|p| p.short().chars().count()).max().unwrap_or(1).max(1) } else { 0 };
        // Columns as wide as what they show, within limits; the finding takes the rest.
        let widest = |text: fn(&Finding) -> &str| self.rows.iter().map(|&i| text(&self.all[i]).chars().count()).max().unwrap_or(0);
        let reduction_w = widest(|f| &f.reduction).clamp(12, (width / if priced { 4 } else { 3 }).max(12));
        let rest = width.saturating_sub(4 + 9 + reduction_w + 2 + if priced { price_w + 2 } else { 0 });
        let resource_w = widest(|f| &f.resource).clamp(8, (rest / 3).max(8));
        let summary_w = rest.saturating_sub(resource_w + 1);
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|&i| {
                let f = &self.all[i];
                let done = self.followed(f);
                let dots = if done { Span::styled("✓  ", Style::new().fg(theme::OK)) } else { Span::styled(f.impact.dots(), Style::new().fg(impact_color(f.impact))) };
                let dim = if done { theme::muted() } else { Style::new() };
                let mut spans = vec![
                    dots,
                    Span::raw(" "),
                    Span::styled(format!("{:<8} ", ellipsize(short_name(f.service), 8)), theme::muted()),
                    Span::styled(format!("{:<resource_w$} ", ellipsize(&f.resource, resource_w)), dim.bold()),
                    Span::styled(format!("{:<summary_w$}", ellipsize(&f.summary, summary_w)), dim),
                ];
                if priced {
                    let text = f.price.as_ref().map_or_else(|| "—".to_string(), |p| p.short());
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(format!("{text:>price_w$}"), if f.price.is_some() { Style::new().fg(theme::OK).bold() } else { theme::muted() }));
                }
                spans.push(Span::raw("  "));
                spans.push(Span::styled(format!("{:>reduction_w$}", ellipsize(&f.reduction, reduction_w)), theme::accent()));
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect) {
        let Some(found) = self.selected() else {
            f.render_widget(widgets::placeholder(Line::styled("", theme::muted())).block(widgets::panel(" Finding ", false)), area);
            return;
        };
        let kw = 11;
        let color = impact_color(found.impact);
        let mut lines = vec![Line::from(vec![
            Span::styled(format!("{} {} impact", found.impact.dots(), found.impact.label()), Style::new().fg(color).bold()),
            Span::styled(format!(" · {}", found.service), theme::muted()),
        ])];
        lines.push(widgets::kv("Found", found.summary.clone(), kw));
        lines.push(widgets::kv("Change", Span::styled(found.recommendation.clone(), theme::bold()), kw));
        lines.push(widgets::kv("Reduction", Span::styled(found.reduction.clone(), theme::accent()), kw));
        match (&found.price, &self.costs) {
            (Some(p), _) => {
                lines.push(widgets::kv("Saves", Span::styled(p.describe(), Style::new().fg(theme::OK).bold()), kw));
                for line in p.rate_lines() {
                    lines.push(widgets::kv("", Span::styled(format!("your bill: {line}"), theme::muted()), kw));
                }
            }
            (None, Some(_)) if !found.billed.is_empty() => {
                let usage: Vec<&str> = found.billed.iter().map(|b| b.usage.as_str()).collect();
                lines.push(widgets::kv("Saves", Span::styled(format!("not on this bill ({})", usage.join(", ")), theme::muted()), kw));
            }
            (None, None) if !found.billed.is_empty() => {
                lines.push(widgets::kv("Saves", Span::styled("dollars once the Costs module reads the bill", theme::muted()), kw));
            }
            _ => {}
        }
        if !found.evidence.is_empty() {
            lines.push(Line::raw(""));
            lines.push(widgets::section("Evidence"));
            lines.extend(found.evidence.iter().map(|e| Line::from(vec![Span::styled("· ", theme::muted()), Span::raw(e.clone())])));
        }
        let price_notes = found.price.as_ref().map_or(&[][..], |p| p.notes.as_slice());
        if !found.caveats.is_empty() || !price_notes.is_empty() {
            lines.push(Line::raw(""));
            lines.push(widgets::section("Caveats"));
            lines.extend(
                found.caveats.iter().chain(price_notes).map(|c| Line::from(vec![Span::styled("! ", Style::new().fg(theme::WARN)), Span::raw(c.clone())])),
            );
        }
        lines.push(Line::raw(""));
        let mut actions = Vec::new();
        if !found.target.is_empty() {
            actions.push(format!("Enter opens it in {}", found.module));
        }
        let key = key_of(found);
        if self.applied.contains(&key) {
            actions.push("✓ applied".to_string());
        } else if self.marked_done(found) {
            actions.push("✓ marked done".to_string());
        } else {
            if found.fix.is_some() {
                actions.push("a applies the change".to_string());
            }
            actions.push("d marks it done (the bill is compared in a week)".to_string());
        }
        lines.push(Line::styled(actions.join(" · "), theme::muted()));
        let block = widgets::panel(Line::from(format!(" {} ", found.resource)).bold(), false);
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
    }

    fn render_dialog(&self, f: &mut Frame, area: Rect, view: &View) {
        let busy = |f: &mut Frame, text: &str| {
            let body = Text::from(vec![
                Line::raw(""),
                Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw(text.to_string())]),
            ]);
            widgets::popup(f, widgets::popup_area(area, 34, 5), "Savings", theme::ACCENT, body);
        };
        match &self.dialog {
            Some(Dialog::Fix { form, busy: working, .. }) => {
                let color = if is_production(&self.env_name) { theme::ERR } else { theme::ACCENT };
                form.render(f, area, color);
                if *working {
                    busy(f, "Applying…");
                }
            }
            Some(Dialog::Verify { form, busy: working, .. }) => {
                form.render(f, area, theme::ACCENT);
                if *working {
                    busy(f, "Reading the bill…");
                }
            }
            Some(Dialog::Memory { form, .. } | Dialog::Done { form, .. } | Dialog::Forget { form, .. }) => form.render(f, area, theme::ACCENT),
            None => {}
        }
    }

    /// The list and its selection for the current tab, for keys that move.
    fn moves(&mut self) -> (&mut ListState, usize) {
        match self.tab {
            Tab::Findings => (&mut self.list_state, self.rows.len()),
            Tab::ByCost => (&mut self.cost_state, self.cost_rows.len()),
            Tab::Verified => (&mut self.ledger_state, self.ledger.len()),
        }
    }

    fn move_to(&mut self, index: usize) {
        if self.tab == Tab::Findings {
            return self.select(index);
        }
        let (state, n) = self.moves();
        if n > 0 {
            state.select(Some(index.min(n - 1)));
        }
        self.detail_scroll = 0;
    }
}

impl Module for Savings {
    fn title(&self) -> &'static str {
        "Savings"
    }

    fn description(&self) -> &'static str {
        "What could cost less · unused or oversized resources, in the units AWS bills and in dollars from your bill"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Savings::new();
        self.env_name = ctx.env().name.clone();
        self.region = ctx.env().region.clone();
        self.costs = costs::cached(&self.env_name);
        self.load_ledger();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        self.load_costs();
        if self.scanned_at.is_none() && !self.scanning {
            self.scan(ctx);
        }
        self.on_tab(ctx);
    }

    /// `service:<check>` (from the Costs module): the findings of that service.
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        if let Some(name) = target.strip_prefix("service:") {
            self.tab = Tab::Findings;
            self.filter.clear();
            self.min_impact = Impact::Low;
            self.service = CHECKS.iter().map(|(s, _)| *s).find(|s| *s == name).or_else(|| find_service(name));
            if self.service.is_none() {
                ctx.toast(Level::Info, format!("Savings has no check for {name}"));
            }
            self.rebuild();
            self.select(0);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        match &mut self.dialog {
            Some(Dialog::Fix { form, busy, .. }) => {
                if !*busy {
                    match form.handle(key) {
                        FormEvent::Submit => self.submit_fix(ctx),
                        FormEvent::Cancel => self.dialog = None,
                        FormEvent::Pending => {}
                    }
                }
                return Handled::Yes;
            }
            Some(Dialog::Memory { estimate, form }) => {
                match form.handle(key) {
                    FormEvent::Submit => self.submit_memory(ctx),
                    FormEvent::Cancel => {
                        self.dialog = None;
                        self.memory = MemoryState::Idle;
                    }
                    FormEvent::Pending => update_memory_form(form, estimate),
                }
                return Handled::Yes;
            }
            Some(Dialog::Done { form, .. }) => {
                match form.handle(key) {
                    FormEvent::Submit => self.submit_done(ctx),
                    FormEvent::Cancel => self.dialog = None,
                    FormEvent::Pending => {}
                }
                return Handled::Yes;
            }
            Some(Dialog::Verify { form, busy, .. }) => {
                if !*busy {
                    match form.handle(key) {
                        FormEvent::Submit => self.submit_verify(ctx),
                        FormEvent::Cancel => self.dialog = None,
                        FormEvent::Pending => {}
                    }
                }
                return Handled::Yes;
            }
            Some(Dialog::Forget { id, form }) => {
                match form.handle(key) {
                    FormEvent::Submit => {
                        if let Err(e) = ledger::remove(id) {
                            form.fail(e.to_string());
                            return Handled::Yes;
                        }
                        self.dialog = None;
                        self.load_ledger();
                    }
                    FormEvent::Cancel => self.dialog = None,
                    FormEvent::Pending => {}
                }
                return Handled::Yes;
            }
            None => {}
        }
        let selected = self.moves().0.selected().unwrap_or(0);
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
            KeyCode::Up | KeyCode::Char('k') => {
                self.touched = true;
                self.move_to(selected.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.touched = true;
                self.move_to(selected + 1);
            }
            KeyCode::Left => self.switch_tab(false, ctx),
            KeyCode::Right => self.switch_tab(true, ctx),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::Char('c') => self.copy_report(ctx),
            _ => return self.tab_key(key, ctx),
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        let pos = (event.column, event.row).into();
        if self.dialog.is_some() || !self.list_area.contains(pos) {
            return Handled::No;
        }
        let selected = self.moves().0.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.move_to(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.move_to(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                self.touched = true;
                // The Findings tab has a filter line above the list; every list has a border.
                let top = self.list_area.y + if self.tab == Tab::Findings { 2 } else { 1 };
                let row = event.row.saturating_sub(top) as usize;
                let offset = self.moves().0.offset();
                self.move_to(offset + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Checked { service, result } => {
                let check = match result {
                    Ok(mut found) => {
                        let n = found.len();
                        price::price_all(&mut found, self.costs.as_ref(), &self.region);
                        self.found.extend(found);
                        Check::Done(n)
                    }
                    Err(e) => Check::Failed(e),
                };
                if let Some(slot) = self.checks.iter_mut().find(|(s, _)| *s == service) {
                    slot.1 = check;
                }
                self.rebuild();
                if !self.touched {
                    self.select(0);
                }
            }
            Msg::Scanned => {
                self.scanning = false;
                self.scanned_at = Some(now_secs());
                if let Some((_, Check::Failed(e))) = self.checks.iter().find(|(_, c)| matches!(c, Check::Failed(e) if e.is_login_required())) {
                    ctx.toast(Level::Warn, e.short());
                }
            }
            Msg::Estimated(result) => match result {
                Ok(estimate) if estimate.functions.is_empty() => {
                    self.memory = MemoryState::Idle;
                    ctx.toast(Level::Info, "No function to analyse");
                }
                Ok(estimate) => {
                    self.memory = MemoryState::Idle;
                    let form = memory_form(&estimate, "Lambda memory analysis".into());
                    self.dialog = Some(Dialog::Memory { estimate, form });
                }
                Err(e) => {
                    self.memory = MemoryState::Failed(e.short());
                    ctx.toast(Level::Error, format!("Could not measure the logs: {}", e.short()));
                }
            },
            Msg::MemoryProgress(done, total) => {
                if matches!(self.memory, MemoryState::Running(..)) {
                    self.memory = MemoryState::Running(done, total);
                }
            }
            Msg::Memory(result) => self.on_memory(result, ctx),
            Msg::Applied { key, result } => {
                let Some(Dialog::Fix { form, busy, .. }) = &mut self.dialog else { return };
                *busy = false;
                match result {
                    Ok(message) => {
                        self.dialog = None;
                        self.record(&key, Source::Applied, message.clone(), ctx);
                        self.applied.insert(key);
                        ctx.toast(Level::Success, message);
                    }
                    Err(e) => form.fail(e.short()),
                }
            }
            Msg::SplitProgress(done, total) => {
                if matches!(self.splits, Splits::Running(..)) {
                    self.splits = Splits::Running(done, total);
                }
            }
            Msg::Split(splits) => {
                self.splits = Splits::Done { splits, at: now_secs() };
                self.rebuild_cost_rows();
            }
            Msg::Verified { id, result } => {
                let Some(Dialog::Verify { form, busy, .. }) = &mut self.dialog else { return };
                *busy = false;
                match result {
                    Ok(v) => {
                        self.dialog = None;
                        if let Some(mut entry) = self.ledger.iter().find(|e| e.id == id).cloned() {
                            entry.verified = Some(v);
                            let status = ledger::status(&entry, now_secs()).label();
                            if let Err(e) = ledger::put(entry) {
                                ctx.toast(Level::Warn, format!("Could not keep the result: {e}"));
                            }
                            ctx.toast(Level::Success, format!("Read the bill: {status}"));
                        }
                        self.load_ledger();
                    }
                    Err(e) => form.fail(e.short()),
                }
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let status = self.status_lines(view);
        let height = widgets::wrapped_height(&status, area.width).min(area.height / 3);
        let [top, body] = area.layout(&Layout::vertical([Constraint::Length(height), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }), top);
        let [list, detail] = body.layout(&Layout::vertical([Constraint::Percentage(55), Constraint::Fill(1)]));
        match self.tab {
            Tab::Findings => {
                self.render_list(f, list, view);
                self.render_detail(f, detail);
            }
            Tab::ByCost => {
                self.list_area = list;
                self.render_by_cost(f, list, view);
                self.render_share(f, detail);
            }
            Tab::Verified => {
                self.list_area = list;
                self.render_ledger(f, list);
                self.render_entry(f, detail);
            }
        }
        self.render_dialog(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.dialog {
            Some(Dialog::Fix { .. }) => return vec![("Enter", "apply"), ("Esc", "cancel")],
            Some(Dialog::Memory { .. }) => return vec![("←→", "window"), ("Enter", "analyse"), ("Esc", "cancel")],
            Some(Dialog::Done { .. }) => return vec![("Enter", "mark done"), ("Esc", "cancel")],
            Some(Dialog::Verify { .. }) => return vec![("Enter", "read the bill"), ("Esc", "cancel")],
            Some(Dialog::Forget { .. }) => return vec![("Enter", "forget"), ("Esc", "cancel")],
            None => {}
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        match self.tab {
            Tab::Findings => vec![
                ("↑↓", "move"),
                ("←→", "tabs"),
                ("Enter", "open resource"),
                ("a", "apply fix"),
                ("d", "mark done"),
                ("m", "Lambda memory"),
                ("s", "service"),
                ("i", "impact"),
                ("/", "search"),
                ("c", "plan for AI"),
                ("y", "copy finding"),
                ("r", "rescan"),
            ],
            Tab::ByCost => vec![("↑↓", "move"), ("←→", "tabs"), ("Enter", "its finding"), ("g", "open resource"), ("r", "split again"), ("c", "plan for AI")],
            Tab::Verified => vec![("↑↓", "move"), ("←→", "tabs"), ("v", "verify on the bill"), ("Enter", "open resource"), ("x", "forget"), ("c", "plan for AI")],
        }
    }

    fn animating(&self) -> bool {
        self.scanning
            || matches!(self.memory, MemoryState::Estimating | MemoryState::Running(..))
            || matches!(self.splits, Splits::Running(..))
            || matches!(self.dialog, Some(Dialog::Fix { busy: true, .. } | Dialog::Verify { busy: true, .. }))
    }

    fn capturing(&self) -> bool {
        self.filtering || self.dialog.is_some()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if self.dialog.is_none() && self.filtering {
            self.filter.insert_str(text, false);
            self.rebuild();
        } else if let Some(Dialog::Fix { form, busy: false, .. } | Dialog::Memory { form, .. }) = &mut self.dialog {
            form.paste(text);
        }
    }
}

impl Savings {
    /// The keys of the current tab.
    fn tab_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        match (self.tab, key.code) {
            (Tab::Findings, KeyCode::Char('/')) => self.filtering = true,
            (Tab::Findings, KeyCode::Esc) if !self.filter.value.is_empty() || self.service.is_some() || self.min_impact != Impact::Low => {
                self.filter.clear();
                self.service = None;
                self.min_impact = Impact::Low;
                self.rebuild();
            }
            (Tab::Findings, KeyCode::Char('s')) => self.next_service(),
            (Tab::Findings, KeyCode::Char('i')) => self.next_impact(),
            (Tab::Findings, KeyCode::Enter | KeyCode::Char('g')) => {
                if let Some(found) = self.selected() {
                    if found.target.is_empty() {
                        ctx.toast(Level::Info, "This one names several resources: see the evidence");
                    } else {
                        ctx.jump(found.module, found.target.clone());
                    }
                }
            }
            (Tab::Findings, KeyCode::Char('a')) => self.open_fix(ctx),
            (Tab::Findings, KeyCode::Char('d')) => self.open_done(ctx),
            (Tab::Findings, KeyCode::Char('m')) => self.start_estimate(ctx),
            (Tab::Findings, KeyCode::Char('y')) => self.copy_finding(ctx),
            (Tab::Findings, KeyCode::Char('r')) => self.scan(ctx),
            (Tab::ByCost, KeyCode::Enter) => self.open_share(ctx),
            (Tab::ByCost, KeyCode::Char('g')) => {
                if let Some((_, Some(share))) = self.selected_share() {
                    ctx.jump(share.module, share.target.clone());
                }
            }
            (Tab::ByCost, KeyCode::Char('r')) => {
                self.load_costs();
                self.start_split(ctx);
            }
            (Tab::Verified, KeyCode::Char('v')) => self.open_verify(ctx),
            (Tab::Verified, KeyCode::Char('x')) => self.open_forget(),
            (Tab::Verified, KeyCode::Enter | KeyCode::Char('g')) => {
                if let Some(e) = self.selected_entry().filter(|e| !e.target.is_empty()) {
                    let module = savings_module(&e.module);
                    ctx.jump(module, e.target.clone());
                }
            }
            (Tab::Verified, KeyCode::Char('r')) => self.load_ledger(),
            _ => return Handled::No,
        }
        Handled::Yes
    }
}

/// A module title kept as text in the ledger, as the `&'static str` a jump takes.
fn savings_module(name: &str) -> &'static str {
    const MODULES: [&str; 12] = ["Lambda", "Logs", "SQS", "DynamoDB", "S3", "Step Functions", "Secrets", "Alarms", "API Gateway", "Aurora", "Redshift", "DMS"];
    MODULES.into_iter().find(|m| *m == name).unwrap_or("Savings")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_choices_are_values_cloudwatch_accepts() {
        let accepted = [1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192, 2557, 2922, 3288, 3653];
        for choice in RETENTIONS {
            assert!(accepted.contains(&retention_days(choice)), "{choice}");
        }
        assert_eq!(leading_number("20 seconds"), 20);
        assert_eq!(leading_number("7 days"), 7);
        assert_eq!(savings_module("Aurora"), "Aurora");
        assert_eq!(savings_module("Nope"), "Savings");
    }
}

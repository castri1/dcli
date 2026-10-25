//! Advisor: the recommendations Claude leaves through `dcli mcp` for the environment, cost and
//! performance, each with what it saves or costs at the bill's rates, the evidence, and the
//! steps to copy. dcli runs none of them: the user copies, marks done (recorded in the savings
//! ledger, so the bill can confirm it) or dismisses with a reason Claude reads next time.
//!
//! The folder is looked at every second and a half, also while another module is open (for
//! the header's count), so recommendations show up while Claude writes them. Figures are priced
//! again in memory when the Costs module reads a newer bill; the files keep Claude's.

mod views;

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::ListState;

use crate::advisor::summary::{self, VIEWS, View as List};
use crate::advisor::{self, Recommendation, State, Store};
use crate::costs::{self, Snapshot, ledger};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::util::{ellipsize, sanitize};

/// How often the folder is looked at for what Claude wrote.
const POLL_EVERY: Duration = Duration::from_millis(1500);

/// The modules a recommendation can open, by title.
const MODULES: [&str; 21] = [
    "Pipelines",
    "Matrix",
    "Approvals",
    "Alarms",
    "Costs",
    "Savings",
    "Parameters",
    "Secrets",
    "Lambda",
    "API Gateway",
    "Logs",
    "CloudFormation",
    "S3",
    "Step Functions",
    "SQS",
    "DynamoDB",
    "Aurora",
    "Redshift",
    "DMS",
    "EventBridge",
    "AppConfig",
];

/// Other names a recommendation may give a module (AWS's own).
const ALIASES: [(&str, &str); 15] = [
    ("cloudwatchlogs", "Logs"),
    ("loggroups", "Logs"),
    ("parameterstore", "Parameters"),
    ("ssm", "Parameters"),
    ("secretsmanager", "Secrets"),
    ("cloudwatchalarms", "Alarms"),
    ("sfn", "Step Functions"),
    ("apigw", "API Gateway"),
    ("rds", "Aurora"),
    ("ddb", "DynamoDB"),
    ("dynamo", "DynamoDB"),
    ("cfn", "CloudFormation"),
    ("codepipeline", "Pipelines"),
    ("lambdafunctions", "Lambda"),
    ("eventbridgerules", "EventBridge"),
];

/// The module titled `name` (any case and spacing), or an AWS name for it.
pub(crate) fn module_of(name: &str) -> Option<&'static str> {
    let squash = |s: &str| s.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    let key = squash(name);
    if key.is_empty() {
        return None;
    }
    MODULES.into_iter().find(|m| squash(m) == key).or_else(|| ALIASES.into_iter().find(|(a, _)| *a == key).map(|(_, m)| m))
}

/// The text to copy for the commands of `r`: the focused step's command (or snippet), or
/// every command, each after a comment with its step. `None` when there is nothing.
/// The bool says whether any of it may change something.
pub(crate) fn commands_text(r: &Recommendation, step: Option<usize>) -> Option<(String, String, bool)> {
    if let Some(i) = step {
        let s = r.steps.get(i)?;
        return match (&s.command, &s.code) {
            (Some(c), _) => Some((sanitize(c), format!("the command of step {}", i + 1), advisor::writes(c))),
            (None, Some(code)) => {
                let what = s.language.as_deref().map_or("snippet".to_string(), |l| format!("{} snippet", sanitize(l)));
                Some((sanitize(code), format!("the {what} of step {}", i + 1), true))
            }
            _ => None,
        };
    }
    let mut text = String::new();
    let (mut count, mut writes) = (0, false);
    for (i, s) in r.steps.iter().enumerate() {
        let Some(c) = &s.command else { continue };
        let title = sanitize(s.text.lines().next().unwrap_or_default());
        text.push_str(&format!("# {}. {}\n{}\n", i + 1, ellipsize(&title, 100), sanitize(c)));
        count += 1;
        writes |= advisor::writes(c);
    }
    (count > 0).then(|| (text, if count == 1 { "1 command".to_string() } else { format!("{count} commands") }, writes))
}

/// A decision being confirmed; `seen` is when the recommendation shown was written, so a
/// version Claude rewrote meanwhile is not decided on blindly.
enum Dialog {
    Done { id: String, seen: f64, form: Form },
    Dismiss { id: String, seen: f64, form: Form },
    Restore { id: String, seen: f64, form: Form },
}

pub struct Advisor {
    env_name: String,
    store: Option<Store>,
    /// Every recommendation of the environment, in [`summary::order`].
    all: Vec<Recommendation>,
    list: List,
    /// The shown ones, as indexes into `all`.
    rows: Vec<usize>,
    list_state: ListState,
    list_area: Rect,
    detail_area: Rect,
    detail_scroll: u16,
    /// The step focused in the detail (`s`), which `c` copies.
    step: Option<usize>,
    /// The detail should scroll to the focused step at the next render.
    follow_step: bool,
    dialog: Option<Dialog>,
    /// The environment's costs as last read: they price the recommendations again.
    costs: Option<Snapshot>,
    /// The folder and the costs as last looked at: they change when Claude writes or the bill
    /// is read again.
    look: Option<(u64, Option<SystemTime>)>,
    polled: Option<Instant>,
    /// The changes the savings ledger follows in this environment (for the done ones).
    ledger: Vec<ledger::Entry>,
    /// Recommendations that arrived while dcli was open, marked until selected.
    fresh: HashSet<String>,
    loaded: bool,
}

impl Advisor {
    pub fn new() -> Advisor {
        Advisor {
            env_name: String::new(),
            store: Store::home(),
            all: Vec::new(),
            list: List::Pending,
            rows: Vec::new(),
            list_state: ListState::default(),
            list_area: Rect::default(),
            detail_area: Rect::default(),
            detail_scroll: 0,
            step: None,
            follow_step: false,
            dialog: None,
            costs: None,
            look: None,
            polled: None,
            ledger: Vec::new(),
            fresh: HashSet::new(),
            loaded: false,
        }
    }

    /// The folder's signature and when the costs were last written.
    fn looks(&self) -> (u64, Option<SystemTime>) {
        let folder = self.store.as_ref().map_or(0, |s| s.signature(&self.env_name));
        let costs = costs::cache_path(&self.env_name).and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok());
        (folder, costs)
    }

    /// Reads the recommendations again, keeping the selection; returns those new since the last
    /// reading.
    fn reload(&mut self) -> Vec<String> {
        let keep = self.selected().map(|r| r.id.clone());
        let before: HashSet<String> = self.all.iter().map(|r| r.id.clone()).collect();
        self.look = Some(self.looks());
        self.costs = costs::cached(&self.env_name);
        let mut all = self.store.as_ref().map(|s| s.list(&self.env_name)).unwrap_or_default();
        for r in &mut all {
            r.reprice(self.costs.as_ref());
        }
        summary::order(&mut all);
        let new: Vec<String> = if self.loaded { all.iter().filter(|r| !before.contains(&r.id)).map(|r| r.id.clone()).collect() } else { Vec::new() };
        self.fresh.extend(new.iter().cloned());
        self.all = all;
        self.ledger = ledger::of_env(&self.env_name);
        self.loaded = true;
        self.rebuild(keep);
        new
    }

    /// The rows of the current list; the selection stays on `keep` while it is there.
    fn rebuild(&mut self, keep: Option<String>) {
        let old = self.list_state.selected().unwrap_or(0);
        self.rows = (0..self.all.len()).filter(|&i| self.list.includes(self.all[i].state)).collect();
        let index = keep.and_then(|id| self.rows.iter().position(|&i| self.all[i].id == id)).unwrap_or(old);
        if self.rows.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(index.min(self.rows.len() - 1)));
        }
        self.seen();
    }

    fn selected(&self) -> Option<&Recommendation> {
        self.all.get(*self.rows.get(self.list_state.selected()?)?)
    }

    /// The selected one is no longer new.
    fn seen(&mut self) {
        if let Some(id) = self.selected().map(|r| r.id.clone()) {
            self.fresh.remove(&id);
        }
    }

    fn select(&mut self, index: usize) {
        if self.rows.is_empty() {
            return;
        }
        let index = index.min(self.rows.len() - 1);
        if self.list_state.selected() != Some(index) {
            self.detail_scroll = 0;
            self.step = None;
        }
        self.list_state.select(Some(index));
        self.seen();
    }

    fn switch_list(&mut self, forward: bool) {
        let i = VIEWS.iter().position(|v| *v == self.list).unwrap_or(0);
        self.list = VIEWS[if forward { (i + 1) % VIEWS.len() } else { (i + VIEWS.len() - 1) % VIEWS.len() }];
        self.list_state.select(Some(0));
        self.detail_scroll = 0;
        self.step = None;
        self.rebuild(None);
    }

    /// Shows recommendation `id` wherever it is.
    fn show(&mut self, id: &str) -> bool {
        let Some(r) = self.all.iter().find(|r| r.id.eq_ignore_ascii_case(id)) else { return false };
        let (list, id) = (List::of(r.state), r.id.clone());
        if list != self.list {
            self.list = list;
            self.step = None;
        }
        self.rebuild(Some(id));
        true
    }

    fn count(&self, list: List) -> usize {
        self.all.iter().filter(|r| list.includes(r.state)).count()
    }

    /// The ledger entry a done recommendation recorded, if it is still there.
    fn entry_of(&self, r: &Recommendation) -> Option<&ledger::Entry> {
        let id = r.ledger.as_deref()?;
        self.ledger.iter().find(|e| e.id == id)
    }

    // ---- actions ----------------------------------------------------------------------------

    fn open_resource(&self, ctx: &mut Ctx) {
        let Some(r) = self.selected() else { return };
        let Some(res) = r.resource() else {
            return ctx.toast(Level::Info, "It names no resource: see the steps");
        };
        let target = if res.target.is_empty() { res.name.clone() } else { res.target.clone() };
        match module_of(&res.module) {
            Some(module) if !target.is_empty() => ctx.jump(module, target),
            _ => ctx.toast(Level::Info, format!("No dcli module opens {} ({}): use the steps", sanitize(&res.name), sanitize(&res.module))),
        }
    }

    fn focus_steps(&mut self, ctx: &mut Ctx) {
        let Some(n) = self.selected().map(|r| r.steps.len()) else { return };
        if n == 0 {
            return ctx.toast(Level::Info, "It has no steps");
        }
        self.step = match self.step {
            Some(_) => None,
            None => Some(0),
        };
        self.follow_step = true;
    }

    fn move_step(&mut self, forward: bool) {
        let (Some(i), Some(n)) = (self.step, self.selected().map(|r| r.steps.len())) else { return };
        self.step = Some(if forward { (i + 1).min(n.saturating_sub(1)) } else { i.saturating_sub(1) });
        self.follow_step = true;
    }

    fn copy_commands(&self, ctx: &mut Ctx) {
        let Some(r) = self.selected() else { return };
        let Some((text, what, writes)) = commands_text(r, self.step) else {
            let message = match self.step {
                Some(i) => format!("Step {} has no command: it is done by hand", i + 1),
                None => "No command to copy: m copies the whole recommendation".to_string(),
            };
            return ctx.toast(Level::Info, message);
        };
        if !crate::platform::copy_to_clipboard(&text) {
            return ctx.toast(Level::Warn, "Clipboard not available: m saves the recommendation to a file");
        }
        if writes {
            ctx.toast(Level::Success, format!("Copied {what}: it can change the account, read it before running it"));
        } else {
            ctx.toast(Level::Success, format!("Copied {what} (read-only)"));
        }
    }

    fn copy_markdown(&self, ctx: &mut Ctx) {
        let Some(r) = self.selected() else { return };
        let md = advisor::markdown(r);
        if crate::platform::copy_to_clipboard(&md) {
            return ctx.toast(Level::Success, format!("Copied {} as Markdown", r.id));
        }
        match crate::platform::write_temp_file(&format!("advisor-{}-{}", self.env_name, r.id), &md) {
            Ok(path) => ctx.toast(Level::Success, format!("Saved {} to {}", r.id, path.display())),
            Err(e) => ctx.toast(Level::Error, format!("Could not save it: {e}")),
        }
    }

    fn open_done(&mut self, ctx: &mut Ctx) {
        let Some(r) = self.selected() else { return };
        if r.state == State::Done {
            return ctx.toast(Level::Info, "Already done: u puts it back to pending");
        }
        let entry = advisor::ledger_entry(r);
        let bill = match &entry {
            Some(e) => {
                let expected = e.expected.map(|x| format!(" · expected ≈ {}/mo", crate::savings::price::money(x, &e.currency))).unwrap_or_default();
                format!("{}: {}{expected}", e.service, e.usage.join(", "))
            }
            None => "names no usage of the bill: not followed there".to_string(),
        };
        let mut form = Form::new(
            format!("Mark {} as done", r.id),
            vec![Field::fixed("Recommendation", ellipsize(&sanitize(&r.title), 70)), Field::fixed("On the bill", sanitize(&bill))],
        )
        .submit_label("mark done")
        .enter_submits();
        form.note = Some(if entry.is_some() {
            "For a change you made (the steps, in code or in the console): dcli ran nothing. It goes to the savings ledger, and \
             after 7 complete days Savings › Verified compares the bill before and after (v there)."
                .into()
        } else {
            "For a change you made (the steps, in code or in the console): dcli ran nothing and only records that you made it.".into()
        });
        self.dialog = Some(Dialog::Done { id: r.id.clone(), seen: r.updated, form });
    }

    fn open_dismiss(&mut self, ctx: &mut Ctx) {
        let Some(r) = self.selected() else { return };
        match r.state {
            State::Done => return ctx.toast(Level::Info, "Already done: u puts it back to pending first"),
            State::Dismissed => return ctx.toast(Level::Info, "Already dismissed: u puts it back to pending"),
            State::Pending | State::Withdrawn => {}
        }
        let mut form = Form::new(
            format!("Dismiss {}", r.id),
            vec![
                Field::fixed("Recommendation", ellipsize(&sanitize(&r.title), 70)),
                Field::text("Reason", "").optional("why not (Claude reads it before recommending again)"),
            ],
        )
        .submit_label("dismiss")
        .enter_submits()
        .focus_on("Reason");
        form.note = Some(
            "Nothing changes in AWS. Claude sees it dismissed, with your reason, and does not propose it again unless something changes."
                .into(),
        );
        self.dialog = Some(Dialog::Dismiss { id: r.id.clone(), seen: r.updated, form });
    }

    fn restore(&mut self, ctx: &mut Ctx) {
        let Some(r) = self.selected() else { return };
        match r.state {
            State::Pending => ctx.toast(Level::Info, "Already pending"),
            State::Done => {
                let mut form = Form::new(format!("{} back to pending", r.id), vec![Field::fixed("Recommendation", ellipsize(&sanitize(&r.title), 70))])
                    .submit_label("back to pending")
                    .enter_submits();
                form.note = Some("Not made after all: it also leaves the changes Savings › Verified follows.".into());
                self.dialog = Some(Dialog::Restore { id: r.id.clone(), seen: r.updated, form });
            }
            State::Dismissed | State::Withdrawn => {
                let (id, seen) = (r.id.clone(), r.updated);
                self.decide(&id, State::Pending, "", seen, ctx);
            }
        }
    }

    /// Records the user's decision on `id`; the error, if any, for the form.
    fn decide(&mut self, id: &str, state: State, note: &str, seen: f64, ctx: &mut Ctx) -> Option<String> {
        let Some(store) = self.store.clone() else {
            return Some("cannot find the home directory".into());
        };
        match advisor::decide(&store, &self.env_name, id, state, note, Some(seen)) {
            Ok(r) => {
                let message = match state {
                    State::Done if r.ledger.is_some() => {
                        format!("{} marked done · Savings › Verified compares the bill after 7 complete days", r.id)
                    }
                    State::Done => format!("{} marked done", r.id),
                    State::Dismissed => format!("{} dismissed: Claude reads why before recommending again", r.id),
                    _ => format!("{} is pending again", r.id),
                };
                ctx.toast(Level::Success, message);
                self.reload();
                None
            }
            Err(e) => {
                ctx.toast(Level::Error, e.clone());
                self.reload();
                Some(e)
            }
        }
    }

    fn dialog_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(dialog) = &mut self.dialog else { return };
        let (id, seen, form) = match dialog {
            Dialog::Done { id, seen, form } | Dialog::Dismiss { id, seen, form } | Dialog::Restore { id, seen, form } => (id.clone(), *seen, form),
        };
        match form.handle(key) {
            FormEvent::Cancel => self.dialog = None,
            FormEvent::Pending => {}
            FormEvent::Submit => {
                let (state, note) = match dialog {
                    Dialog::Done { .. } => (State::Done, String::new()),
                    Dialog::Dismiss { form, .. } => (State::Dismissed, form.get("Reason").trim().to_string()),
                    Dialog::Restore { .. } => (State::Pending, String::new()),
                };
                let dialog = self.dialog.take();
                if let Some(error) = self.decide(&id, state, &note, seen, ctx) {
                    self.dialog = dialog;
                    if let Some(Dialog::Done { form, .. } | Dialog::Dismiss { form, .. } | Dialog::Restore { form, .. }) = &mut self.dialog {
                        form.fail(error);
                    }
                }
            }
        }
    }
}

impl Module for Advisor {
    fn title(&self) -> &'static str {
        "Advisor"
    }

    fn description(&self) -> &'static str {
        "Claude's recommendations (dcli mcp) · cost and performance, priced with your bill, steps to copy"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Advisor::new();
        self.env_name = ctx.env().name.clone();
        self.reload();
        self.polled = Some(Instant::now());
    }

    fn activate(&mut self, _ctx: &mut Ctx) {
        self.reload();
    }

    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if self.polled.is_some_and(|t| t.elapsed() < POLL_EVERY) {
            return;
        }
        self.polled = Some(Instant::now());
        if self.look == Some(self.looks()) {
            return;
        }
        let new = self.reload();
        if !active || new.is_empty() {
            return;
        }
        match new.as_slice() {
            [id] => {
                let title = self.all.iter().find(|r| r.id == *id).map(|r| ellipsize(&sanitize(&r.title), 60)).unwrap_or_default();
                ctx.toast(Level::Info, format!("Claude wrote {id}: {title}"));
            }
            many => ctx.toast(Level::Info, format!("Claude wrote {} recommendations", many.len())),
        }
    }

    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        self.reload();
        if !target.is_empty() && !self.show(target) {
            ctx.toast(Level::Info, format!("{} has no recommendation {}", self.env_name, sanitize(target)));
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() {
            self.dialog_key(key, ctx);
            return Handled::Yes;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        let stepping = self.step.is_some();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if stepping => self.move_step(false),
            KeyCode::Down | KeyCode::Char('j') if stepping => self.move_step(true),
            KeyCode::Esc if stepping => self.step = None,
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(usize::MAX),
            KeyCode::Left => self.switch_list(false),
            KeyCode::Right => self.switch_list(true),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::Enter | KeyCode::Char('g') => self.open_resource(ctx),
            KeyCode::Char('s') => self.focus_steps(ctx),
            KeyCode::Char('c') => self.copy_commands(ctx),
            KeyCode::Char('m') => self.copy_markdown(ctx),
            KeyCode::Char('d') => self.open_done(ctx),
            KeyCode::Char('x') => self.open_dismiss(ctx),
            KeyCode::Char('u') => self.restore(ctx),
            KeyCode::Char('r') => {
                self.reload();
                ctx.toast(Level::Info, format!("{} recommendations", self.all.len()));
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() {
            return Handled::No;
        }
        let pos = (event.column, event.row).into();
        let selected = self.list_state.selected().unwrap_or(0);
        if self.detail_area.contains(pos) {
            match event.kind {
                MouseEventKind::ScrollUp => self.detail_scroll = self.detail_scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => self.detail_scroll = self.detail_scroll.saturating_add(3),
                _ => return Handled::No,
            }
            return Handled::Yes;
        }
        if !self.list_area.contains(pos) {
            return Handled::No;
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = event.row.saturating_sub(self.list_area.y + 1) as usize;
                let offset = self.list_state.offset();
                if offset + row < self.rows.len() {
                    self.select(offset + row);
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        self.render_all(f, area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.dialog {
            Some(Dialog::Done { .. }) => return vec![("Enter", "mark done"), ("Esc", "cancel")],
            Some(Dialog::Dismiss { .. }) => return vec![("Enter", "dismiss"), ("Esc", "cancel")],
            Some(Dialog::Restore { .. }) => return vec![("Enter", "back to pending"), ("Esc", "cancel")],
            None => {}
        }
        if self.all.is_empty() {
            return vec![("r", "reload")];
        }
        if self.step.is_some() {
            return vec![("↑↓", "step"), ("c", "copy this step"), ("Esc", "back to the list"), ("m", "copy as Markdown"), ("d", "mark done")];
        }
        vec![
            ("↑↓", "move"),
            ("←→", "pending · done · dismissed"),
            ("Enter", "open resource"),
            ("s", "steps"),
            ("c", "copy commands"),
            ("m", "copy as Markdown"),
            ("d", "mark done"),
            ("x", "dismiss"),
            ("u", "back to pending"),
            ("r", "reload"),
        ]
    }

    fn badge(&self) -> Option<(String, Color)> {
        let pending = self.count(List::Pending);
        (pending > 0).then(|| (pending.to_string(), theme::ACCENT))
    }

    fn capturing(&self) -> bool {
        self.dialog.is_some()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if let Some(Dialog::Dismiss { form, .. }) = &mut self.dialog {
            form.paste(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::Step;
    use crate::advisor::summary::tests::rec;

    #[test]
    fn modules_by_title_or_aws_name() {
        assert_eq!(module_of("Lambda"), Some("Lambda"));
        assert_eq!(module_of("api gateway"), Some("API Gateway"));
        assert_eq!(module_of("CloudWatch Logs"), Some("Logs"));
        assert_eq!(module_of("Secrets Manager"), Some("Secrets"));
        assert_eq!(module_of("step-functions"), Some("Step Functions"));
        assert_eq!(module_of("EC2"), None);
        assert_eq!(module_of(""), None);
    }

    #[test]
    fn commands_to_copy() {
        let mut r = rec(1, State::Pending, None, None);
        r.steps = vec![
            Step { text: "Check the memory\nfirst".into(), command: Some("aws lambda get-function-configuration --function-name orders".into()), ..Default::default() },
            Step { text: "Change it in the template".into(), code: Some("MemorySize: 1024".into()), language: Some("yaml".into()), ..Default::default() },
            Step { text: "Apply".into(), command: Some("aws lambda update-function-configuration --function-name orders --memory-size 1024".into()), ..Default::default() },
            Step { text: "Watch the duration for a day".into(), ..Default::default() },
        ];
        let (all, what, writes) = commands_text(&r, None).unwrap();
        assert_eq!(
            all,
            "# 1. Check the memory\naws lambda get-function-configuration --function-name orders\n\
             # 3. Apply\naws lambda update-function-configuration --function-name orders --memory-size 1024\n"
        );
        assert_eq!((what.as_str(), writes), ("2 commands", true));
        let (first, what, writes) = commands_text(&r, Some(0)).unwrap();
        assert!(first.starts_with("aws lambda get-function-configuration") && !writes, "{what}");
        let (code, what, _) = commands_text(&r, Some(1)).unwrap();
        assert_eq!((code.as_str(), what.as_str()), ("MemorySize: 1024", "the yaml snippet of step 2"));
        assert!(commands_text(&r, Some(3)).is_none() && commands_text(&r, Some(9)).is_none());
        r.steps.retain(|s| s.command.is_none());
        assert!(commands_text(&r, None).is_none());
    }
}

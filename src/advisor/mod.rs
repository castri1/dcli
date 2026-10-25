//! Recommendations Claude leaves through dcli's MCP server (`dcli mcp`): cost and performance
//! changes for an environment, each with what was found, the evidence, the steps to copy and
//! run (dcli runs none of them), the risk, how to undo it, how to check it worked, and what it
//! saves or costs a month from the account's own bill ([`estimate`]). The Advisor module and
//! `dcli advisor` show them; marking one done records it in the savings ledger
//! ([`crate::costs::ledger`]), so the bill can confirm it later.
//!
//! Each recommendation is a file, `~/.deployment-cli/advisor/<env>/R<n>.json`: the MCP server
//! and the UI run in different processes, and each writes whole files of its own, so neither
//! loses what the other wrote. Claude writes the content; the user sets the state (done,
//! dismissed), and Claude never overrides it.

#![allow(dead_code)] // used by the MCP server and the Advisor module

pub mod estimate;
pub mod summary;

use std::fmt::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::costs::{self, Snapshot, ledger};
use crate::savings::price;
use crate::util::{md_block, md_code, now_secs, sanitize};

/// Recommendations kept per environment, at most.
pub const MAX_PER_ENV: u32 = 500;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    #[default]
    Cost,
    Performance,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Cost => "cost",
            Category::Performance => "performance",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Low,
    #[default]
    Medium,
    High,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Low => "low",
            Level::Medium => "medium",
            Level::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Waiting for the user.
    #[default]
    Pending,
    /// The user made the change (recorded in the savings ledger when it touches the bill).
    Done,
    /// The user does not want it; `state_note` says why.
    Dismissed,
    /// Claude took it back (no longer true, replaced by another); `state_note` says why.
    Withdrawn,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Done => "done",
            State::Dismissed => "dismissed",
            State::Withdrawn => "withdrawn",
        }
    }
}

/// A resource a recommendation is about, and how dcli opens it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResourceRef {
    /// The title of the dcli module that opens it (`Lambda`, `Aurora`…), or empty.
    pub module: String,
    /// What that module's `open` takes (a function name, a cluster id…).
    pub target: String,
    /// As shown.
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Step {
    /// What to do, in words.
    pub text: String,
    /// A shell command to copy and run, on one line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// A snippet to paste (infrastructure as code: CloudFormation, Terraform, CDK…).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The snippet's language: `yaml`, `hcl`, `typescript`…
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// A figure Claude worked out for what the bill cannot price, and how.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClaudeEstimate {
    /// A month: positive saves, negative costs more.
    pub monthly: f64,
    pub reasoning: String,
}

/// What a recommendation saves or costs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Cost {
    /// The usage it changes, as the bill names it: priced again whenever newer costs are read.
    pub changes: Vec<estimate::Change>,
    /// Those changes at the bill's rates, as last priced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bill: Option<estimate::Estimate>,
    /// Claude's own figure, for what the bill cannot price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeEstimate>,
    /// No effect on the bill, and why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub none: Option<String>,
}

impl Cost {
    /// A month, from the bill and Claude's figure together: positive saves.
    pub fn monthly(&self) -> Option<f64> {
        let bill = self.bill.as_ref().filter(|b| b.priced()).map(|b| b.monthly);
        let claude = self.claude.as_ref().map(|c| c.monthly);
        match (bill, claude) {
            (None, None) => None,
            (b, c) => Some(b.unwrap_or(0.0) + c.unwrap_or(0.0)),
        }
    }

    /// Whether the whole figure comes from the bill.
    pub fn all_from_bill(&self) -> bool {
        self.claude.is_none() && self.bill.as_ref().is_some_and(|b| b.priced() && b.unpriced.is_empty())
    }

    pub fn currency(&self) -> &str {
        self.bill.as_ref().map_or("USD", |b| b.currency.as_str())
    }

    /// For a narrow column: `≈ US$ 412/mo`, `≤ US$ 50/mo`, `+US$ 120/mo`, `—`.
    pub fn short(&self) -> String {
        let Some(m) = self.monthly() else { return "—".into() };
        let money = price::money(m.abs(), self.currency());
        if m < 0.0 {
            format!("+{money}/mo")
        } else if self.bill.as_ref().is_some_and(|b| b.at_most) {
            format!("≤ {money}/mo")
        } else {
            format!("≈ {money}/mo")
        }
    }

    /// Where the figure comes from: `your bill`, `Claude's estimate`, `your bill and Claude's
    /// estimate`, or why there is none.
    pub fn source(&self) -> String {
        let bill = self.bill.as_ref().is_some_and(estimate::Estimate::priced);
        match (bill, self.claude.is_some(), &self.none) {
            (true, true, _) => "your bill and Claude's estimate".into(),
            (true, false, _) => "your bill".into(),
            (false, true, _) => "Claude's estimate".into(),
            (false, false, Some(why)) => format!("no cost change: {why}"),
            (false, false, None) => "not priced: read the environment's costs".into(),
        }
    }
}

/// A recommendation, as kept on disk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Recommendation {
    /// `R7`: numbered per environment.
    pub id: String,
    pub env: String,
    pub region: String,
    /// The AWS account, when known.
    pub account: String,
    /// Unix seconds.
    pub created: f64,
    pub updated: f64,
    /// Who wrote it: the MCP client (`claude-code 2.1.0`).
    pub author: String,
    pub category: Category,
    /// One line: `Lower orders-api's memory to 1024 MB`.
    pub title: String,
    pub resources: Vec<ResourceRef>,
    /// What was found.
    pub finding: String,
    /// The measurements behind it.
    pub evidence: Vec<String>,
    pub steps: Vec<Step>,
    pub risk: Level,
    pub risk_note: String,
    /// How to undo it.
    pub rollback: String,
    /// How to check it worked.
    pub verify: String,
    pub effort: Level,
    pub confidence: Level,
    /// The expected effect on performance (`p95 −30 %`), if any.
    pub performance: String,
    pub cost: Cost,
    pub state: State,
    /// Why it was dismissed or withdrawn.
    pub state_note: String,
    pub state_at: f64,
    /// The savings ledger entry recorded when it was marked done.
    pub ledger: Option<String>,
}

impl Recommendation {
    /// `7` for `R7`.
    pub fn number(&self) -> u32 {
        number(&self.id).unwrap_or(0)
    }

    /// The first resource, which the module opens.
    pub fn resource(&self) -> Option<&ResourceRef> {
        self.resources.first()
    }

    /// Prices its changes again with `costs` when those are newer than its last pricing;
    /// true when it changed.
    pub fn reprice(&mut self, costs: Option<&Snapshot>) -> bool {
        let Some(s) = costs else { return false };
        if self.cost.changes.is_empty() || self.cost.bill.as_ref().is_some_and(|b| b.priced_at >= s.read_at) {
            return false;
        }
        self.cost.bill = Some(estimate::estimate(&self.cost.changes, s, &self.region));
        true
    }
}

// ---- drafts: what the MCP server receives ------------------------------------------------

/// A recommendation as Claude writes it; [`submit`] checks it and keeps it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Draft {
    /// To change an existing one.
    pub id: Option<String>,
    pub category: Category,
    pub title: String,
    pub resources: Vec<ResourceRef>,
    pub finding: String,
    pub evidence: Vec<String>,
    pub steps: Vec<Step>,
    pub risk: Level,
    pub risk_note: String,
    pub rollback: String,
    pub verify: String,
    pub effort: Level,
    pub confidence: Level,
    pub performance: String,
    pub cost: CostDraft,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CostDraft {
    pub changes: Vec<estimate::Change>,
    pub claude_estimate: Option<ClaudeEstimate>,
    pub no_cost_change: Option<String>,
}

/// Where a draft is kept: the environment and who sends it.
pub struct Origin<'a> {
    pub env: &'a str,
    pub region: &'a str,
    pub account: &'a str,
    pub author: &'a str,
}

/// One line of at most `max` characters (newlines and tabs become spaces).
fn line(text: &str, max: usize, what: &str, required: bool) -> Result<String, String> {
    let t = sanitize(text).replace(['\n', '\t', '\r'], " ").trim().to_string();
    check_len(t, max, what, required)
}

/// Text of at most `max` characters.
fn text(text: &str, max: usize, what: &str, required: bool) -> Result<String, String> {
    check_len(sanitize(text).trim().to_string(), max, what, required)
}

fn check_len(t: String, max: usize, what: &str, required: bool) -> Result<String, String> {
    if required && t.is_empty() {
        return Err(format!("`{what}` is required"));
    }
    if t.chars().count() > max {
        return Err(format!("`{what}` is longer than {max} characters"));
    }
    Ok(t)
}

impl Draft {
    /// The draft with its text cleaned and checked (lengths, one-line fields, a cost figure
    /// or why there is none); an error says what to fix.
    fn checked(&self) -> Result<Draft, String> {
        if self.resources.len() > 20 {
            return Err("at most 20 resources".into());
        }
        if self.evidence.len() > 20 {
            return Err("at most 20 lines of evidence".into());
        }
        if self.steps.is_empty() || self.steps.len() > 15 {
            return Err("give 1 to 15 steps".into());
        }
        let resources = self
            .resources
            .iter()
            .map(|r| {
                Ok(ResourceRef {
                    module: line(&r.module, 40, "resources.module", false)?,
                    target: line(&r.target, 2048, "resources.target", false)?,
                    name: line(&r.name, 300, "resources.name", true)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let steps = self
            .steps
            .iter()
            .map(|s| {
                let command = match s.command.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
                    Some(c) if c.contains(['\n', '\r']) => return Err("a step's `command` is one line: put scripts in `code`".to_string()),
                    Some(c) if c.chars().any(crate::util::is_unsafe) => {
                        return Err("a step's `command` has control or invisible characters".to_string());
                    }
                    Some(c) => Some(text(c, 2000, "steps.command", true)?),
                    None => None,
                };
                let code = match s.code.as_deref().filter(|c| !c.trim().is_empty()) {
                    Some(c) => Some(text(c, 10_000, "steps.code", true)?),
                    None => None,
                };
                let language = s.language.as_deref().map(|l| l.trim().to_ascii_lowercase()).filter(|l| !l.is_empty());
                if language.as_deref().is_some_and(|l| l.len() > 20 || !l.chars().all(|c| c.is_ascii_alphanumeric() || "+-#.".contains(c))) {
                    return Err("a step's `language` is a short name: yaml, hcl, json, typescript…".into());
                }
                Ok(Step { text: text(&s.text, 1000, "steps.text", true)?, command, code, language })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let evidence = self.evidence.iter().map(|e| line(e, 600, "evidence", true)).collect::<Result<Vec<_>, String>>()?;
        estimate::validate(&self.cost.changes)?;
        let claude_estimate = match &self.cost.claude_estimate {
            Some(c) if !c.monthly.is_finite() || c.monthly.abs() > 1e9 => return Err("`claudeEstimate.monthly` is a number of dollars a month".into()),
            Some(c) => Some(ClaudeEstimate { monthly: c.monthly, reasoning: text(&c.reasoning, 2000, "claudeEstimate.reasoning", true)? }),
            None => None,
        };
        let no_cost_change = match &self.cost.no_cost_change {
            Some(why) => Some(line(why, 500, "noCostChange", true)?),
            None => None,
        };
        if self.cost.changes.is_empty() && claude_estimate.is_none() {
            match (&no_cost_change, self.category) {
                (None, _) => {
                    return Err("every recommendation needs a cost estimate: `changes` priced with the bill (try them with estimate_cost), \
                         `claudeEstimate` with your reasoning, or for a performance change without effect on the bill, `noCostChange` saying why"
                        .into());
                }
                (Some(_), Category::Cost) => return Err("a cost recommendation needs a figure: `changes` or `claudeEstimate`".into()),
                _ => {}
            }
        }
        Ok(Draft {
            id: self.id.clone(),
            category: self.category,
            title: line(&self.title, 140, "title", true)?,
            resources,
            finding: text(&self.finding, 3000, "finding", true)?,
            evidence,
            steps,
            risk: self.risk,
            risk_note: text(&self.risk_note, 1000, "riskNote", false)?,
            rollback: text(&self.rollback, 2000, "rollback", true)?,
            verify: text(&self.verify, 2000, "verify", true)?,
            effort: self.effort,
            confidence: self.confidence,
            performance: line(&self.performance, 500, "performance", false)?,
            cost: CostDraft { changes: self.cost.changes.clone(), claude_estimate, no_cost_change },
        })
    }
}

/// Checks `draft` and keeps it as a new recommendation of `origin.env`, or as the new content of
/// the one it names; `costs` (the environment's, if read) price its changes. The stored
/// recommendation comes back, or what to fix.
pub fn submit(store: &Store, draft: &Draft, origin: &Origin, costs: Option<&Snapshot>) -> Result<Recommendation, String> {
    let d = draft.checked()?;
    let bill = match (costs, d.cost.changes.is_empty()) {
        (Some(s), false) => Some(estimate::estimate(&d.cost.changes, s, origin.region)),
        _ => None,
    };
    if !d.cost.changes.is_empty() && d.cost.claude_estimate.is_none() {
        match &bill {
            None => {
                return Err(format!(
                    "the costs of '{}' have not been read, so the bill cannot price `changes`: ask the user to run `dcli costs {} --refresh` \
                     (Cost Explorer bills each request), or give `claudeEstimate` with your reasoning",
                    origin.env, origin.env
                ));
            }
            Some(b) if !b.priced() => {
                return Err(format!(
                    "the bill has no rate for {}: use usage types it has (estimate_cost lists them) or give `claudeEstimate` with your reasoning",
                    b.unpriced.join(", ")
                ));
            }
            _ => {}
        }
    }
    let now = now_secs();
    let content = |r: &mut Recommendation| {
        r.category = d.category;
        r.title = d.title.clone();
        r.resources = d.resources.clone();
        r.finding = d.finding.clone();
        r.evidence = d.evidence.clone();
        r.steps = d.steps.clone();
        r.risk = d.risk;
        r.risk_note = d.risk_note.clone();
        r.rollback = d.rollback.clone();
        r.verify = d.verify.clone();
        r.effort = d.effort;
        r.confidence = d.confidence;
        r.performance = d.performance.clone();
        r.cost = Cost { changes: d.cost.changes.clone(), bill: bill.clone(), claude: d.cost.claude_estimate.clone(), none: d.cost.no_cost_change.clone() };
        r.updated = now;
        if !origin.account.is_empty() {
            r.account = origin.account.to_string();
        }
    };
    let existing = store.list(origin.env);
    if let Some(id) = d.id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        let Some(mut r) = existing.into_iter().find(|r| r.id.eq_ignore_ascii_case(id)) else {
            return Err(format!("'{}' has no recommendation {id}", origin.env));
        };
        match r.state {
            State::Dismissed => {
                return Err(format!(
                    "the user dismissed {} ({}): leave it, or if something changed, write a new one that says what",
                    r.id,
                    if r.state_note.is_empty() { "no reason given" } else { &r.state_note }
                ));
            }
            State::Done => return Err(format!("the user already made {}: write a new one for a further change", r.id)),
            State::Pending | State::Withdrawn => {}
        }
        content(&mut r);
        r.state = State::Pending;
        r.state_note.clear();
        r.author = origin.author.to_string();
        store.save(&r).map_err(|e| format!("cannot save {}: {e}", r.id))?;
        return Ok(r);
    }
    let key = |title: &str| title.to_lowercase();
    let same = |r: &Recommendation| {
        r.state == State::Pending
            && key(&r.title) == key(&d.title)
            && r.resources.iter().map(|x| &x.name).eq(d.resources.iter().map(|x| &x.name))
    };
    if let Some(r) = existing.iter().find(|r| same(r)) {
        return Err(format!("{} already recommends this: pass `id: \"{}\"` to change it", r.id, r.id));
    }
    if existing.len() >= MAX_PER_ENV as usize {
        return Err(format!("'{}' has {MAX_PER_ENV} recommendations: withdraw old ones first", origin.env));
    }
    let mut r = Recommendation {
        env: origin.env.to_string(),
        region: origin.region.to_string(),
        account: origin.account.to_string(),
        author: origin.author.to_string(),
        created: now,
        ..Default::default()
    };
    content(&mut r);
    store.create(&mut r).map_err(|e| format!("cannot save the recommendation: {e}"))?;
    Ok(r)
}

/// Claude takes back a pending recommendation of `env` (no longer true, replaced).
pub fn withdraw(store: &Store, env: &str, id: &str, reason: &str) -> Result<Recommendation, String> {
    let reason = line(reason, 500, "reason", true)?;
    let mut r = store.get(env, id).ok_or_else(|| format!("'{env}' has no recommendation {id}"))?;
    if r.state != State::Pending {
        return Err(format!("{} is {} already", r.id, r.state.label()));
    }
    r.state = State::Withdrawn;
    r.state_note = reason;
    r.state_at = now_secs();
    store.save(&r).map_err(|e| format!("cannot save {}: {e}", r.id))?;
    Ok(r)
}

/// The user's decision on a recommendation: done (recorded in the savings ledger when it changes
/// the bill's usage), dismissed with a reason, or pending again.
pub fn decide(store: &Store, env: &str, id: &str, state: State, note: &str) -> Result<Recommendation, String> {
    let mut r = store.get(env, id).ok_or_else(|| format!("'{env}' has no recommendation {id}"))?;
    r.state = state;
    r.state_note = line(note, 500, "note", false)?;
    r.state_at = now_secs();
    match state {
        State::Done if r.ledger.is_none() => {
            // What the ledger expects is what the latest costs say, as the user saw it.
            r.reprice(costs::cached(env).as_ref());
            if let Some(entry) = ledger_entry(&r) {
                ledger::put(entry.clone()).map_err(|e| format!("cannot record it in the savings ledger: {e}"))?;
                r.ledger = Some(entry.id);
            }
        }
        // Not made after all: it leaves the ledger too.
        State::Pending => {
            if let Some(id) = r.ledger.take() {
                ledger::remove(&id).map_err(|e| format!("cannot update the savings ledger: {e}"))?;
            }
            r.state_note.clear();
        }
        _ => {}
    }
    store.save(&r).map_err(|e| format!("cannot save {}: {e}", r.id))?;
    Ok(r)
}

/// The savings ledger entry of a recommendation made: only when it names the bill's usage, so the
/// bill can tell whether it saved.
pub fn ledger_entry(r: &Recommendation) -> Option<ledger::Entry> {
    let first = r.cost.changes.first()?;
    let mut usage: Vec<String> = Vec::new();
    for c in r.cost.changes.iter().filter(|c| c.service == first.service) {
        for u in std::iter::once(&c.usage_type).chain(&c.to) {
            if !usage.contains(u) {
                usage.push(u.clone());
            }
        }
    }
    let bill = r.cost.bill.as_ref().filter(|b| b.priced());
    let expected = bill.filter(|b| !b.at_most && b.monthly > 0.0 && r.cost.claude.is_none()).map(|b| b.monthly);
    let resource = r.resource().cloned().unwrap_or_default();
    Some(ledger::entry_for_usage(
        &r.env,
        &r.region,
        &format!("{}: {}", r.id, r.title),
        (&resource.module, &resource.target, &resource.name),
        &first.service,
        usage,
        expected,
        bill.map_or("", |b| b.currency.as_str()),
    ))
}

// ---- storage ------------------------------------------------------------------------------

/// Where recommendations are kept: `~/.deployment-cli/advisor` ([`Store::home`]), one folder
/// per environment (its name escaped as the costs cache's).
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn home() -> Option<Store> {
        Some(Store { root: crate::util::home_dir()?.join(".deployment-cli").join("advisor") })
    }

    /// A store in another folder (tests).
    pub fn at(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into() }
    }

    pub fn dir(&self, env: &str) -> PathBuf {
        self.root.join(costs::file_name(env))
    }

    fn path(&self, env: &str, id: &str) -> Option<PathBuf> {
        Some(self.dir(env).join(format!("R{}.json", number(id)?)))
    }

    /// Every recommendation of `env`, in order of number. A file that cannot be read (being
    /// written, damaged) is left out, and left alone.
    pub fn list(&self, env: &str) -> Vec<Recommendation> {
        let Ok(entries) = std::fs::read_dir(self.dir(env)) else { return Vec::new() };
        let mut out: Vec<Recommendation> = entries
            .flatten()
            .filter_map(|e| {
                // `R7.json` holds R7: a file under another name is not one.
                let n = e.file_name().to_string_lossy().strip_suffix(".json").and_then(number)?;
                let r: Recommendation = serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
                (r.env == env && number(&r.id) == Some(n)).then(|| cleaned(r))
            })
            .collect();
        out.sort_by_key(Recommendation::number);
        out
    }

    pub fn get(&self, env: &str, id: &str) -> Option<Recommendation> {
        let text = std::fs::read_to_string(self.path(env, id)?).ok()?;
        serde_json::from_str::<Recommendation>(&text).ok().filter(|r| r.env == env && number(&r.id) == number(id)).map(cleaned)
    }

    /// Replaces the file of `r` (which has its id).
    pub fn save(&self, r: &Recommendation) -> std::io::Result<()> {
        let path = self.path(&r.env, &r.id).ok_or_else(|| std::io::Error::other(format!("not a recommendation id: {}", r.id)))?;
        crate::platform::write_private(&path, (serde_json::to_string_pretty(r).unwrap_or_default() + "\n").as_bytes())
    }

    /// Keeps `r` as a new recommendation, giving it the next free number of its environment.
    pub fn create(&self, r: &mut Recommendation) -> std::io::Result<()> {
        use std::io::Write as _;
        let dir = self.dir(&r.env);
        std::fs::create_dir_all(&dir)?;
        let mut n = self.list(&r.env).last().map_or(0, Recommendation::number) + 1;
        for _ in 0..50 {
            r.id = format!("R{n}");
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(dir.join(format!("R{n}.json"))) {
                Ok(mut file) => {
                    file.write_all((serde_json::to_string_pretty(r).unwrap_or_default() + "\n").as_bytes())?;
                    return file.sync_all();
                }
                // Another process took this number a moment ago.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("no free recommendation number"))
    }

    /// Changes whenever a recommendation of `env` is added or rewritten: the UI polls it to show
    /// what Claude writes as it writes it.
    pub fn signature(&self, env: &str) -> u64 {
        let Ok(entries) = std::fs::read_dir(self.dir(env)) else { return 0 };
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |v: u64| {
            hash = (hash ^ v).wrapping_mul(0x0100_0000_01b3);
        };
        let mut files: Vec<(String, u64, u64)> = entries
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as u64;
                Some((e.file_name().to_string_lossy().into_owned(), modified, meta.len()))
            })
            .collect();
        files.sort();
        for (name, modified, len) in files {
            name.bytes().for_each(|b| mix(u64::from(b)));
            mix(modified);
            mix(len);
        }
        hash
    }
}

/// A recommendation as read from disk, where anyone could have edited it: its text without
/// escape sequences or hidden characters (as [`submit`] keeps it), and a command that spans
/// lines shown as a snippet instead, so copying a "command" never pastes several.
fn cleaned(mut r: Recommendation) -> Recommendation {
    let one_line = |t: &mut String| *t = sanitize(t).replace(['\n', '\t'], " ");
    let text = |t: &mut String| *t = sanitize(t);
    for t in [&mut r.title, &mut r.author, &mut r.region, &mut r.account, &mut r.performance, &mut r.state_note] {
        one_line(t);
    }
    for t in [&mut r.finding, &mut r.risk_note, &mut r.rollback, &mut r.verify] {
        text(t);
    }
    r.evidence.iter_mut().for_each(one_line);
    for x in &mut r.resources {
        [&mut x.module, &mut x.target, &mut x.name].into_iter().for_each(one_line);
    }
    for step in &mut r.steps {
        text(&mut step.text);
        if let Some(code) = &mut step.code {
            text(code);
        }
        if let Some(language) = &mut step.language {
            one_line(language);
        }
        if let Some(command) = step.command.take() {
            if command.contains(['\n', '\r']) || command.chars().any(crate::util::is_unsafe) {
                let shown = sanitize(&command);
                step.code = Some(match step.code.take() {
                    Some(code) => format!("{shown}\n{code}"),
                    None => shown,
                });
            } else {
                step.command = Some(command);
            }
        }
    }
    let c = &mut r.cost;
    for change in &mut c.changes {
        one_line(&mut change.service);
        one_line(&mut change.usage_type);
        if let Some(to) = &mut change.to {
            one_line(to);
        }
    }
    if let Some(b) = &mut c.bill {
        one_line(&mut b.currency);
        b.notes.iter_mut().for_each(one_line);
        b.unpriced.iter_mut().for_each(one_line);
        for l in &mut b.lines {
            for t in [&mut l.unit, &mut l.change.service, &mut l.change.usage_type] {
                one_line(t);
            }
            for t in [&mut l.change.to, &mut l.scaled_from, &mut l.to_scaled_from].into_iter().flatten() {
                one_line(t);
            }
        }
    }
    if let Some(claude) = &mut c.claude {
        text(&mut claude.reasoning);
    }
    if let Some(none) = &mut c.none {
        one_line(none);
    }
    if let Some(ledger) = &mut r.ledger {
        one_line(ledger);
    }
    r
}

/// `7` for `R7` (or `r7`); `None` for anything else.
pub fn number(id: &str) -> Option<u32> {
    let digits = id.strip_prefix(['R', 'r'])?;
    if digits.is_empty() || digits.len() > 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|n| *n > 0)
}

// ---- text ---------------------------------------------------------------------------------

/// Whether a command changes something (in AWS or elsewhere), as far as its words tell:
/// AWS CLI calls other than describe/get/list…, dcli commands that deploy or write, and any
/// other program (dcli cannot tell what it does).
pub fn writes(command: &str) -> bool {
    // Pipes, redirections, substitutions, globs, quotes: what runs is no longer plain words, so
    // it may change something.
    if command.chars().any(|c| "|;&<>$`(){}\\\"'*?!#~\n\r".contains(c) || crate::util::is_unsafe(c)) {
        return true;
    }
    let words: Vec<String> = command.split_whitespace().map(str::to_ascii_lowercase).collect();
    let program = words.first().map(|w| w.rsplit(['/', '\\']).next().unwrap_or(w).trim_end_matches(".exe").to_string());
    match program.as_deref() {
        Some("aws") => {
            // aws [--global options] <service> <operation> [options]; most options take a value.
            let flags = ["--debug", "--no-paginate", "--no-verify-ssl", "--no-sign-request", "--no-cli-pager", "--no-cli-auto-prompt"];
            let mut rest = words.iter().skip(1);
            let mut positional = Vec::new();
            while let Some(w) = rest.next() {
                if !w.starts_with('-') {
                    positional.push(w.as_str());
                    if positional.len() == 2 {
                        break;
                    }
                } else if w.starts_with("--endpoint-url") {
                    return true;
                } else if !w.contains('=') && !flags.contains(&w.as_str()) {
                    rest.next();
                }
            }
            let (Some(service), Some(op)) = (positional.first(), positional.get(1)) else { return true };
            let reading = op.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && (["describe-", "get-", "list-", "search-", "lookup-", "filter-", "batch-get-"].iter().any(|p| op.starts_with(p))
                    || ["help", "wait", "tail", "ls", "query", "scan"].contains(op));
            // Reads that stream what they read into a local file.
            let saves = [
                "get-object", "get-object-torrent", "get-job-output", "get-export", "get-sdk", "get-thing-shadow",
                "get-package-version-asset", "get-snapshot-block", "get-media", "get-media-for-fragment-list", "get-clip",
                "get-work-unit-results", "get-raw-message-content", "get-retained-message",
            ]
            .contains(op);
            if !reading || saves || *service == "s3api" && op.starts_with("get-object") {
                return true;
            }
            // After the operation, a value with no option before it is an output file (`ls` and
            // `tail` take a path or a log group instead), and so is `--outfile`. Options take a
            // value, but for those known to take none, so a value after one of those counts too.
            let boolean = |w: &str| {
                w.starts_with("--no-")
                    || ["--debug", "--dry-run", "--recursive", "--summarize", "--human-readable", "--follow", "--force", "--cli-auto-prompt", "--generate-cli-skeleton"]
                        .contains(&w)
            };
            let mut expecting = false;
            for w in rest {
                if w.starts_with("--outfile") || w.starts_with("--endpoint-url") {
                    return true;
                }
                if w.starts_with('-') {
                    expecting = w.starts_with("--") && !w.contains('=') && !boolean(w);
                } else if expecting {
                    expecting = false;
                } else if !["ls", "tail", "help"].contains(op) {
                    return true;
                }
            }
            false
        }
        Some("dcli") => {
            let writing = ["deploy", "approve", "stop", "promote", "rollback", "update", "add", "remove", "rm", "use", "login"];
            // Flags that only choose what to read; any other may act.
            let reading = [
                "--json", "--since", "--filter", "--follow", "-f", "--full", "--days", "--service", "--report", "--md", "--by-cost",
                "--ledger", "--limit", "-n", "--range", "--usage", "--daily", "--commitments", "--config", "--all", "--help", "-h",
            ];
            words.get(1).is_some_and(|w| writing.contains(&w.as_str()))
                || words.iter().skip(1).any(|w| w.starts_with('-') && !reading.contains(&w.split('=').next().unwrap_or(w)))
        }
        _ => true,
    }
}

/// A recommendation as Markdown: to copy into a ticket, or for `dcli advisor --md`. Claude's text
/// is kept as written; commands go in code blocks.
pub fn markdown(r: &Recommendation) -> String {
    let mut md = String::new();
    let _ = writeln!(md, "## {} · {}\n", r.id, r.title);
    let account = if r.account.is_empty() { String::new() } else { format!(", account {}", md_code(&r.account)) };
    let _ = writeln!(
        md,
        "{} · {} ({}{account}) · confidence {} · risk {} · effort {} · {}\n",
        r.category.label(),
        md_code(&r.env),
        md_code(&r.region),
        r.confidence.label(),
        r.risk.label(),
        r.effort.label(),
        r.state.label()
    );
    let _ = writeln!(md, "**Cost:** {}", cost_line(&r.cost));
    if let Some(b) = r.cost.bill.as_ref().filter(|b| b.priced()) {
        for l in b.rate_lines() {
            let _ = writeln!(md, "- {}", md_code(&l));
        }
        for n in &b.notes {
            let _ = writeln!(md, "- {n}");
        }
    }
    if let Some(c) = &r.cost.claude {
        let _ = writeln!(md, "- Claude's estimate: {}", c.reasoning);
    }
    md.push('\n');
    if !r.performance.is_empty() {
        let _ = writeln!(md, "**Performance:** {}\n", r.performance);
    }
    if !r.resources.is_empty() {
        let names: Vec<String> = r.resources.iter().map(|x| md_code(&x.name)).collect();
        let _ = writeln!(md, "**Resources:** {}\n", names.join(", "));
    }
    let _ = writeln!(md, "**Found:** {}\n", r.finding);
    if !r.evidence.is_empty() {
        md.push_str("**Evidence:**\n");
        for e in &r.evidence {
            let _ = writeln!(md, "- {e}");
        }
        md.push('\n');
    }
    md.push_str("**Steps:**\n\n");
    for (i, s) in r.steps.iter().enumerate() {
        let _ = writeln!(md, "{}. {}", i + 1, s.text);
        // Indented under the step, so the numbered list goes on after the block.
        let indent = " ".repeat(format!("{}. ", i + 1).len());
        let mut block = |lang: &str, text: &str| {
            let mut fenced = String::new();
            md_block(&mut fenced, lang, text);
            md.push('\n');
            for l in fenced.lines() {
                let _ = writeln!(md, "{}{l}", if l.is_empty() { "" } else { indent.as_str() });
            }
        };
        if let Some(c) = &s.command {
            block("sh", c);
        }
        if let Some(c) = &s.code {
            block(s.language.as_deref().unwrap_or("text"), c);
        }
    }
    md.push('\n');
    let risk = if r.risk_note.is_empty() { String::new() } else { format!(": {}", r.risk_note) };
    let _ = writeln!(md, "**Risk:** {}{risk}\n", r.risk.label());
    let _ = writeln!(md, "**Undo:** {}\n", r.rollback);
    let _ = writeln!(md, "**Verify:** {}\n", r.verify);
    if !r.state_note.is_empty() {
        let _ = writeln!(md, "**{}:** {}\n", capitalize(r.state.label()), r.state_note);
    }
    md
}

/// `saves ≈ US$ 412 a month (your bill)`.
pub fn cost_line(c: &Cost) -> String {
    match cost_figure(c) {
        Some(figure) => format!("{figure} ({})", c.source()),
        None => c.source(),
    }
}

/// `saves ≈ US$ 412 a month (US$ 90 now, …)`, `costs ≈ US$ 120 more a month`, or `None`
/// without a figure; [`Cost::source`] says where it comes from.
pub fn cost_figure(c: &Cost) -> Option<String> {
    let m = c.monthly()?;
    Some(match c.bill.as_ref().filter(|b| b.priced() && c.claude.is_none()) {
        Some(b) => b.describe(),
        None if m < 0.0 => format!("costs ≈ {} more a month", price::money(-m, c.currency())),
        None => format!("saves ≈ {} a month", price::money(m, c.currency())),
    })
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use estimate::{Change, Effect};

    fn draft() -> Draft {
        Draft {
            category: Category::Cost,
            title: "Lower orders-api's memory\nto 1024 MB".into(),
            resources: vec![ResourceRef { module: "Lambda".into(), target: "orders-api".into(), name: "orders-api".into() }],
            finding: "Uses 410 of 3008 MB".into(),
            evidence: vec!["max memory used 410 MB over 14 days".into()],
            steps: vec![Step {
                text: "Set the memory".into(),
                command: Some("aws lambda update-function-configuration --function-name orders-api --memory-size 1024".into()),
                ..Default::default()
            }],
            rollback: "Set it back to 3008 MB".into(),
            verify: "Duration p95 stays under 800 ms".into(),
            cost: CostDraft {
                changes: vec![Change {
                    service: "AWS Lambda".into(),
                    usage_type: "Lambda-GB-Second".into(),
                    effect: Effect::Remove,
                    quantity: 3_000_000.0,
                    to: None,
                    at_most: false,
                }],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn drafts_are_cleaned_and_need_a_cost() {
        let d = draft().checked().unwrap();
        assert_eq!(d.title, "Lower orders-api's memory to 1024 MB");
        let mut no_cost = draft();
        no_cost.cost = CostDraft::default();
        assert!(no_cost.checked().unwrap_err().contains("needs a cost estimate"));
        no_cost.cost.no_cost_change = Some("same instance hours".into());
        assert!(no_cost.checked().unwrap_err().contains("needs a figure"), "a cost recommendation");
        no_cost.category = Category::Performance;
        assert!(no_cost.checked().is_ok());
        let mut two_lines = draft();
        two_lines.steps[0].command = Some("aws s3 ls\nrm -rf /".into());
        assert!(two_lines.checked().unwrap_err().contains("one line"));
        let mut hidden = draft();
        hidden.steps[0].command = Some("aws s3 ls \u{202e}x".into());
        assert!(hidden.checked().unwrap_err().contains("invisible"));
        let mut escape = draft();
        escape.finding = "red \u{1b}[31m text".into();
        assert_eq!(escape.checked().unwrap().finding, "red  text");
    }

    #[test]
    fn ids_and_commands() {
        assert_eq!(number("R7"), Some(7));
        assert_eq!(number("r12"), Some(12));
        for bad in ["R", "R0", "7", "R-1", "R1/..", "R1234567"] {
            assert_eq!(number(bad), None, "{bad}");
        }
        assert!(!writes("aws lambda get-function-configuration --function-name x --profile prod"));
        assert!(!writes("aws --profile prod cloudwatch get-metric-statistics --namespace AWS/Lambda"));
        assert!(writes("aws lambda update-function-configuration --function-name x --memory-size 1024"));
        assert!(writes("aws logs describe-log-groups | xargs -n1 aws logs delete-log-group"));
        assert!(!writes("aws s3 ls --region us-east-1"));
        assert!(!writes("dcli lambda prod orders-api"));
        assert!(!writes("dcli optimize prod --by-cost --json"));
        assert!(writes("dcli dms prod orders --resume"));
        assert!(writes("dcli deploy prod orders"));
        assert!(writes("dcli param prod /x --set 1"));
        assert!(writes("terraform apply"));
        assert!(writes("aws lambda get-function --function-name x;rm -rf ~"));
        assert!(writes("aws lambda get-function --function-name $(curl evil)"));
        assert!(writes("aws lambda get-function --function-name x&&reboot"));
        assert!(writes("aws lambda get-function.x --function-name x"));
        assert!(writes("aws s3api get-object --bucket b --key k out.txt"));
        assert!(writes("aws apigateway get-export --rest-api-id a --stage-name p --export-type oas30 api.json"));
        assert!(writes("aws lambda get-function --function-name x --outfile y"));
        assert!(writes("aws --endpoint-url http://evil lambda get-function --function-name x"));
        assert!(writes("aws lambda get-function --function-name x extra"));
        assert!(writes("aws"));
        assert!(writes("aws s3api get-object --bucket b --key k --no-paginate out.txt"));
        assert!(writes("aws glacier get-job-output --account-id - --vault-name v --job-id j out"));
        assert!(writes("aws lambda get-function --function-name x --debug out"));
        assert!(!writes("aws s3 ls s3://bucket/prefix/ --recursive --summarize"));
        assert!(!writes("aws logs tail /aws/lambda/orders --since 1h"));
        assert!(!writes("aws dynamodb scan --table-name orders --max-items 5"));
    }

    fn temp_store(name: &str) -> Store {
        let root = std::env::temp_dir().join(format!("dcli-advisor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Store::at(root)
    }

    #[test]
    fn submissions_are_kept_numbered_and_respect_the_user() {
        let store = temp_store("submit");
        let origin = Origin { env: "prod.eu", region: "us-east-1", account: "123456789012", author: "test" };
        let costs = Snapshot {
            read_at: 1_790_000_000.0,
            usage: vec![crate::costs::UsageCost {
                service: "AWS Lambda".into(),
                usage_type: "Lambda-GB-Second".into(),
                cost: 800.0,
                quantity: 48_000_000.0,
                unit: "Lambda-GB-Second".into(),
            }],
            ..Default::default()
        };
        let r = submit(&store, &draft(), &origin, Some(&costs)).unwrap();
        assert_eq!(r.id, "R1");
        assert!((r.cost.monthly().unwrap() - 50.0).abs() < 1e-9);
        assert!(store.dir("prod.eu").ends_with("prod%2Eeu"));
        // Not priced without the costs, unless Claude gives its own figure.
        assert!(submit(&store, &draft(), &origin, None).unwrap_err().contains("dcli costs prod.eu --refresh"));
        // The same thing twice is refused; with its id it is updated.
        assert!(submit(&store, &draft(), &origin, Some(&costs)).unwrap_err().contains("R1 already recommends this"));
        let mut again = draft();
        again.id = Some("r1".into());
        again.finding = "Uses 400 of 3008 MB".into();
        assert_eq!(submit(&store, &again, &origin, Some(&costs)).unwrap().finding, "Uses 400 of 3008 MB");
        let mut other = draft();
        other.title = "Move orders-api to arm64".into();
        assert_eq!(submit(&store, &other, &origin, Some(&costs)).unwrap().id, "R2");
        assert_eq!(store.list("prod.eu").iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["R1", "R2"]);
        // The user's decision stands.
        decide(&store, "prod.eu", "R1", State::Dismissed, "we need the headroom").unwrap();
        assert!(submit(&store, &again, &origin, Some(&costs)).unwrap_err().contains("we need the headroom"));
        assert!(withdraw(&store, "prod.eu", "R1", "x").unwrap_err().contains("dismissed already"));
        assert_eq!(withdraw(&store, "prod.eu", "R2", "replaced").unwrap().state, State::Withdrawn);
        // A file under another name is not read.
        std::fs::copy(store.dir("prod.eu").join("R2.json"), store.dir("prod.eu").join("R9.json")).unwrap();
        assert_eq!(store.list("prod.eu").len(), 2);
        let _ = std::fs::remove_dir_all(store.dir("prod.eu"));
    }

    #[test]
    fn markdown_keeps_commands_inside_their_step() {
        let d = draft().checked().unwrap();
        let mut steps = d.steps.clone();
        steps.push(Step { text: "Check".into(), code: Some("a: 1\n\nb: 2".into()), language: Some("yaml".into()), ..Default::default() });
        let r = Recommendation { id: "R1".into(), env: "dev".into(), title: d.title.clone(), steps, ..Default::default() };
        let md = markdown(&r);
        assert!(md.contains("1. Set the memory\n\n   ```sh\n   aws lambda update-function-configuration"), "{md}");
        assert!(md.contains("2. Check\n\n   ```yaml\n   a: 1\n\n   b: 2\n   ```"), "{md}");
    }

    #[test]
    fn files_edited_by_hand_are_cleaned_when_read() {
        let store = temp_store("cleaned");
        let mut r = Recommendation { id: "R1".into(), env: "dev".into(), title: "x\u{1b}]52;c;AAAA\u{7}y".into(), ..Default::default() };
        r.steps = vec![Step { text: "go".into(), command: Some("aws s3 ls\nrm -rf ~".into()), ..Default::default() }];
        r.cost.changes = vec![estimate::Change {
            service: "AWS\u{1b}[2J Lambda".into(),
            usage_type: "x".into(),
            effect: Effect::Remove,
            quantity: 1.0,
            to: None,
            at_most: false,
        }];
        std::fs::create_dir_all(store.dir("dev")).unwrap();
        std::fs::write(store.dir("dev").join("R1.json"), serde_json::to_string(&r).unwrap()).unwrap();
        let read = store.get("dev", "R1").unwrap();
        assert_eq!(read.title, "xy");
        assert_eq!(read.steps[0].command, None, "a command over two lines is not a command");
        assert_eq!(read.steps[0].code.as_deref(), Some("aws s3 ls\nrm -rf ~"));
        assert_eq!(read.cost.changes[0].service, "AWS Lambda");
        assert_eq!(store.list("dev")[0], read);
        let _ = std::fs::remove_dir_all(store.dir("dev"));
    }

    #[test]
    fn costs_read_well() {
        let mut c = Cost { claude: Some(ClaudeEstimate { monthly: -120.0, reasoning: "2 more ACUs".into() }), ..Default::default() };
        assert_eq!(c.short(), "+US$ 120/mo");
        assert_eq!(cost_line(&c), "costs ≈ US$ 120 more a month (Claude's estimate)");
        c.claude = None;
        c.none = Some("same instance hours".into());
        assert_eq!(c.short(), "—");
        assert_eq!(cost_line(&c), "no cost change: same instance hours");
    }
}

//! Changes made to save money (applied by dcli, or marked done by the user) and whether the
//! bill shows the saving: the cost of the usage types they touch before and after, from Cost
//! Explorer. Kept in `~/.deployment-cli/savings-ledger.json`.
//!
//! A usage type mixes resources (every function's GB-seconds, every log group's ingestion),
//! so other changes and traffic move it too: a verification says what the bill did around
//! the change, not what the change alone did.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::explorer;
use crate::aws::{self, Aws, AwsError, rds};
use crate::savings::{self, Finding, billing};
use crate::util::now_secs;

/// Days of costs compared on each side of a change, at most.
pub const WINDOW_DAYS: u32 = 14;
/// Full days after a change before its verification means something.
pub const READY_DAYS: u32 = 7;
const DAY: f64 = 86_400.0;
/// Days in an average month, to turn a daily difference into a monthly one.
const MONTH_DAYS: f64 = 30.4375;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// dcli applied a finding's fix (Savings module).
    Applied,
    /// The user marked a finding done (changed elsewhere: code, the console).
    #[default]
    Done,
    /// A cost-relevant action of another module (stopping a cluster, a smaller class…).
    Action,
}

/// The daily cost of the change's usage types before and after it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Verification {
    /// When it was read (Unix seconds).
    pub at: f64,
    /// Average cost a day before and after.
    pub before: f64,
    pub after: f64,
    pub days_before: u32,
    pub days_after: u32,
    /// The account whose bill was read.
    pub account: String,
    /// What the comparison cannot tell (usage that commitments cover, say).
    pub notes: Vec<String>,
}

impl Verification {
    /// What the difference comes to in a month (positive: it costs less).
    pub fn saved_monthly(&self) -> f64 {
        (self.before - self.after) * MONTH_DAYS
    }

    /// `≈ US$ 38 less a month`, `≈ US$ 12 more a month`.
    pub fn difference(&self, currency: &str) -> String {
        let saved = self.saved_monthly();
        if saved >= 0.0 {
            format!("≈ {} less a month", savings::price::money(saved, currency))
        } else {
            format!("≈ {} more a month", savings::price::money(-saved, currency))
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Entry {
    pub id: String,
    pub env: String,
    /// The AWS account, from the costs read for the environment (empty when none were).
    pub account: String,
    pub region: String,
    /// When the change was made (Unix seconds).
    pub at: f64,
    pub source: Source,
    /// What was changed: "Log retention on /ecs/orders-api", "orders-2 to db.r6g.large".
    pub what: String,
    /// The module and target that open the resource.
    pub module: String,
    pub target: String,
    pub resource: String,
    /// Cost Explorer's name of the service.
    pub service: String,
    /// The usage types (without region prefix) whose cost should drop.
    pub usage: Vec<String>,
    /// What the finding said it would save a month, when it was priced.
    pub expected: Option<f64>,
    pub currency: String,
    pub verified: Option<Verification>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    /// Not enough days since the change: ready then (Unix seconds).
    Waiting(f64),
    /// Can be verified now.
    Ready,
    /// Costs less, by about what was expected (or something, when nothing was).
    AsExpected,
    /// Costs less, but clearly less than expected.
    Less,
    /// Costs about the same.
    NoChange,
    /// Costs more than before.
    Up,
}

impl Status {
    pub fn label(self) -> String {
        match self {
            Status::Waiting(ready) => format!("waiting for data until {}", day_text(ready)),
            Status::Ready => "ready to verify".into(),
            Status::AsExpected => "saving as expected".into(),
            Status::Less => "saving less than expected".into(),
            Status::NoChange => "no change on the bill".into(),
            Status::Up => "the cost went up".into(),
        }
    }
}

fn day_start(t: f64) -> f64 {
    (t / DAY).floor() * DAY
}

/// `2026-10-22`
fn day_text(t: f64) -> String {
    let (y, m, d, ..) = crate::util::utc_parts(t as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// What an entry's verification says.
pub fn status(e: &Entry, now: f64) -> Status {
    // READY_DAYS whole days after the change's own, yesterday (not complete on the bill yet)
    // left out.
    let ready = day_start(e.at) + f64::from(READY_DAYS + 2) * DAY;
    let Some(v) = &e.verified else {
        return if now < ready { Status::Waiting(ready) } else { Status::Ready };
    };
    let saved = v.saved_monthly();
    // Noise: a twentieth of what it cost before, and at least a cent a day.
    let noise = (v.before * MONTH_DAYS * 0.05).max(0.01 * MONTH_DAYS);
    match e.expected.filter(|x| *x > 0.0) {
        _ if saved < -noise => Status::Up,
        _ if saved.abs() <= noise => Status::NoChange,
        Some(expected) if saved < expected * 0.8 => Status::Less,
        _ => Status::AsExpected,
    }
}

pub fn path() -> Option<PathBuf> {
    Some(crate::util::home_dir()?.join(".deployment-cli").join("savings-ledger.json"))
}

/// Every entry, oldest first.
pub fn load() -> Vec<Entry> {
    path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Every entry; an error when the file exists but cannot be read or is not valid, so that
/// it is never replaced by what little dcli would write over it.
fn load_checked() -> std::io::Result<Vec<Entry>> {
    let path = path().ok_or_else(|| std::io::Error::other("cannot find the home directory"))?;
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| std::io::Error::other(format!("{} is not valid ({e}): fix or delete it", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(std::io::Error::other(format!("cannot read {}: {e}", path.display()))),
    }
}

/// One change to the ledger at a time (the UI and background actions both record).
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Reads the entries, changes them with `change` and writes them back.
fn update(change: impl FnOnce(&mut Vec<Entry>)) -> std::io::Result<()> {
    let _one = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = load_checked()?;
    change(&mut all);
    let path = path().ok_or_else(|| std::io::Error::other("cannot find the home directory"))?;
    crate::platform::write_private(&path, (serde_json::to_string_pretty(&all).unwrap_or_default() + "\n").as_bytes())
}

/// The entries of `env`, newest first.
pub fn of_env(env: &str) -> Vec<Entry> {
    let mut list: Vec<Entry> = load().into_iter().filter(|e| e.env == env).collect();
    list.sort_by(|a, b| b.at.total_cmp(&a.at));
    list
}

/// Keeps `entry` (a new one, or the new state of one with its id).
pub fn put(entry: Entry) -> std::io::Result<()> {
    update(|all| match all.iter_mut().find(|e| e.id == entry.id) {
        Some(slot) => *slot = entry,
        None => all.push(entry),
    })
}

pub fn remove(id: &str) -> std::io::Result<()> {
    update(|all| all.retain(|e| e.id != id))
}

/// Notes `v` on `e`, and the account it was read for when `e` did not know it yet.
pub fn verified(e: &mut Entry, v: Verification) {
    if !crate::aws::sts::is_account_id(&e.account) && crate::aws::sts::is_account_id(&v.account) {
        e.account = v.account.clone();
    }
    e.verified = Some(v);
}

fn new_id(at: f64, what: &str) -> String {
    let mut hash: u32 = 2_166_136_261;
    for b in what.bytes() {
        hash = (hash ^ u32::from(b)).wrapping_mul(16_777_619);
    }
    format!("{:x}-{hash:08x}", at as u64)
}

/// An entry for `f`, changed now in `env` (whose account comes from its costs, if read).
pub fn entry_for(f: &Finding, env: &str, region: &str, source: Source, what: impl Into<String>) -> Entry {
    let at = now_secs();
    let what = what.into();
    let mut usage: Vec<String> = Vec::new();
    for b in &f.billed {
        for u in std::iter::once(&b.usage).chain(&b.instead) {
            if !usage.contains(u) {
                usage.push(u.clone());
            }
        }
    }
    let costs = super::cached(env);
    Entry {
        id: new_id(at, &format!("{env}{what}{}", f.target)),
        env: env.into(),
        account: costs.as_ref().map(|c| c.account.clone()).unwrap_or_default(),
        region: region.into(),
        at,
        source,
        what,
        module: f.module.into(),
        target: f.target.clone(),
        resource: f.resource.clone(),
        service: f.billed_service().unwrap_or_default().into(),
        usage,
        expected: f.price.as_ref().filter(|p| !p.at_most).map(|p| p.monthly),
        currency: f.price.as_ref().map(|p| p.currency.clone()).or_else(|| costs.map(|c| c.currency)).unwrap_or_default(),
        verified: None,
    }
}

/// An entry for a change the user made elsewhere, named by the usage types it touches (a
/// recommendation of the Advisor marked done): `service` is Cost Explorer's name.
#[allow(clippy::too_many_arguments)]
pub fn entry_for_usage(
    env: &str,
    region: &str,
    what: &str,
    (module, target, resource): (&str, &str, &str),
    service: &str,
    usage: Vec<String>,
    expected: Option<f64>,
    currency: &str,
) -> Entry {
    let at = now_secs();
    let costs = super::cached(env);
    Entry {
        id: new_id(at, &format!("{env}{what}{target}")),
        env: env.into(),
        account: costs.as_ref().map(|c| c.account.clone()).unwrap_or_default(),
        region: region.into(),
        at,
        source: Source::Done,
        what: what.into(),
        module: module.into(),
        target: target.into(),
        resource: resource.into(),
        service: service.into(),
        usage,
        expected,
        currency: if currency.is_empty() { costs.map(|c| c.currency).unwrap_or_default() } else { currency.into() },
        verified: None,
    }
}

/// A cost-relevant action another module makes (stopping a cluster, a smaller class…), to
/// record once it is made and verify on the bill later.
#[derive(Debug, Clone)]
pub struct Note {
    env: String,
    region: String,
    /// The module that made it, which is also the Savings check of its service.
    module: &'static str,
    resource: String,
    usage: Vec<String>,
    what: String,
}

impl Note {
    pub fn new(env: &str, region: &str, module: &'static str, resource: &str, usage: Vec<String>, what: String) -> Note {
        Note { env: env.into(), region: region.into(), module, resource: resource.into(), usage, what }
    }

    /// Keeps it in the ledger. Best effort: a ledger that cannot be written does not fail
    /// the action.
    pub fn record(&self) {
        let f = Finding { service: self.module, ..Finding::new(self.module, &self.resource, &self.resource, "", "", "", savings::Impact::Low) };
        let mut entry = entry_for(&f, &self.env, &self.region, Source::Action, self.what.clone());
        entry.usage = self.usage.clone();
        let _ = put(entry);
    }
}

/// The usage types of an instance class, as the bill names them.
pub fn usage_of_instance(module: &str, class: &str) -> Vec<String> {
    match module {
        "Aurora" if class == "db.serverless" => vec![billing::AURORA_SERVERLESS.to_string()],
        "Aurora" => vec![billing::rds_instance(class)],
        "Redshift" => vec![billing::redshift_node(class)],
        "DMS" => vec![billing::dms_instance(class), billing::dms_standby(class)],
        _ => Vec::new(),
    }
}

/// Records an Aurora action that changes the bill (stopping a cluster, another class, another
/// Serverless v2 range), made in `env` with `inv` as the clusters were.
pub fn note_aurora(env: &str, region: &str, inv: &rds::Inventory, action: &rds::Action) {
    let classes = |cluster: &str| -> Vec<String> {
        let mut usage: Vec<String> = Vec::new();
        for i in inv.instances_of(cluster) {
            for u in usage_of_instance("Aurora", &i.class) {
                if !usage.contains(&u) {
                    usage.push(u);
                }
            }
        }
        usage
    };
    let note = match action {
        rds::Action::Stop { cluster } => Note::new(env, region, "Aurora", cluster, classes(cluster), format!("Stopped {cluster}")),
        // A class change left for the maintenance window is not followed: the bill around
        // today would only show the old class.
        rds::Action::SetClass { now: false, .. } => return,
        rds::Action::SetClass { instance, class, .. } => {
            let Some(i) = inv.instances.iter().find(|i| i.id == *instance) else { return };
            let usage = usage_of_instance("Aurora", &i.class).into_iter().chain(usage_of_instance("Aurora", class)).collect();
            Note::new(env, region, "Aurora", &i.cluster, usage, format!("{instance} from {} to {class}", i.class))
        }
        rds::Action::SetCapacity { cluster, min, max } => Note::new(
            env,
            region,
            "Aurora",
            cluster,
            vec![billing::AURORA_SERVERLESS.to_string()],
            format!("{cluster} scales in {}–{} ACU", rds::fmt_acu(*min), rds::fmt_acu(*max)),
        ),
        _ => return,
    };
    note.record();
}

/// The days compared for a change at `at`, read `now`: (start, end) of the days before and
/// after, whole UTC days; the change's own day, yesterday (not complete on the bill yet) and
/// today left out.
pub fn windows(at: f64, now: f64) -> ((f64, f64), (f64, f64)) {
    let day = day_start(at);
    let before = (day - f64::from(WINDOW_DAYS) * DAY, day);
    let after_end = (day + f64::from(WINDOW_DAYS + 1) * DAY).min(day_start(now) - DAY);
    ((before.0, before.1), (day + DAY, after_end.max(day + DAY)))
}

/// The Cost Explorer requests a verification makes: one, and up to [`VERIFY_PAGES`] when the
/// answer comes in pages.
pub const VERIFY_PAGES: usize = 3;

/// The account `e`'s bill is read for: its own, else the one its environment's costs were
/// read for, else the one the environment's profile signs in to (STS, free).
fn account_for(env_aws: &Aws, e: &Entry) -> aws::Result<String> {
    let valid = |a: &str| crate::aws::sts::is_account_id(a);
    if valid(&e.account) {
        return Ok(e.account.clone());
    }
    if let Some(s) = super::cached(&e.env).filter(|s| valid(&s.account)) {
        return Ok(s.account);
    }
    crate::aws::sts::caller_account(env_aws).and_then(|a| {
        if valid(&a) { Ok(a) } else { Err(AwsError::Config("the account of this change is not known: read the costs first".into())) }
    })
}

/// Reads the daily cost of the entry's usage types around the change, in its account and
/// region: ONE Cost Explorer request (up to [`VERIFY_PAGES`] when it answers in pages), each
/// billed by AWS.
pub fn verify(env_aws: &Aws, e: &Entry, now: f64) -> aws::Result<Verification> {
    if e.service.is_empty() || e.usage.is_empty() {
        return Err(AwsError::Config("this change names no usage type to look for on the bill".into()));
    }
    // Unreadable settings would quietly read with another profile: stop instead.
    super::settings::check().map_err(|err| AwsError::Config(err.to_string()))?;
    let ((b0, b1), (a0, a1)) = windows(e.at, now);
    if a1 <= a0 {
        return Err(AwsError::Config(format!("no complete day on the bill since the change yet: verify after {}", day_text(a0 + 2.0 * DAY))));
    }
    let account = account_for(env_aws, e)?;
    let reader = explorer::billing(env_aws);
    let service = json!({ "Dimensions": { "Key": "SERVICE", "Values": [e.service] } });
    let filter = match explorer::account_filter(&account) {
        Some(a) => json!({ "And": [service, a] }),
        None => service,
    };
    let mut input = json!({
        "TimePeriod": { "Start": day_text(b0), "End": day_text(a1) },
        "Granularity": "DAILY",
        "Metrics": ["AmortizedCost"],
        "Filter": filter,
        "GroupBy": [{ "Type": "DIMENSION", "Key": "USAGE_TYPE" }],
    });
    let mut days: std::collections::BTreeMap<i64, f64> = Default::default();
    for _ in 0..VERIFY_PAGES {
        let out: Value = explorer::call(&reader, "GetCostAndUsage", &input)?;
        // A day split over two pages adds up.
        for (day, cost) in daily_costs(&out, &e.usage, &e.region) {
            *days.entry(day as i64).or_default() += cost;
        }
        match out.get("NextPageToken").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            Some(token) => input["NextPageToken"] = json!(token),
            None => break,
        }
    }
    let days: Vec<(f64, f64)> = days.into_iter().map(|(d, c)| (d as f64, c)).collect();
    let mut v = compare(&days, (b0, b1), (a0, a1), now);
    v.account = account;
    let covered = super::cached(&e.env).map_or(0.0, |s| s.covered(&e.service));
    if covered > 0.0 && e.usage.iter().any(|u| crate::savings::price::coverable(u)) {
        v.notes.push(format!(
            "Savings Plans or reservations cover {} of this service: usage they no longer cover can still be paid for in \
             their fee until they renew, so the bill may show less of a saving than this compares.",
            crate::savings::percent(covered)
        ));
    }
    Ok(v)
}

/// (day start, cost of `usage`'s usage types that day in `region`) from a GetCostAndUsage
/// answer grouped by usage type.
fn daily_costs(out: &Value, usage: &[String], region: &str) -> Vec<(f64, f64)> {
    let mut days = Vec::new();
    for r in out.get("ResultsByTime").and_then(Value::as_array).into_iter().flatten() {
        let Some(start) = r.pointer("/TimePeriod/Start").and_then(Value::as_str).and_then(|d| crate::util::parse_rfc3339(&format!("{d}T00:00:00Z")))
        else {
            continue;
        };
        let cost: f64 = r
            .get("Groups")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|g| {
                g.pointer("/Keys/0").and_then(Value::as_str).is_some_and(|k| {
                    usage.iter().any(|u| billing::split_region(k).1 == u) && (region.is_empty() || billing::in_region(k, region))
                })
            })
            .filter_map(|g| g.pointer("/Metrics/AmortizedCost/Amount").and_then(Value::as_str).and_then(|a| a.parse::<f64>().ok()))
            .filter(|c| c.is_finite())
            .sum();
        days.push((start, cost));
    }
    days
}

/// Average daily cost on each side.
fn compare(days: &[(f64, f64)], before: (f64, f64), after: (f64, f64), now: f64) -> Verification {
    let side = |(from, to): (f64, f64)| {
        let picked: Vec<f64> = days.iter().filter(|(t, _)| *t >= from && *t < to).map(|(_, c)| *c).collect();
        let n = picked.len() as u32;
        (if n == 0 { 0.0 } else { picked.iter().sum::<f64>() / f64::from(n) }, n)
    };
    let ((b, nb), (a, na)) = (side(before), side(after));
    Verification { at: now, before: b, after: a, days_before: nb, days_after: na, ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: f64 = 1_790_000_000.0; // 2026-09-21 14:13 UTC

    fn entry(expected: Option<f64>) -> Entry {
        Entry { at: AT, expected, service: "AWS Lambda".into(), usage: vec!["Lambda-GB-Second".into()], ..Default::default() }
    }

    #[test]
    fn windows_leave_out_the_change_day_yesterday_and_today() {
        let ((b0, b1), (a0, a1)) = windows(AT, AT + 10.0 * DAY);
        assert_eq!(day_text(b0), "2026-09-07");
        assert_eq!(day_text(b1), "2026-09-21");
        assert_eq!(day_text(a0), "2026-09-22");
        assert_eq!(day_text(a1), "2026-09-30", "today (10-01) and yesterday, still being billed, are left out");
        let (_, (_, a1)) = windows(AT, AT + 40.0 * DAY);
        assert_eq!(day_text(a1), "2026-10-06", "at most 14 days after");
        let (_, (a0, a1)) = windows(AT, AT + DAY);
        assert!(a1 <= a0, "nothing complete the day after");
    }

    #[test]
    fn reads_daily_costs_of_the_usage_types_in_the_region() {
        let out = json!({ "ResultsByTime": [
            { "TimePeriod": { "Start": "2026-09-20" }, "Groups": [
                { "Keys": ["Lambda-GB-Second"], "Metrics": { "AmortizedCost": { "Amount": "10.5" } } },
                { "Keys": ["USW2-Lambda-GB-Second"], "Metrics": { "AmortizedCost": { "Amount": "1.5" } } },
                { "Keys": ["Request"], "Metrics": { "AmortizedCost": { "Amount": "99" } } } ] },
            { "TimePeriod": { "Start": "2026-09-22" }, "Groups": [] } ] });
        let days = daily_costs(&out, &["Lambda-GB-Second".into()], "us-east-1");
        assert_eq!(days.len(), 2);
        assert!((days[0].1 - 10.5).abs() < 1e-9, "us-west-2's line is another region");
        assert_eq!(days[1].1, 0.0);
        assert!((daily_costs(&out, &["Lambda-GB-Second".into()], "us-west-2")[0].1 - 1.5).abs() < 1e-9);
        assert!((daily_costs(&out, &["Lambda-GB-Second".into()], "")[0].1 - 12.0).abs() < 1e-9, "no region: all of them");
    }

    #[test]
    fn verifications_keep_the_account_they_read() {
        let mut e = entry(None);
        verified(&mut e, Verification { account: "123456789012".into(), ..Default::default() });
        assert_eq!(e.account, "123456789012");
        verified(&mut e, Verification { account: "210987654321".into(), ..Default::default() });
        assert_eq!(e.account, "123456789012", "an account already known stays");
    }

    #[test]
    fn verifications_compare_with_what_was_expected() {
        let before: Vec<(f64, f64)> = (1..=14).map(|d| (day_start(AT) - f64::from(d) * DAY, 10.0)).collect();
        let after: Vec<(f64, f64)> = (1..=8).map(|d| (day_start(AT) + f64::from(d) * DAY, 6.0)).collect();
        let days: Vec<(f64, f64)> = before.into_iter().chain(after).collect();
        let now = AT + 9.5 * DAY;
        let (b, a) = windows(AT, now);
        let v = compare(&days, b, a, now);
        assert_eq!((v.days_before, v.days_after), (14, 8));
        assert!((v.saved_monthly() - 4.0 * MONTH_DAYS).abs() < 1e-9);
        let verified = |expected| Entry { verified: Some(v.clone()), ..entry(expected) };
        assert_eq!(status(&verified(Some(130.0)), now), Status::AsExpected);
        assert_eq!(status(&verified(Some(400.0)), now), Status::Less);
        assert_eq!(status(&verified(None), now), Status::AsExpected);
        let up = Entry { verified: Some(Verification { before: 10.0, after: 13.0, ..v.clone() }), ..entry(Some(50.0)) };
        assert_eq!(status(&up, now), Status::Up);
        let flat = Entry { verified: Some(Verification { before: 10.0, after: 9.9, ..v }), ..entry(Some(50.0)) };
        assert_eq!(status(&flat, now), Status::NoChange);
        assert!(matches!(status(&entry(None), AT + 3.0 * DAY), Status::Waiting(_)));
        assert_eq!(status(&entry(None), AT + 9.0 * DAY), Status::Ready);
    }

    #[test]
    fn entries_name_every_usage_type_a_finding_moves() {
        let f = Finding { service: "Lambda", ..Finding::new("Lambda", "f", "f", "s", "r", "x", savings::Impact::High) }
            .billed_instead(billing::LAMBDA_GBS, billing::LAMBDA_GBS_ARM, 1.0);
        let e = entry_for(&f, "nowhere-env-for-tests", "us-east-1", Source::Done, "arm64");
        assert_eq!(e.usage, ["Lambda-GB-Second", "Lambda-GB-Second-ARM"]);
        assert_eq!(e.service, "AWS Lambda");
        assert_eq!(e.source, Source::Done);
        assert!(e.expected.is_none());
    }
}

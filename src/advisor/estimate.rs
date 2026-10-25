//! What a recommendation saves or costs, from the account's own bill. Whoever writes the
//! recommendation (Claude, through `dcli mcp`) describes the change as usage the bill knows:
//! so much less of a usage type a month, so much more, or so much billed as another usage
//! type instead (a smaller instance class, arm64). dcli prices it with the account's
//! effective rates, the costs read by the Costs module ([`Snapshot`]): it keeps no prices.
//!
//! A size of an instance family the bill has not used (db.r6g.2xlarge when only
//! db.r6g.xlarge ran) is priced from a size it has, in proportion to their sizes: AWS prices
//! the sizes of a family that way (the normalization factors of reservations).

use serde::{Deserialize, Serialize};

use crate::costs::{self, Snapshot};
use crate::savings::{billing, percent, price};

/// Changes one estimate takes, at most.
pub const MAX_CHANGES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    /// Less of the usage type a month.
    Remove,
    /// More of it.
    Add,
    /// The same quantity billed as another usage type instead (`to`).
    Move,
}

/// A change of usage, as the bill would see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    /// Cost Explorer's name of the service: `AWS Lambda`, `Amazon Relational Database Service`.
    pub service: String,
    /// The usage type, without its region prefix: `Lambda-GB-Second`, `InstanceUsage:db.r6g.large`.
    pub usage_type: String,
    pub effect: Effect,
    /// How much a month, in the unit the bill uses for the usage type.
    pub quantity: f64,
    /// For a move: the usage type the quantity is billed as instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The change removes part of `quantity`: the figure is the most it saves.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub at_most: bool,
}

/// One change priced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    #[serde(flatten)]
    pub change: Change,
    /// What it saves a month; negative when it costs more.
    pub monthly: f64,
    /// The part of `monthly` the bill shows right away (the rest is usage commitments cover).
    pub now: f64,
    /// The bill's rate for the usage type, and its unit.
    pub rate: f64,
    pub unit: String,
    /// For a move: the rate of the usage type it moves to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_rate: Option<f64>,
    /// A rate the bill did not have, scaled from this usage type (another size of the family).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaled_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_scaled_from: Option<String>,
}

/// What a set of changes saves (or costs) a month at the bill's rates.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Estimate {
    /// A month: positive saves, negative costs more.
    pub monthly: f64,
    /// What shows on the bill as soon as the change is made.
    pub now: f64,
    /// What commitments cover: saved when they renew (or freed for other usage).
    pub later: f64,
    /// When the first commitment covering the services ends (Unix seconds).
    pub renewal: Option<f64>,
    /// Some change removes part of its usage: the figure is the most it saves.
    pub at_most: bool,
    pub currency: String,
    pub lines: Vec<Line>,
    /// Usage types the bill has no rate for: those changes are not priced.
    pub unpriced: Vec<String>,
    /// What the figure assumes.
    pub notes: Vec<String>,
    /// When the costs it used were read (Unix seconds).
    pub priced_at: f64,
}

impl Estimate {
    /// Whether any change was priced.
    pub fn priced(&self) -> bool {
        !self.lines.is_empty()
    }

    /// `saves ≈ US$ 412 a month`, `costs ≈ US$ 120 more a month`, with the part saved later.
    pub fn describe(&self) -> String {
        let money = |v: f64| price::money(v, &self.currency);
        let head = if self.monthly < 0.0 {
            format!("costs ≈ {} more a month", money(-self.monthly))
        } else if self.at_most {
            format!("saves up to {} a month", money(self.monthly))
        } else {
            format!("saves ≈ {} a month", money(self.monthly))
        };
        if self.later <= 0.0 {
            return head;
        }
        let when = self.renewal.map(|t| format!(", {}", day(t))).unwrap_or_default();
        format!("{head} ({} now, {} when commitments renew{when})", money(self.now), money(self.later))
    }

    /// The rates used, one line each.
    pub fn rate_lines(&self) -> Vec<String> {
        let rate = |r: f64, unit: &str| price::rate_text(r, unit, &self.currency);
        self.lines
            .iter()
            .map(|l| {
                let c = &l.change;
                let scaled = |from: &Option<String>| from.as_ref().map(|f| format!(", scaled from {f}")).unwrap_or_default();
                match (&c.to, l.to_rate) {
                    (Some(to), Some(r2)) => format!(
                        "{} {} moved from {} at {}{} to {to} at {}{}",
                        crate::savings::qty(c.quantity),
                        l.unit,
                        c.usage_type,
                        rate(l.rate, &l.unit),
                        scaled(&l.scaled_from),
                        rate(r2, &l.unit),
                        scaled(&l.to_scaled_from)
                    ),
                    _ => format!(
                        "{} {} {} of {} at {}{}",
                        if c.effect == Effect::Add { "+" } else { "−" },
                        crate::savings::qty(c.quantity),
                        l.unit,
                        c.usage_type,
                        rate(l.rate, &l.unit),
                        scaled(&l.scaled_from)
                    ),
                }
            })
            .collect()
    }
}

/// `2027-03-13`
fn day(t: f64) -> String {
    let (y, m, d, ..) = crate::util::utc_parts(t as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Checks changes before pricing them: an error says what to fix.
pub fn validate(changes: &[Change]) -> Result<(), String> {
    if changes.len() > MAX_CHANGES {
        return Err(format!("at most {MAX_CHANGES} changes"));
    }
    for (i, c) in changes.iter().enumerate() {
        let n = i + 1;
        // Names end up on screen and in Markdown: no escape sequences or hidden characters.
        if [Some(&c.service), Some(&c.usage_type), c.to.as_ref()].into_iter().flatten().any(|t| t.chars().any(crate::util::is_unsafe)) {
            return Err(format!("change {n}: names have control or invisible characters"));
        }
        let usage = billing::split_region(c.usage_type.trim()).1;
        if c.service.trim().is_empty() || c.service.len() > 120 {
            return Err(format!("change {n}: give the service as Cost Explorer names it (`AWS Lambda`)"));
        }
        if usage.is_empty() || usage.len() > 200 {
            return Err(format!("change {n}: give the usage type as the bill names it (`Lambda-GB-Second`)"));
        }
        if !c.quantity.is_finite() || c.quantity <= 0.0 || c.quantity > 1e15 {
            return Err(format!("change {n}: the quantity is a positive monthly amount; `effect` says whether it is removed, added or moved"));
        }
        match (&c.effect, c.to.as_deref().map(str::trim)) {
            (Effect::Move, None | Some("")) => return Err(format!("change {n}: a move needs `to`, the usage type billed instead")),
            (Effect::Move, Some(to)) if to.len() > 200 => return Err(format!("change {n}: `to` is too long")),
            (Effect::Remove | Effect::Add, Some(to)) if !to.is_empty() => {
                return Err(format!("change {n}: only a move takes `to`"));
            }
            _ => {}
        }
        if c.at_most && c.effect != Effect::Remove {
            return Err(format!("change {n}: only a removal can be `atMost`"));
        }
    }
    Ok(())
}

/// Prices `changes` with the costs `s` of the environment's account (`region` its own).
pub fn estimate(changes: &[Change], s: &Snapshot, region: &str) -> Estimate {
    let currency = if s.currency.is_empty() { "USD".to_string() } else { s.currency.clone() };
    let mut out = Estimate { currency, priced_at: s.read_at, ..Default::default() };
    let (mut added_covered, mut different_units, mut services_later) = (false, Vec::new(), Vec::new());
    for c in changes {
        let usage = billing::split_region(c.usage_type.trim()).1.to_string();
        let change = Change { usage_type: usage.clone(), to: c.to.as_ref().map(|t| billing::split_region(t.trim()).1.to_string()), ..c.clone() };
        let Some((rate, unit, scaled_from)) = rate_of(s, &c.service, &usage, region) else {
            unpriced(&mut out, &usage);
            continue;
        };
        let covered = s.covered(&c.service).clamp(0.0, 1.0);
        let coverable = price::coverable(&usage);
        let (monthly, now, to_rate, to_scaled_from) = match c.effect {
            Effect::Remove => {
                let monthly = c.quantity * rate;
                (monthly, if coverable { monthly * (1.0 - covered) } else { monthly }, None, None)
            }
            Effect::Add => {
                added_covered |= coverable && covered > 0.0;
                (-c.quantity * rate, -c.quantity * rate, None, None)
            }
            Effect::Move => {
                let to = change.to.clone().unwrap_or_default();
                let Some((r2, unit2, from2)) = rate_of(s, &c.service, &to, region) else {
                    unpriced(&mut out, &to);
                    continue;
                };
                if !unit2.eq_ignore_ascii_case(&unit) {
                    different_units.push(format!("{usage} ({unit}) and {to} ({unit2})"));
                    unpriced(&mut out, &to);
                    continue;
                }
                let monthly = c.quantity * (rate - r2);
                added_covered |= monthly < 0.0 && coverable && covered > 0.0;
                let now = if monthly > 0.0 && coverable { monthly * (1.0 - covered) } else { monthly };
                (monthly, now, Some(r2), from2)
            }
        };
        if !monthly.is_finite() {
            unpriced(&mut out, &usage);
            continue;
        }
        if monthly - now > 0.0 && !services_later.contains(&c.service) {
            services_later.push(c.service.clone());
        }
        out.at_most |= c.at_most;
        out.lines.push(Line { change, monthly, now, rate, unit, to_rate, scaled_from, to_scaled_from });
    }
    out.monthly = out.lines.iter().map(|l| l.monthly).fold(0.0, |a, v| a + v);
    out.now = out.lines.iter().map(|l| l.now).fold(0.0, |a, v| a + v);
    out.later = (out.monthly - out.now).max(0.0);
    if out.later > 0.0 {
        out.renewal = services_later.iter().filter_map(|svc| s.renewal(svc)).reduce(f64::min);
        for svc in &services_later {
            out.notes.push(format!(
                "{} of {svc}'s usage is covered by Savings Plans or reservations: that share is already paid for and saves when they renew, or covers other usage meanwhile.",
                percent(s.covered(svc).clamp(0.0, 1.0))
            ));
        }
    }
    for l in &out.lines {
        for (target, from) in [(Some(&l.change.usage_type), &l.scaled_from), (l.change.to.as_ref(), &l.to_scaled_from)] {
            if let (Some(target), Some(from)) = (target, from) {
                out.notes.push(format!(
                    "{target} has no cost on this bill: priced from {from} in proportion to their sizes, as AWS prices the sizes of a family."
                ));
            }
        }
    }
    if out.lines.iter().any(|l| billing::tiered(&l.change.usage_type)) {
        out.notes.push("An average rate: this usage is priced in tiers, so the last units can cost a little less or more.".into());
    }
    if out.at_most {
        out.notes.push("Part of the usage goes away at most: the saving is a ceiling.".into());
    }
    if added_covered {
        out.notes.push(
            "Added usage is covered only while commitments have room; beyond that it is billed on demand, above this average rate."
                .into(),
        );
    }
    if !different_units.is_empty() {
        out.notes.push(format!(
            "Billed in different units, so not a move: {}. Describe it as a removal and an addition.",
            different_units.join("; ")
        ));
    }
    if !out.unpriced.is_empty() {
        out.notes.push(format!("No cost on this bill for {}: that part is not priced.", out.unpriced.join(", ")));
    }
    out
}

fn unpriced(out: &mut Estimate, usage: &str) {
    if !out.unpriced.iter().any(|u| u == usage) {
        out.unpriced.push(usage.to_string());
    }
}

/// The bill's rate for `usage` of `service` (see [`price::rate`]), or one scaled from another
/// size of its family: (rate, unit, the usage type it was scaled from).
fn rate_of(s: &Snapshot, service: &str, usage: &str, region: &str) -> Option<(f64, String, Option<String>)> {
    if let Some((r, unit)) = price::rate(s, service, usage, region) {
        return Some((r, unit, None));
    }
    let (prefix, family, size) = sized(usage)?;
    let mut best: Option<(f64, f64, String, String)> = None; // (cost, rate, unit, from)
    for u in s.usage.iter().filter(|u| u.service == service) {
        let name = billing::split_region(&u.usage_type).1;
        let Some((p, f, other)) = sized(name) else { continue };
        if p != prefix || f != family || other == size {
            continue;
        }
        let Some((r, unit)) = price::rate(s, service, name, region) else { continue };
        let cost: f64 = s.usage.iter().filter(|x| x.service == service && costs::usage_is(&x.usage_type, name)).map(|x| x.cost).sum();
        if best.as_ref().is_none_or(|b| cost > b.0) {
            best = Some((cost, r * size / other, unit, name.to_string()));
        }
    }
    best.map(|(_, r, unit, from)| (r, unit, Some(from)))
}

/// Instance usage types whose sizes AWS prices in proportion: (prefix, family, size factor).
/// `InstanceUsage:db.r6g.2xlarge` → (`InstanceUsage:`, `db.r6g`, 16). Redshift's older node
/// types (dc2) are not priced in proportion, so only RA3 nodes are.
fn sized(usage: &str) -> Option<(&str, &str, f64)> {
    let (prefix, class) = usage.split_at(usage.rfind(':')? + 1);
    let proportional = matches!(prefix, "InstanceUsage:" | "InstanceUsageIOOptimized:" | "Multi-AZUsage:" | "InstanceUsg:" | "Multi-AZUsg:")
        || (prefix == "Node:" && class.starts_with("ra3."));
    if !proportional {
        return None;
    }
    let (family, size) = class.rsplit_once('.')?;
    (!family.is_empty()).then_some((prefix, family, size_factor(size)?))
}

/// The normalization factor of an instance size: `large` 4, `xlarge` 8, `4xlarge` 32.
fn size_factor(size: &str) -> Option<f64> {
    Some(match size {
        "nano" => 0.25,
        "micro" => 0.5,
        "small" => 1.0,
        "medium" => 2.0,
        "large" => 4.0,
        "xlarge" => 8.0,
        _ => {
            let n: u32 = size.strip_suffix("xlarge")?.parse().ok().filter(|n| (2..=64).contains(n))?;
            8.0 * f64::from(n)
        }
    })
}

/// The services on the bill, most expensive first: for an estimate that named none of them.
pub fn services(s: &Snapshot) -> Vec<String> {
    let mut list: Vec<(String, f64)> = Vec::new();
    for u in &s.usage {
        match list.iter_mut().find(|(name, _)| *name == u.service) {
            Some(slot) => slot.1 += u.cost,
            None => list.push((u.service.clone(), u.cost)),
        }
    }
    list.sort_by(|a, b| b.1.total_cmp(&a.1));
    list.into_iter().map(|(name, _)| name).collect()
}

/// The usage types of `service` on the bill (without region prefix), most expensive first,
/// with their unit: for an estimate whose usage type the bill does not have.
pub fn usage_types(s: &Snapshot, service: &str) -> Vec<(String, String)> {
    let mut list: Vec<(String, String, f64)> = Vec::new();
    for u in s.usage.iter().filter(|u| u.service == service) {
        let name = billing::split_region(&u.usage_type).1;
        match list.iter_mut().find(|(n, _, _)| n == name) {
            Some(slot) => slot.2 += u.cost,
            None => list.push((name.to_string(), u.unit.clone(), u.cost)),
        }
    }
    list.sort_by(|a, b| b.2.total_cmp(&a.2));
    list.into_iter().map(|(n, unit, _)| (n, unit)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::costs::{Commitment, Coverage, UsageCost};

    fn usage(service: &str, usage_type: &str, cost: f64, quantity: f64, unit: &str) -> UsageCost {
        UsageCost { service: service.into(), usage_type: usage_type.into(), cost, quantity, unit: unit.into() }
    }

    const RDS: &str = "Amazon Relational Database Service";

    fn snapshot() -> Snapshot {
        Snapshot {
            currency: "USD".into(),
            read_at: 1_790_000_000.0,
            usage: vec![
                usage("AWS Lambda", "Lambda-GB-Second", 800.0, 48_000_000.0, "Lambda-GB-Second"),
                usage("AWS Lambda", "Lambda-GB-Second-ARM", 133.0, 10_000_000.0, "Lambda-GB-Second"),
                usage("AWS Lambda", "Request", 20.0, 100_000_000.0, "Requests"),
                usage(RDS, "InstanceUsage:db.r6g.xlarge", 380.0, 720.0, "Hrs"),
                usage(RDS, "InstanceUsage:db.r6g.large", 19.0, 72.0, "Hrs"),
                usage(RDS, "Aurora:ServerlessV2Usage", 86.4, 720.0, "ACU-Hr"),
                usage("Amazon Redshift", "Node:dc2.large", 180.0, 720.0, "Hrs"),
                usage("AmazonCloudWatch", "DataProcessing-Bytes", 400.0, 800.0, "GB"),
            ],
            coverage: vec![Coverage { service: RDS.into(), covered: 0.6, ..Default::default() }],
            commitments: vec![Commitment { kind: "RDS reserved instances".into(), services: vec![RDS.into()], end: Some(1_804_896_000.0), ..Default::default() }],
            ..Default::default()
        }
    }

    fn change(service: &str, usage: &str, effect: Effect, quantity: f64, to: Option<&str>) -> Change {
        Change { service: service.into(), usage_type: usage.into(), effect, quantity, to: to.map(Into::into), at_most: false }
    }

    #[test]
    fn removals_save_and_additions_cost() {
        let s = snapshot();
        let e = estimate(&[change("AWS Lambda", "Lambda-GB-Second", Effect::Remove, 3_000_000.0, None)], &s, "us-east-1");
        assert!((e.monthly - 50.0).abs() < 1e-9 && e.now == e.monthly && e.later == 0.0);
        assert_eq!(e.describe(), "saves ≈ US$ 50 a month");
        let e = estimate(&[change("AWS Lambda", "Request", Effect::Add, 50_000_000.0, None)], &s, "us-east-1");
        assert!((e.monthly + 10.0).abs() < 1e-9);
        assert_eq!(e.describe(), "costs ≈ US$ 10 more a month");
        assert_eq!(e.rate_lines(), ["+ 50 M Requests of Request at US$ 0.0000002 per Requests"]);
    }

    #[test]
    fn moves_price_the_difference_either_way() {
        let s = snapshot();
        let arm = change("AWS Lambda", "Lambda-GB-Second", Effect::Move, 3_000_000.0, Some("Lambda-GB-Second-ARM"));
        let e = estimate(&[arm], &s, "us-east-1");
        let expected = 3_000_000.0 * (800.0 / 48_000_000.0 - 133.0 / 10_000_000.0);
        assert!((e.monthly - expected).abs() < 1e-9);
        // A larger class costs more, all of it right away.
        let up = change(RDS, "InstanceUsage:db.r6g.large", Effect::Move, 730.0, Some("InstanceUsage:db.r6g.xlarge"));
        let e = estimate(&[up], &s, "us-east-1");
        assert!(e.monthly < 0.0 && e.now == e.monthly && e.later == 0.0);
        assert!(e.notes.iter().any(|n| n.contains("on demand")), "{:?}", e.notes);
    }

    #[test]
    fn covered_usage_saves_partly_at_renewal() {
        let s = snapshot();
        let e = estimate(&[change(RDS, "USE1-InstanceUsage:db.r6g.xlarge", Effect::Remove, 730.0, None)], &s, "us-east-1");
        let monthly = 730.0 * 380.0 / 720.0;
        assert!((e.monthly - monthly).abs() < 1e-9);
        assert!((e.now - monthly * 0.4).abs() < 1e-9 && (e.later - monthly * 0.6).abs() < 1e-9);
        assert_eq!(e.renewal, Some(1_804_896_000.0));
        assert_eq!(e.lines[0].change.usage_type, "InstanceUsage:db.r6g.xlarge", "the region prefix is dropped");
        assert!(e.describe().contains("when commitments renew, 2027-03-13"), "{}", e.describe());
    }

    #[test]
    fn unseen_sizes_scale_from_the_family() {
        let s = snapshot();
        let up = change(RDS, "InstanceUsage:db.r6g.xlarge", Effect::Move, 730.0, Some("InstanceUsage:db.r6g.4xlarge"));
        let e = estimate(&[up], &s, "us-east-1");
        // Scaled from the xlarge, which cost more on the bill than the large.
        let xl = 380.0 / 720.0;
        assert!((e.lines[0].to_rate.unwrap() - xl * 4.0).abs() < 1e-9);
        assert_eq!(e.lines[0].to_scaled_from.as_deref(), Some("InstanceUsage:db.r6g.xlarge"));
        assert!((e.monthly - 730.0 * (xl - xl * 4.0)).abs() < 1e-6);
        assert!(e.notes.iter().any(|n| n.contains("in proportion")));
        // Another family, or a Redshift dc2 node, is not scaled.
        let other = change(RDS, "InstanceUsage:db.r7g.large", Effect::Add, 730.0, None);
        assert_eq!(estimate(&[other], &s, "us-east-1").unpriced, ["InstanceUsage:db.r7g.large"]);
        let dc2 = change("Amazon Redshift", "Node:dc2.8xlarge", Effect::Add, 730.0, None);
        assert!(!estimate(&[dc2], &s, "us-east-1").priced());
        assert_eq!(size_factor("2xlarge"), Some(16.0));
        assert_eq!(size_factor("xlplus"), None);
        assert_eq!(size_factor("0xlarge"), None);
    }

    #[test]
    fn different_units_are_not_a_move() {
        let s = snapshot();
        let to_serverless = change(RDS, "InstanceUsage:db.r6g.xlarge", Effect::Move, 730.0, Some("Aurora:ServerlessV2Usage"));
        let e = estimate(&[to_serverless], &s, "us-east-1");
        assert!(!e.priced());
        assert!(e.notes.iter().any(|n| n.contains("removal and an addition")), "{:?}", e.notes);
    }

    #[test]
    fn unpriced_parts_and_ceilings_say_so() {
        let s = snapshot();
        let logs = Change { at_most: true, ..change("AmazonCloudWatch", "DataProcessing-Bytes", Effect::Remove, 100.0, None) };
        let e = estimate(&[logs, change("AWS Lambda", "Lambda-Provisioned-Concurrency", Effect::Add, 1.0, None)], &s, "us-east-1");
        assert!(e.at_most && (e.monthly - 50.0).abs() < 1e-9);
        assert_eq!(e.describe(), "saves up to US$ 50 a month");
        assert_eq!(e.unpriced, ["Lambda-Provisioned-Concurrency"]);
        assert!(e.notes.iter().any(|n| n.contains("tiers")) && e.notes.iter().any(|n| n.contains("not priced")));
    }

    #[test]
    fn validation_explains_what_to_fix() {
        let ok = change("AWS Lambda", "Lambda-GB-Second", Effect::Remove, 1.0, None);
        assert!(validate(std::slice::from_ref(&ok)).is_ok());
        let bad = |c: Change| validate(&[c]).unwrap_err();
        assert!(bad(Change { quantity: -5.0, ..ok.clone() }).contains("positive"));
        assert!(bad(Change { quantity: f64::NAN, ..ok.clone() }).contains("positive"));
        assert!(bad(Change { effect: Effect::Move, ..ok.clone() }).contains("needs `to`"));
        assert!(bad(Change { to: Some("x".into()), ..ok.clone() }).contains("only a move"));
        assert!(bad(Change { at_most: true, effect: Effect::Add, ..ok.clone() }).contains("only a removal"));
        assert!(bad(Change { service: " ".into(), ..ok.clone() }).contains("service"));
        assert!(bad(Change { usage_type: "Lambda\u{1b}[2J".into(), ..ok.clone() }).contains("control"));
        assert!(bad(Change { effect: Effect::Move, to: Some("x\u{202e}".into()), ..ok.clone() }).contains("control"));
        assert!(validate(&vec![ok; MAX_CHANGES + 1]).is_err());
    }

    #[test]
    fn hints_list_what_the_bill_has() {
        let s = snapshot();
        assert_eq!(services(&s)[0], "AWS Lambda");
        let types = usage_types(&s, "AWS Lambda");
        assert_eq!(types[0], ("Lambda-GB-Second".to_string(), "Lambda-GB-Second".to_string()));
        assert_eq!(types.len(), 3);
    }
}

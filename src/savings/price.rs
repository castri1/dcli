//! Dollars on findings, from the account's own bill. dcli keeps no prices: a usage type's
//! rate is what it cost over the last 30 days divided by what was used (amortized, so
//! discounts and Savings Plans are in it), read by the Costs module ([`Snapshot`]). A
//! finding's monthly quantity of that usage type times the rate is what it saves.
//!
//! Usage that Savings Plans or reservations cover is already paid for: removing it saves
//! when the commitment renews (or frees it for other usage), so each price says how much
//! shows on the bill now and how much later.

use serde::Serialize;

use super::{Finding, billing};
use crate::costs::{Snapshot, UsageCost};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    /// The usage type removed (without its region prefix).
    pub usage: String,
    pub monthly: f64,
    /// The part of `monthly` outside commitments: it shows on the bill right away.
    pub now: f64,
    /// The bill's rate for `usage` and its unit.
    pub rate: f64,
    pub unit: String,
    /// For a change to another usage type: that one, and its rate.
    pub instead: Option<(String, f64)>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Price {
    /// About this much a month.
    pub monthly: f64,
    /// What shows on the bill as soon as the change is made.
    pub now: f64,
    /// What commitments cover: saved when they renew.
    pub later: f64,
    /// When the first commitment covering the service ends (Unix seconds).
    pub renewal: Option<f64>,
    /// The whole cost of the usage: the change removes part of it, so it saves at most this.
    pub at_most: bool,
    pub currency: String,
    pub items: Vec<Item>,
    /// What the figure assumes.
    pub notes: Vec<String>,
}

/// Usage types Savings Plans or reservations can cover: compute and instance hours, not
/// requests, storage or Aurora Serverless capacity (reserved instances do not apply to it).
pub fn coverable(usage: &str) -> bool {
    usage.starts_with("Lambda-GB-Second")
        || usage.starts_with("Lambda-Provisioned")
        || usage.starts_with("InstanceUsage")
        || usage.starts_with("InstanceUsg:")
        || usage.starts_with("Multi-AZUsg:")
        || usage.starts_with("Node:")
}

/// The bill's rate for the usage type `name` of `service`: in `region` when the bill has it
/// there, else over every region, as (cost per unit, unit).
pub fn rate(s: &Snapshot, service: &str, name: &str, region: &str) -> Option<(f64, String)> {
    let all: Vec<&UsageCost> = s.usage.iter().filter(|u| u.service == service && billing::split_region(&u.usage_type).1 == name).collect();
    let local: Vec<&UsageCost> = all.iter().copied().filter(|u| billing::in_region(&u.usage_type, region)).collect();
    let picked = if local.iter().any(|u| u.quantity > 0.0) { local } else { all };
    let (cost, quantity) = picked.iter().fold((0.0, 0.0), |(c, q), u| (c + u.cost, q + u.quantity));
    let unit = picked.first()?.unit.clone();
    (quantity > 0.0 && cost.is_finite() && cost >= 0.0).then(|| (cost / quantity, unit))
}

/// What `f` saves a month with the rates of `s`, the costs of the account it runs in
/// (`region` its environment's); `None` when the bill has nothing to price it with.
pub fn price(f: &Finding, s: &Snapshot, region: &str) -> Option<Price> {
    let service = f.billed_service()?;
    let covered = s.covered(service).clamp(0.0, 1.0);
    let (mut items, mut notes, mut missing) = (Vec::new(), Vec::new(), Vec::new());
    let mut at_most = false;
    for b in &f.billed {
        let Some((r, unit)) = rate(s, service, &b.usage, region) else {
            missing.push(b.usage.clone());
            continue;
        };
        let (per_unit, instead) = match &b.instead {
            None => (r, None),
            Some(other) => match rate(s, service, other, region) {
                Some((r2, _)) if r2 < r => (r - r2, Some((other.clone(), r2))),
                Some(_) => {
                    notes.push(format!("On this bill {other} costs no less than {}.", b.usage));
                    continue;
                }
                None => {
                    missing.push(other.clone());
                    continue;
                }
            },
        };
        let monthly = b.quantity * per_unit;
        if !monthly.is_finite() || monthly <= 0.0 {
            continue;
        }
        at_most |= b.at_most;
        items.push(Item { usage: b.usage.clone(), monthly, now: monthly, rate: r, unit, instead });
    }
    if items.is_empty() {
        return None;
    }
    // What commitments can cover comes off the usage outside them first.
    let coverable_monthly: f64 = items.iter().filter(|i| coverable(&i.usage)).map(|i| i.monthly).sum();
    if coverable_monthly > 0.0 {
        let share = s.saved_now(service, coverable_monthly) / coverable_monthly;
        for i in items.iter_mut().filter(|i| coverable(&i.usage)) {
            i.now = i.monthly * share;
        }
    }
    let monthly: f64 = items.iter().map(|i| i.monthly).sum();
    let now: f64 = items.iter().map(|i| i.now).sum();
    let later = (monthly - now).max(0.0);
    if items.iter().any(|i| i.instead.is_some()) {
        notes.push("The difference between the two rates of your bill, for the same usage.".into());
    }
    if items.iter().any(|i| billing::tiered(&i.usage)) {
        notes.push("An average rate: this usage is priced in tiers, so the last units can cost a little less or more.".into());
    }
    if at_most {
        notes.push("What this usage costs now: the change removes part of it, so it saves at most this.".into());
    }
    if later > 0.0 {
        notes.push(commitment_note(s, service, covered));
    }
    if !missing.is_empty() {
        notes.push(format!("No cost on this bill for {}: that part is not priced.", missing.join(", ")));
    }
    Some(Price {
        monthly,
        now,
        later,
        renewal: if later > 0.0 { s.renewal(service) } else { None },
        at_most,
        currency: if s.currency.is_empty() { "USD".into() } else { s.currency.clone() },
        items,
        notes,
    })
}

/// Why part of a saving waits for the commitments of `service` to renew.
pub fn commitment_note(s: &Snapshot, service: &str, covered: f64) -> String {
    match s.on_demand_monthly(service) {
        Some(outside) => format!(
            "Savings Plans or reservations cover {} of {service}'s usage and go on covering what is left: what runs outside them ({} a month) is saved now, the rest when they renew (or it covers other usage meanwhile).",
            super::percent(covered),
            money(outside, if s.currency.is_empty() { "USD" } else { &s.currency })
        ),
        None => format!(
            "{} of {service}'s usage is covered by Savings Plans or reservations: that share is already paid for and saves when they renew, or covers other usage meanwhile.",
            super::percent(covered)
        ),
    }
}

/// Prices every finding with `s` (or clears their prices without costs).
pub fn price_all(findings: &mut [Finding], s: Option<&Snapshot>, region: &str) {
    for f in findings {
        f.price = s.and_then(|s| price(f, s, region));
    }
}

/// The findings' dollars together.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Total {
    pub monthly: f64,
    pub now: f64,
    /// Findings with a firm figure, and those priced only as "at most".
    pub priced: usize,
    pub at_most: usize,
    pub currency: String,
}

/// What `findings` save together. Findings on the same resource can remove the same usage
/// (memory and arm64 on one function, an idle cluster and the class of one of its instances):
/// for each resource (the finding's target: a function, a cluster) and usage type, either the
/// largest of the findings on the whole resource counts, or the findings on its parts (each
/// instance, the largest on it) added up, whichever is more. A finding's items of one usage
/// type (two idle instances of one class) add up. "At most" figures are left out.
pub fn total<'a>(findings: impl IntoIterator<Item = &'a Finding>) -> Option<Total> {
    use std::collections::HashMap;
    /// Per resource and usage type: the largest whole-resource figure, and per part its largest.
    #[derive(Default)]
    struct Slot {
        whole: (f64, f64),
        parts: HashMap<String, (f64, f64)>,
    }
    let max = |slot: &mut (f64, f64), v: (f64, f64)| {
        if v.0 > slot.0 {
            *slot = v;
        }
    };
    let mut slots: HashMap<(String, String, String), Slot> = HashMap::new();
    let mut out = Total::default();
    for f in findings {
        let Some(p) = &f.price else { continue };
        out.currency = p.currency.clone();
        if p.at_most {
            out.at_most += 1;
            continue;
        }
        out.priced += 1;
        let resource = if f.target.is_empty() { f.resource.clone() } else { f.target.clone() };
        let mut by_usage: HashMap<&str, (f64, f64)> = HashMap::new();
        for i in &p.items {
            let v = by_usage.entry(i.usage.as_str()).or_default();
            *v = (v.0 + i.monthly, v.1 + i.now);
        }
        for (usage, v) in by_usage {
            let slot = slots.entry((f.service.to_string(), resource.clone(), usage.to_string())).or_default();
            if f.resource.is_empty() || f.resource == resource {
                max(&mut slot.whole, v);
            } else {
                max(slot.parts.entry(f.resource.clone()).or_default(), v);
            }
        }
    }
    if out.priced == 0 && out.at_most == 0 {
        return None;
    }
    // Folds from 0, not `sum`: an empty float sum is -0.0.
    for slot in slots.values() {
        let parts = slot.parts.values().fold((0.0, 0.0), |a, v| (a.0 + v.0, a.1 + v.1));
        let v = if parts.0 > slot.whole.0 { parts } else { slot.whole };
        out.monthly += v.0;
        out.now += v.1;
    }
    Some(out)
}

/// `US$ 1,130`, `US$ 41`, `US$ 4.20`, `< US$ 0.01`, `-US$ 12`; other currencies after the
/// figure.
pub fn money(v: f64, currency: &str) -> String {
    if v <= -0.005 {
        return format!("-{}", money(-v, currency));
    }
    let figure = if v >= 100.0 {
        thousands(v.round())
    } else if v >= 10.0 {
        format!("{v:.0}")
    } else if v >= 0.005 {
        format!("{v:.2}")
    } else {
        return if currency.is_empty() || currency == "USD" { "< US$ 0.01".into() } else { format!("< 0.01 {currency}") };
    };
    if currency.is_empty() || currency == "USD" { format!("US$ {figure}") } else { format!("{figure} {currency}") }
}

fn thousands(v: f64) -> String {
    let digits = format!("{:.0}", v.abs());
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if v < 0.0 { format!("-{out}") } else { out }
}

/// A rate with three significant figures: `US$ 0.0000167 per Lambda-GB-Second`.
pub fn rate_text(rate: f64, unit: &str, currency: &str) -> String {
    let decimals = if rate >= 1.0 || !rate.is_finite() || rate <= 0.0 { 2 } else { ((-rate.log10()).ceil() as usize + 2).min(12) };
    let figure = format!("{rate:.decimals$}");
    let figure = if figure.contains('.') { figure.trim_end_matches('0').trim_end_matches('.').to_string() } else { figure };
    let per = if unit.is_empty() || unit == "N/A" { String::new() } else { format!(" per {unit}") };
    if currency.is_empty() || currency == "USD" { format!("US$ {figure}{per}") } else { format!("{figure} {currency}{per}") }
}

impl Price {
    /// `≈ US$ 140 a month (US$ 90 now, US$ 50 when commitments renew, 2027-03-13)`.
    pub fn describe(&self) -> String {
        let head = if self.at_most {
            format!("up to {} a month", money(self.monthly, &self.currency))
        } else {
            format!("≈ {} a month", money(self.monthly, &self.currency))
        };
        if self.later <= 0.0 {
            return head;
        }
        let when = self.renewal.map(|t| format!(", {}", crate::util::fmt_utc(t).split(' ').next().unwrap_or_default())).unwrap_or_default();
        format!("{head} ({} now, {} when commitments renew{when})", money(self.now, &self.currency), money(self.later, &self.currency))
    }

    /// `≈ US$ 140/mo`, `≤ US$ 410/mo`: for a narrow column.
    pub fn short(&self) -> String {
        format!("{} {}/mo", if self.at_most { "≤" } else { "≈" }, money(self.monthly, &self.currency))
    }

    /// The rates used, one line each.
    pub fn rate_lines(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|i| match &i.instead {
                None => format!("{}: {} ({})", i.usage, rate_text(i.rate, &i.unit, &self.currency), money(i.monthly, &self.currency)),
                Some((other, r2)) => format!(
                    "{other} at {} instead of {} at {} ({})",
                    rate_text(*r2, &i.unit, &self.currency),
                    i.usage,
                    rate_text(i.rate, &i.unit, &self.currency),
                    money(i.monthly, &self.currency)
                ),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::costs::{Commitment, Coverage, UsageCost};
    use crate::savings::{Finding, Impact};

    fn usage(service: &str, usage_type: &str, cost: f64, quantity: f64, unit: &str) -> UsageCost {
        UsageCost { service: service.into(), usage_type: usage_type.into(), cost, quantity, unit: unit.into() }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            currency: "USD".into(),
            usage: vec![
                usage("AWS Lambda", "Lambda-GB-Second", 800.0, 48_000_000.0, "Lambda-GB-Second"),
                usage("AWS Lambda", "USW2-Lambda-GB-Second", 10.0, 500_000.0, "Lambda-GB-Second"),
                usage("AWS Lambda", "Lambda-GB-Second-ARM", 133.0, 10_000_000.0, "Lambda-GB-Second"),
                usage("AmazonCloudWatch", "DataProcessing-Bytes", 400.0, 800.0, "GB"),
                usage("Amazon Relational Database Service", "InstanceUsage:db.r6g.xlarge", 380.0, 720.0, "Hrs"),
                usage("Amazon Relational Database Service", "InstanceUsage:db.r6g.large", 190.0, 720.0, "Hrs"),
                usage("Amazon DynamoDB", "TimedStorage-ByteHrs", 25.0, 100.0, "GB-Mo"),
                usage("Amazon DynamoDB", "IA-TimedStorage-ByteHrs", 10.0, 100.0, "GB-Mo"),
            ],
            coverage: vec![Coverage { service: "Amazon Relational Database Service".into(), covered: 0.6, ..Default::default() }],
            commitments: vec![Commitment {
                kind: "RDS reserved instances".into(),
                services: vec!["Amazon Relational Database Service".into()],
                end: Some(1_804_896_000.0),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn finding(service: &'static str) -> Finding {
        Finding { service, ..Finding::new("Lambda", "f", "f", "s", "r", "x", Impact::High) }
    }

    #[test]
    fn rates_prefer_the_environment_region() {
        let s = snapshot();
        let (r, unit) = rate(&s, "AWS Lambda", "Lambda-GB-Second", "us-east-1").unwrap();
        assert!((r - 800.0 / 48_000_000.0).abs() < 1e-12, "the us-east-1 line only");
        assert_eq!(unit, "Lambda-GB-Second");
        let (r, _) = rate(&s, "AWS Lambda", "Lambda-GB-Second", "us-west-2").unwrap();
        assert!((r - 10.0 / 500_000.0).abs() < 1e-12);
        let (r, _) = rate(&s, "AWS Lambda", "Lambda-GB-Second", "eu-west-3").unwrap();
        assert!((r - 810.0 / 48_500_000.0).abs() < 1e-12, "every region when the bill has none there");
        // DynamoDB's Standard storage is not mixed with Standard-IA's.
        assert_eq!(rate(&s, "Amazon DynamoDB", "TimedStorage-ByteHrs", "us-east-1").unwrap().0, 0.25);
    }

    #[test]
    fn prices_on_demand_usage_now() {
        let f = finding("Lambda").billed(billing::LAMBDA_GBS, 3_000_000.0);
        let p = price(&f, &snapshot(), "us-east-1").unwrap();
        assert!((p.monthly - 50.0).abs() < 1e-9);
        assert_eq!((p.now, p.later, p.renewal), (p.monthly, 0.0, None));
        assert_eq!(p.short(), "≈ US$ 50/mo");
    }

    #[test]
    fn two_rates_price_the_difference() {
        let f = finding("Lambda").billed_instead(billing::LAMBDA_GBS, billing::LAMBDA_GBS_ARM, 3_000_000.0);
        let p = price(&f, &snapshot(), "us-east-1").unwrap();
        let expected = 3_000_000.0 * (800.0 / 48_000_000.0 - 133.0 / 10_000_000.0);
        assert!((p.monthly - expected).abs() < 1e-9);
        assert_eq!(
            p.rate_lines()[0],
            "Lambda-GB-Second-ARM at US$ 0.0000133 per Lambda-GB-Second instead of Lambda-GB-Second at US$ 0.0000167 per Lambda-GB-Second (US$ 10)"
        );
        // Without the arm64 rate on the bill there is nothing to compare.
        let mut s = snapshot();
        s.usage.retain(|u| !u.usage_type.ends_with("-ARM"));
        assert!(price(&f, &s, "us-east-1").is_none());
    }

    #[test]
    fn covered_usage_saves_at_renewal() {
        let f = finding("Aurora").billed_instead(&billing::rds_instance("db.r6g.xlarge"), &billing::rds_instance("db.r6g.large"), 730.0);
        let p = price(&f, &snapshot(), "us-east-1").unwrap();
        let monthly = 730.0 * (380.0 - 190.0) / 720.0;
        assert!((p.monthly - monthly).abs() < 1e-9);
        assert!((p.later - monthly * 0.6).abs() < 1e-9 && (p.now - monthly * 0.4).abs() < 1e-9);
        assert_eq!(p.renewal, Some(1_804_896_000.0));
        assert!(p.describe().contains("when commitments renew, 2027-03-13"), "{}", p.describe());
    }

    #[test]
    fn removals_come_off_what_runs_outside_commitments_first() {
        // Six nodes, four reserved: removing two saves all of it now.
        let mut s = snapshot();
        s.coverage[0].on_demand_cost = 190.0;
        let f = finding("Aurora").billed(&billing::rds_instance("db.r6g.large"), 365.0);
        let p = price(&f, &s, "us-east-1").unwrap();
        assert!((p.monthly - 365.0 * 190.0 / 720.0).abs() < 1e-9);
        assert_eq!((p.now, p.later), (p.monthly, 0.0));
        // More than runs outside them: the rest waits for the renewal.
        let f = finding("Aurora").billed(&billing::rds_instance("db.r6g.xlarge"), 730.0);
        let p = price(&f, &s, "us-east-1").unwrap();
        let outside = 190.0 * 30.4375 / 30.0;
        assert!((p.now - outside).abs() < 1e-9 && (p.later - (p.monthly - outside)).abs() < 1e-9);
        assert!(p.notes.iter().any(|n| n.contains("go on covering what is left")), "{:?}", p.notes);
    }

    #[test]
    fn at_most_and_tiers_say_so() {
        let f = finding("CloudWatch Logs").billed_at_most(billing::LOGS_INGESTED, 400.0);
        let p = price(&f, &snapshot(), "us-east-1").unwrap();
        assert!(p.at_most && (p.monthly - 200.0).abs() < 1e-9);
        assert!(p.notes.iter().any(|n| n.contains("tiers")) && p.notes.iter().any(|n| n.contains("at most")));
        assert_eq!(p.short(), "≤ US$ 200/mo");
    }

    #[test]
    fn totals_count_each_resource_usage_once() {
        let s = snapshot();
        let mut memory = finding("Lambda").billed(billing::LAMBDA_GBS, 3_000_000.0);
        let mut arm = finding("Lambda").billed_instead(billing::LAMBDA_GBS, billing::LAMBDA_GBS_ARM, 3_000_000.0);
        let mut other = Finding { target: "g".into(), ..finding("Lambda").billed(billing::LAMBDA_GBS, 600_000.0) };
        let mut logs = finding("CloudWatch Logs").billed_at_most(billing::LOGS_INGESTED, 400.0);
        price_all(std::slice::from_mut(&mut memory), Some(&s), "us-east-1");
        price_all(std::slice::from_mut(&mut arm), Some(&s), "us-east-1");
        price_all(std::slice::from_mut(&mut other), Some(&s), "us-east-1");
        price_all(std::slice::from_mut(&mut logs), Some(&s), "us-east-1");
        let t = total([&memory, &arm, &other, &logs]).unwrap();
        assert!((t.monthly - 60.0).abs() < 1e-9, "50 for f (the larger of two), 10 for g: {}", t.monthly);
        assert_eq!((t.priced, t.at_most), (3, 1));
    }

    #[test]
    fn totals_add_instances_of_a_cluster() {
        let s = snapshot();
        let xl = billing::rds_instance("db.r6g.xlarge");
        let priced = |mut f: Finding| {
            price_all(std::slice::from_mut(&mut f), Some(&s), "us-east-1");
            f
        };
        let aurora = |resource: &str| Finding {
            service: "Aurora",
            ..Finding::new("Aurora", "orders", resource, "s", "r", "x", Impact::High)
        };
        // An idle cluster of two instances of one class: both count.
        let idle = priced(aurora("orders").billed(&xl, 730.0).billed(&xl, 730.0));
        let rate = 380.0 / 720.0;
        let t = total([&idle]).unwrap();
        assert!((t.monthly - 2.0 * 730.0 * rate).abs() < 1e-6, "{}", t.monthly);
        // Two oversized instances of one cluster: both count.
        let one = priced(aurora("orders / orders-1").billed(&xl, 365.0));
        let two = priced(aurora("orders / orders-2").billed(&xl, 365.0));
        let t = total([&one, &two]).unwrap();
        assert!((t.monthly - 730.0 * rate).abs() < 1e-6, "{}", t.monthly);
        // With the idle cluster too, the cluster's whole saving counts, not more.
        let t = total([&idle, &one, &two]).unwrap();
        assert!((t.monthly - 2.0 * 730.0 * rate).abs() < 1e-6, "{}", t.monthly);
    }

    #[test]
    fn money_and_rates_read_well() {
        assert_eq!(money(1130.4, "USD"), "US$ 1,130");
        assert_eq!(money(41.2, "USD"), "US$ 41");
        assert_eq!(money(4.2, ""), "US$ 4.20");
        assert_eq!(money(0.001, "USD"), "< US$ 0.01");
        assert_eq!(money(1_234_567.0, "EUR"), "1,234,567 EUR");
        // A cost that went up shows as such, not as "< US$ 0.01".
        assert_eq!(money(-12.4, "USD"), "-US$ 12");
        assert_eq!(money(-1500.0, "EUR"), "-1,500 EUR");
        assert_eq!(money(-0.001, "USD"), "< US$ 0.01");
        // A free usage type (rate 0) or a broken one reads plainly, without a panic.
        assert_eq!(rate_text(0.0, "GB", "USD"), "US$ 0 per GB");
        assert_eq!(rate_text(f64::NAN, "GB", "USD"), "US$ NaN per GB");
        assert_eq!(rate_text(800.0 / 48_000_000.0, "Lambda-GB-Second", "USD"), "US$ 0.0000167 per Lambda-GB-Second");
        assert_eq!(rate_text(0.5, "GB", "USD"), "US$ 0.5 per GB");
        assert_eq!(rate_text(0.527777, "Hrs", "USD"), "US$ 0.528 per Hrs");
    }
}

//! X-Ray: sampling rules that record far more traces than an overview needs (each trace
//! recorded is billed), and groups with Insights on. Which rule recorded which traces is not
//! known without reading traces (billed per trace), so the saving is taken from the bill's
//! traces as a ceiling.

use super::{Finding, Impact, Scan, billing, qty};
use crate::aws::xray::{self, SamplingRule};
use crate::aws;
use crate::costs::{self, Snapshot};

const MODULE: &str = "X-Ray";
/// X-Ray's own default: 1 request a second, then 5 % of the rest.
const DEFAULT_RATE: f64 = 0.05;
const DEFAULT_RESERVOIR: i64 = 1;
/// A rule from this rate (or this reservoir, per second) records much more than it needs to.
const HIGH_RATE: f64 = 0.5;
const HIGH_RESERVOIR: i64 = 10;
/// Traces a month removed: medium and high impact from these.
const TRACES_MEDIUM: f64 = 1_000_000.0;
const TRACES_HIGH: f64 = 10_000_000.0;
/// Cost Explorer's name of the service.
const SERVICE: &str = "AWS X-Ray";

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let costs = costs::cached(scan.env);
    let traces = recorded(costs.as_ref());
    let mut out: Vec<Finding> = xray::sampling_rules(scan.aws)?.iter().filter_map(|r| sampling(r, traces.as_ref())).collect();
    if let Some((usage, monthly)) = insights(costs.as_ref()) {
        let groups: Vec<String> = xray::groups(scan.aws)?
            .into_iter()
            .filter(|g| g.insights_configuration.as_ref().is_some_and(|i| i.insights_enabled))
            .map(|g| g.group_name)
            .collect();
        if !groups.is_empty() {
            out.push(
                Finding::new(
                    MODULE,
                    "",
                    format!("Insights on {}", groups.join(", ")),
                    format!("{} group{} with X-Ray Insights on", groups.len(), if groups.len() == 1 { "" } else { "s" }),
                    "Turn Insights off on the groups nobody watches (it analyses every trace they match)",
                    format!("up to {} traces analysed a month", qty(monthly)),
                    Impact::of(monthly, TRACES_MEDIUM, TRACES_HIGH),
                )
                .kind("xray-insights")
                .billed_at_most(&usage, monthly)
                .caveat("The bill counts the traces Insights analysed for every group together."),
            );
        }
    }
    Ok(out)
}

/// A usage type of X-Ray on the bill whose name has one of `words`, with its monthly quantity:
/// (usage type without region, quantity), the most expensive when several match.
fn usage_named(costs: Option<&Snapshot>, words: &[&str]) -> Option<(String, f64)> {
    let s = costs?;
    let mut by_name: Vec<(String, f64, f64)> = Vec::new();
    for u in s.usage.iter().filter(|u| u.service == SERVICE) {
        let name = billing::split_region(&u.usage_type).1;
        let lower = name.to_ascii_lowercase();
        if !words.iter().any(|w| lower.contains(w)) {
            continue;
        }
        match by_name.iter_mut().find(|(n, _, _)| n == name) {
            Some(slot) => {
                slot.1 += u.quantity;
                slot.2 += u.cost;
            }
            None => by_name.push((name.to_string(), u.quantity, u.cost)),
        }
    }
    by_name.sort_by(|a, b| b.2.total_cmp(&a.2));
    // The last 30 days, as a month.
    by_name.into_iter().next().map(|(n, q, _)| (n, q * 30.4375 / 30.0)).filter(|(_, q)| *q > 0.0)
}

/// The traces recorded a month, from the bill (`XRay-TracesStored`, or another name with
/// "stored" or "recorded").
fn recorded(costs: Option<&Snapshot>) -> Option<(String, f64)> {
    let usual = billing::XRAY_TRACES.to_ascii_lowercase();
    usage_named(costs, &[usual.as_str(), "tracesstored", "tracesrecorded", "traces-stored", "traces-recorded"])
}

/// The traces Insights analysed a month, from the bill.
fn insights(costs: Option<&Snapshot>) -> Option<(String, f64)> {
    usage_named(costs, &["insight"])
}

/// A rule that samples much more than X-Ray's default; `traces`: what the bill recorded a
/// month (usage type, quantity), when the costs were read.
fn sampling(r: &SamplingRule, traces: Option<&(String, f64)>) -> Option<Finding> {
    let default = r.rule_name == "Default";
    let high = if default {
        r.fixed_rate > DEFAULT_RATE + 1e-9 || r.reservoir_size > DEFAULT_RESERVOIR
    } else {
        r.fixed_rate >= HIGH_RATE || r.reservoir_size >= HIGH_RESERVOIR
    };
    // Without traces on the bill there is nothing to save.
    if !high || traces.is_some_and(|(_, q)| *q <= 0.0) {
        return None;
    }
    let target = DEFAULT_RATE.min(r.fixed_rate);
    // The share of this rule's sampled requests a lower rate would no longer record.
    let share = if r.fixed_rate > 0.0 { 1.0 - target / r.fixed_rate } else { 0.0 };
    let scope = [
        (r.service_name.as_str(), "service"),
        (r.http_method.as_str(), "method"),
        (r.url_path.as_str(), "path"),
        (r.host.as_str(), "host"),
    ]
    .into_iter()
    .filter(|(v, _)| !v.is_empty() && *v != "*")
    .map(|(v, what)| format!("{what} {v}"))
    .collect::<Vec<_>>();
    let mut f = Finding::new(
        MODULE,
        "",
        format!("sampling rule {}", r.rule_name),
        format!(
            "records {} request{} a second, then {:.0} % of the rest",
            r.reservoir_size,
            if r.reservoir_size == 1 { "" } else { "s" },
            r.fixed_rate * 100.0
        ),
        format!(
            "Lower it to {:.0} % and a reservoir of {} a second{}",
            target * 100.0,
            DEFAULT_RESERVOIR.min(r.reservoir_size).max(1),
            if default { "" } else { ", or delete it so the default rule applies" }
        ),
        match traces {
            Some((_, q)) => format!("up to {} traces a month", qty(q * share)),
            None => format!("{:.0} % fewer traces from this rule", share * 100.0),
        },
        traces.map_or(Impact::Low, |(_, q)| Impact::of(q * share, TRACES_MEDIUM, TRACES_HIGH)),
    )
    .kind("xray-sampling")
    .score(traces.map_or(0.0, |(_, q)| q * share / TRACES_HIGH))
    .evidence(format!("priority {} · {}", r.priority, if scope.is_empty() { "matches every request".into() } else { scope.join(" · ") }))
    .caveat("The bill does not say which rule recorded which traces: this is the most, if this rule recorded them all.")
    .caveat("Fewer traces means rare errors are traced less often; keep a reservoir of at least 1 a second so every service stays on the map.");
    if let Some((usage, q)) = traces {
        f = f.billed_at_most(usage, q * share).evidence(format!("{} traces recorded a month on the bill", qty(*q)));
    }
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::costs::UsageCost;

    fn rule(name: &str, rate: f64, reservoir: i64) -> SamplingRule {
        SamplingRule { rule_name: name.into(), fixed_rate: rate, reservoir_size: reservoir, priority: 100, service_name: "*".into(), ..Default::default() }
    }

    #[test]
    fn rules_recording_much_more_than_the_default() {
        let traces = ("XRay-TracesStored".to_string(), 20_000_000.0);
        let f = sampling(&rule("checkout", 1.0, 1), Some(&traces)).unwrap();
        assert_eq!(f.kind, "xray-sampling");
        assert!(f.billed[0].at_most && (f.billed[0].quantity - 19_000_000.0).abs() < 1e-6);
        assert_eq!(f.impact, Impact::High);
        assert!(f.recommendation.contains("5 %") && f.recommendation.contains("delete it"));
        assert!(sampling(&rule("checkout", 0.1, 1), Some(&traces)).is_none());
        assert!(sampling(&rule("Default", 0.05, 1), Some(&traces)).is_none());
        let raised = sampling(&rule("Default", 0.2, 1), Some(&traces)).unwrap();
        assert!(!raised.recommendation.contains("delete it"));
        // Without the costs read: the finding stands, unpriced.
        assert!(sampling(&rule("checkout", 1.0, 1), None).unwrap().billed.is_empty());
    }

    #[test]
    fn traces_come_from_the_bill_by_name() {
        let usage = |usage_type: &str, quantity: f64, cost: f64| UsageCost {
            service: SERVICE.into(),
            usage_type: usage_type.into(),
            quantity,
            cost,
            unit: "Traces".into(),
        };
        let s = Snapshot {
            usage: vec![usage("USE1-XRay-TracesStored", 3_000_000.0, 15.0), usage("USE1-XRay-TracesAccessed", 9_000_000.0, 4.5), usage("XRay-InsightsTraces", 1_000.0, 0.01)],
            ..Default::default()
        };
        let (name, q) = recorded(Some(&s)).unwrap();
        assert_eq!(name, "XRay-TracesStored");
        assert!((q - 3_000_000.0 * 30.4375 / 30.0).abs() < 1e-6);
        assert_eq!(insights(Some(&s)).unwrap().0, "XRay-InsightsTraces");
        assert!(recorded(None).is_none());
    }
}

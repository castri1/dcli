//! CloudWatch custom metrics: every metric outside the `AWS/` namespaces that receives data is
//! billed each month (prorated by the hour), and each combination of dimension values is a
//! metric of its own. The namespaces with the most metrics, and the dimensions that multiply
//! them, are what to look at: Lambda Insights publishes several metrics per function, the
//! CloudWatch agent several per host, an application's embedded metrics one per value of every
//! dimension.

use std::collections::{BTreeMap, BTreeSet};

use super::{Finding, Impact, Scan, billing, qty};
use crate::aws::metrics::{self, Metric};
use crate::aws;

/// Metrics listed at most (ListMetrics is free, 500 a page).
const MAX_METRICS: usize = 50_000;
/// Namespaces reported, at most.
const TOP_NAMESPACES: usize = 10;
/// Metrics in a namespace: a finding from 20, medium impact from 100, high from 1,000.
const METRICS_LOW: f64 = 20.0;
const METRICS_MEDIUM: f64 = 100.0;
const METRICS_HIGH: f64 = 1000.0;
/// Names and dimensions shown in the evidence.
const SHOWN: usize = 6;

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let (all, more) = metrics::list_recent_metrics(scan.aws, MAX_METRICS)?;
    Ok(findings(&all, more))
}

/// The namespaces of `all` with the most custom metrics, largest first; `more`: the listing
/// was cut short.
pub(crate) fn findings(all: &[Metric], more: bool) -> Vec<Finding> {
    let mut by_namespace: BTreeMap<&str, Vec<&Metric>> = BTreeMap::new();
    for m in all.iter().filter(|m| !m.namespace.starts_with("AWS/") && !m.namespace.is_empty()) {
        by_namespace.entry(&m.namespace).or_default().push(m);
    }
    let mut list: Vec<(&str, Vec<&Metric>)> = by_namespace.into_iter().filter(|(_, v)| v.len() as f64 >= METRICS_LOW).collect();
    list.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
    list.into_iter().take(TOP_NAMESPACES).map(|(namespace, metrics)| namespace_finding(namespace, &metrics, more)).collect()
}

fn namespace_finding(namespace: &str, metrics: &[&Metric], more: bool) -> Finding {
    let n = metrics.len() as f64;
    let mut names: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dimensions: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for m in metrics {
        *names.entry(&m.name).or_default() += 1;
        for (name, value) in &m.dimensions {
            dimensions.entry(name).or_default().insert(value);
        }
    }
    let mut names: Vec<(&str, usize)> = names.into_iter().collect();
    names.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut dims: Vec<(&str, &BTreeSet<&str>)> = dimensions.iter().map(|(k, v)| (*k, v)).collect();
    dims.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
    let recommendation = match namespace {
        "LambdaInsights" => {
            "Turn Lambda Insights off on the functions whose metrics nobody watches (remove the LambdaInsightsExtension layer), and keep it where you use them"
        }
        "CWAgent" => "Collect fewer metrics, or fewer dimensions per metric, in the CloudWatch agent's configuration",
        _ => {
            "Publish fewer metrics or fewer dimension combinations: aggregate before publishing, drop dimensions with many values (ids, users), or log the values instead"
        }
    };
    let mut finding = Finding::new(
        "",
        "",
        namespace,
        format!("{} custom metric{} in {namespace}", qty(n), if metrics.len() == 1 { "" } else { "s" }),
        recommendation,
        format!("up to {} metrics a month", qty(n)),
        Impact::of(n, METRICS_MEDIUM, METRICS_HIGH),
    )
    .kind("custom-metrics")
    .score(n / METRICS_HIGH)
    .billed_at_most(billing::METRICS, n);
    let shown: Vec<String> = names.iter().take(SHOWN).map(|(name, count)| format!("{name} ×{count}")).collect();
    let rest = names.len().saturating_sub(SHOWN);
    finding = finding.evidence(format!(
        "{} metric name{}: {}{}",
        names.len(),
        if names.len() == 1 { "" } else { "s" },
        shown.join(", "),
        if rest > 0 { format!(", and {rest} more") } else { String::new() }
    ));
    if !dims.is_empty() {
        let shown: Vec<String> =
            dims.iter().take(SHOWN).map(|(name, values)| format!("{name} ({} value{})", values.len(), if values.len() == 1 { "" } else { "s" })).collect();
        finding = finding.evidence(format!("dimensions, each value a metric of its own: {}", shown.join(", ")));
    }
    if namespace == "LambdaInsights"
        && let Some(functions) = dimensions.get("function_name")
    {
        let list: Vec<&str> = functions.iter().take(10).copied().collect();
        let rest = functions.len().saturating_sub(list.len());
        finding = finding.evidence(format!(
            "on {} function{}: {}{}",
            functions.len(),
            if functions.len() == 1 { "" } else { "s" },
            list.join(", "),
            if rest > 0 { format!(", and {rest} more") } else { String::new() }
        ));
    }
    if more {
        finding = finding.caveat("The account has more metrics than dcli listed: this count is part of them.");
    }
    finding
        .caveat("A metric is billed for the hours it receives data; the first ones each month are free, and the rate falls in tiers as the count grows.")
        .caveat("Alarms and dashboards that read these metrics stop working once they are no longer published.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(namespace: &str, name: &str, dims: &[(&str, &str)]) -> Metric {
        Metric { namespace: namespace.into(), name: name.into(), dimensions: dims.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    #[test]
    fn namespaces_with_the_most_custom_metrics() {
        let mut all = Vec::new();
        for f in 0..30 {
            let function = format!("fn-{f:02}");
            for name in ["memory_utilization", "cpu_total_time", "init_duration", "rx_bytes"] {
                all.push(metric("LambdaInsights", name, &[("function_name", &function)]));
            }
            all.push(metric("AWS/Lambda", "Invocations", &[("FunctionName", &function)]));
        }
        for customer in 0..25 {
            all.push(metric("Orders/App", "OrderCreated", &[("Service", "orders"), ("CustomerId", &customer.to_string())]));
        }
        for i in 0..5 {
            all.push(metric("Tiny", "x", &[("n", &i.to_string())]));
        }
        let found = findings(&all, true);
        let names: Vec<&str> = found.iter().map(|f| f.resource.as_str()).collect();
        assert_eq!(names, ["LambdaInsights", "Orders/App"], "AWS/ namespaces and small ones are left out");
        let insights = &found[0];
        assert_eq!(insights.summary, "120 custom metrics in LambdaInsights");
        assert_eq!((insights.kind, insights.impact), ("custom-metrics", Impact::Medium));
        assert_eq!(insights.billed[0].usage, billing::METRICS);
        assert!(insights.billed[0].at_most && insights.billed[0].quantity == 120.0);
        assert!(insights.recommendation.contains("LambdaInsightsExtension"));
        assert!(insights.evidence.iter().any(|e| e.starts_with("on 30 functions: fn-00, fn-01")), "{:?}", insights.evidence);
        assert!(insights.caveats.iter().any(|c| c.contains("more metrics than dcli listed")));
        let app = &found[1];
        assert!(app.evidence.iter().any(|e| e == "dimensions, each value a metric of its own: CustomerId (25 values), Service (1 value)"), "{:?}", app.evidence);
        assert!(app.evidence[0].starts_with("1 metric name: OrderCreated ×25"));
        assert!(found.iter().all(|f| f.target.is_empty()));
    }
}

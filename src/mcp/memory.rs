//! `lambda_memory`: the memory Lambda functions use against what they have, read from their
//! REPORT lines with Logs Insights. Insights bills the data it scans, so the user sets how much
//! one analysis and one month may scan (`dcli mcp --insights-gb`, `--insights-gb-month`); every
//! analysis is measured first (the groups' IncomingBytes metrics, cheap) and refused when it
//! would pass either. Analyses are kept ([`crate::savings::lambda::keep`]): savings_findings and
//! dcli's Savings module show their findings for 30 days without scanning again.

use serde_json::{Value, json};

use super::tools::{aws_data, date, env_prop, finding_id, money, object};
use super::{Server, aws_error, usage};
use crate::costs::Snapshot;
use crate::savings::{self, Finding, GB, lambda as memory, price, size};
use crate::util::{now_secs, sanitize};

/// Days read unless asked otherwise, and at most.
const DEFAULT_DAYS: u32 = 7;
const MAX_DAYS: u32 = 14;

pub const DESCRIPTION: &str = "The memory Lambda functions use against what they have, from their reports over the last days (text or \
JSON logs), with the findings of functions that could have less (priced with the bill). It runs Logs Insights, which bills the data \
it scans: every run is billed. Call it with `estimateOnly` first (no Insights charge), tell the user the GB and what they cost, and \
run it only once the user agrees (or already said to go ahead within a budget). It stays within the GB the user allows per analysis \
and per month (`dcli mcp --insights-gb`), and stops a query that reaches them. Without `functions` it reads the most expensive \
functions (by GB-seconds) that fit. The analysis is kept: savings_findings and dcli's Savings module show its findings for 30 days.";

pub fn schema() -> Value {
    object(&["env"], json!({
        "env": env_prop(),
        "functions": {
            "type": "array", "maxItems": 200, "items": { "type": "string", "maxLength": 170 },
            "description": "Function names to read; default: the most expensive ones that fit in what may be scanned"
        },
        "days": { "type": "integer", "description": "Days of logs to read, 1 to 14 (default 7)" },
        "estimateOnly": { "type": "boolean", "description": "Only say what it would scan and cost, without running Insights" }
    }))
}

/// The functions to read, by name: those asked, or the most expensive (`ranked` is name,
/// GB-seconds, bytes of logs, most expensive first) whose logs fit in `room` bytes. Functions
/// that ran nothing, or whose groups received no logs over the window, are left out: there is
/// nothing to read. The second list is what did not fit.
fn choose(ranked: &[(String, f64, f64)], asked: bool, room: f64) -> (Vec<String>, Vec<String>) {
    if asked {
        return (ranked.iter().filter(|r| r.2 > 0.0).map(|r| r.0.clone()).collect(), Vec::new());
    }
    let (mut chosen, mut left, mut bytes) = (Vec::new(), Vec::new(), 0.0);
    for (name, gbs, b) in ranked {
        if *gbs <= 0.0 || *b <= 0.0 {
            continue;
        }
        if bytes + b <= room {
            bytes += b;
            chosen.push(name.clone());
        } else {
            left.push(name.clone());
        }
    }
    (chosen, left)
}

/// What scanning `bytes` costs at the bill's Logs Insights rate, when the costs read have it:
/// (cost, rate as text).
fn scan_cost(costs: Option<&Snapshot>, region: &str, bytes: f64) -> Option<(f64, String)> {
    let s = costs?;
    let (rate, unit) = price::rate(s, "AmazonCloudWatch", "DataScanned-Bytes", region)?;
    let currency = if s.currency.is_empty() { "USD" } else { s.currency.as_str() };
    Some((bytes / GB * rate, price::rate_text(rate, &unit, currency)))
}

/// Why the tool does not run, and how the user turns it on; `rate` is the bill's Logs Insights
/// rate, when the costs read have it.
fn off(env: &str, rate: Option<String>) -> String {
    let rate = rate.map(|r| format!(" (Logs Insights costs {r} on this account's bill)")).unwrap_or_default();
    format!(
        "Logs Insights is off for this server, so it cannot read the memory functions use: the user allows it by registering dcli \
         with how many GB one analysis may scan, e.g. `claude mcp remove dcli` and then `claude mcp add dcli -- dcli mcp --env {env} \
         --insights-gb 1000`{rate}. Meanwhile `estimateOnly` says what an analysis would scan, and resource_report kind lambda shows a \
         function's memory over 24 h."
    )
}

fn gb(bytes: f64) -> f64 {
    (bytes / GB * 10.0).round() / 10.0
}

pub fn lambda_memory(s: &Server, args: &Value) -> Result<String, String> {
    let (name, env) = s.env(args)?;
    let days = args.get("days").and_then(Value::as_u64).unwrap_or(u64::from(DEFAULT_DAYS)).clamp(1, u64::from(MAX_DAYS)) as u32;
    let estimate_only = args.get("estimateOnly").and_then(Value::as_bool).unwrap_or(false);
    let caps = &s.insights;
    let costs = (s.costs)(name);
    if caps.per_run <= 0.0 && !estimate_only {
        return Err(off(name, scan_cost(costs.as_ref(), &env.region, GB).map(|(_, rate)| rate)));
    }
    if !estimate_only && caps.per_month <= 0.0 {
        return Err("the GB Logs Insights may scan a month is 0 (--insights-gb-month): nothing may be scanned".into());
    }
    if !estimate_only && caps.usage.is_none() {
        return Err("cannot find the home directory, where dcli counts what Logs Insights scans a month: it does not scan without counting".into());
    }
    let mut asked: Vec<String> = Vec::new();
    for v in args.get("functions").and_then(Value::as_array).into_iter().flatten() {
        let n = v.as_str().unwrap_or_default().trim().to_string();
        if !n.is_empty() && !asked.contains(&n) {
            asked.push(n);
        }
    }
    let aws = s.session(name, env)?;
    let all = memory::all_functions(&aws).map_err(|e| aws_error(name, &e))?;
    let not_found: Vec<String> = asked.iter().filter(|n| !all.iter().any(|f| f.function_name == **n)).cloned().collect();
    let candidates = if asked.is_empty() { all } else { all.into_iter().filter(|f| asked.contains(&f.function_name)).collect() };
    if candidates.is_empty() {
        return Err(format!("no such Lambda function in '{name}': {}", sanitize(&not_found.join(", "))));
    }
    let estimate = memory::memory_estimate(&aws, Some(candidates), days).map_err(|e| aws_error(name, &e))?;
    let gbs = memory::gb_seconds_by_function(&aws, &estimate.functions, days).map_err(|e| aws_error(name, &e))?;
    let mut ranked: Vec<(String, f64, f64)> = estimate
        .functions
        .iter()
        .zip(&estimate.per_function)
        .map(|(f, bytes)| (f.function_name.clone(), gbs.get(&f.function_name).copied().unwrap_or(0.0), *bytes))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let now = now_secs();
    let used = match &caps.usage {
        Some(path) => usage::scanned(path, now).map_err(|e| format!("cannot read what Logs Insights scanned this month: {e}"))?,
        None => 0.0,
    };
    let left_this_month = (caps.per_month - used).max(0.0);
    let room = if caps.per_run <= 0.0 { f64::INFINITY } else { caps.per_run.min(left_this_month) };
    let (chosen, over) = choose(&ranked, !asked.is_empty(), room);
    let picked = estimate.only(&chosen);
    // Asked for, but their groups received nothing over the window: nothing to read.
    let no_logs: Vec<String> = ranked.iter().filter(|r| r.2 <= 0.0 && asked.contains(&r.0)).map(|r| r.0.clone()).collect();
    let cost_of = |bytes: f64| scan_cost(costs.as_ref(), &env.region, bytes).map(|(c, rate)| json!({ "cost": money(c), "rate": rate }));
    let listed = |names: &[String]| -> Vec<Value> {
        names
            .iter()
            .filter_map(|n| ranked.iter().find(|r| r.0 == *n))
            .map(|(n, g, b)| json!({ "function": n, "gbSecondsInWindow": g.round(), "logs": size(*b) }))
            .collect()
    };
    let month = json!({
        "scannedGb": gb(used),
        "capGb": gb(caps.per_month),
        "leftGb": gb(left_this_month),
        "perAnalysisGb": gb(caps.per_run),
    });

    if estimate_only {
        return aws_data(json!({
            "env": name,
            "days": days,
            "wouldScan": size(picked.bytes),
            "wouldScanGb": gb(picked.bytes),
            "scanCost": cost_of(picked.bytes),
            "analysisAllowed": caps.per_run > 0.0,
            "month": month,
            "functions": listed(&chosen),
            "notFitting": listed(&over),
            "noLogs": no_logs,
            "sharedLogGroups": estimate.shared,
            "notFound": not_found,
            "notes": [
                "Measured from the log groups' IncomingBytes over the same window the analysis reads: what Logs Insights would scan, and bill.",
                "Functions that share a log group cannot be told apart in their reports and are left out.",
                "Running it is billed: tell the user the GB and the cost, and run it once they agree.",
            ],
        }));
    }
    if picked.functions.is_empty() {
        return Err(match over.first() {
            Some(first) => format!(
                "even the most expensive function, {}, has more logs over {days} days than may be scanned now ({} of {} left this \
                 month, {} an analysis): ask for fewer `days`",
                sanitize(first),
                size(left_this_month),
                size(caps.per_month),
                size(caps.per_run)
            ),
            None => format!("no function of '{name}' with its own log group ran in the last {days} days: nothing to read"),
        });
    }
    if picked.bytes > caps.per_run {
        return Err(format!(
            "reading {} over {days} days would scan {}, more than the {} one analysis may scan (--insights-gb): ask for fewer \
             `functions` (the most expensive: {}) or fewer `days`, or leave `functions` out to read the most expensive ones that fit",
            if chosen.len() == 1 { sanitize(&chosen[0]) } else { format!("{} functions", chosen.len()) },
            size(picked.bytes),
            size(caps.per_run),
            sanitize(&ranked.iter().take(5).map(|r| r.0.as_str()).collect::<Vec<_>>().join(", "))
        ));
    }
    // Counted before it runs, so two analyses at once cannot pass the month's cap together; a
    // twentieth more than the estimate, as logs can grow a little, within both caps. The
    // analysis stops any query that reaches it.
    let Some(path) = &caps.usage else { return Err("cannot count what Logs Insights scans".into()) };
    let budget = (picked.bytes * 1.05).min(caps.per_run).min(left_this_month).max(picked.bytes);
    let reserved = match usage::reserve(path, now, budget, caps.per_month) {
        Ok(Ok(r)) => r,
        Ok(Err(left)) => {
            return Err(format!(
                "this analysis would scan {}, but only {} is left this month of the {} allowed (--insights-gb-month): ask for fewer \
                 `functions` or `days`, or wait for next month",
                size(picked.bytes),
                size(left),
                size(caps.per_month)
            ));
        }
        Err(e) => return Err(format!("cannot count what Logs Insights scans: {e}")),
    };
    let run = match memory::memory_analysis_within(&aws, name, &picked, Some(reserved.bytes), &|_, _| {}) {
        Ok(run) => run,
        Err(e) => {
            // Nothing was scanned: the reservation is given back.
            let _ = usage::settle(path, &reserved, 0.0);
            return Err(aws_error(name, &e));
        }
    };
    if let Err(e) = usage::settle(path, &reserved, run.bytes_scanned) {
        eprintln!("dcli mcp: cannot update what Logs Insights scanned this month: {e}");
    }
    let mut found: Vec<Finding> = run.uses.iter().filter_map(memory::memory_finding).map(|f| Finding { service: "Lambda", ..f }).collect();
    price::price_all(&mut found, costs.as_ref(), &env.region);
    savings::sort(&mut found);
    let after = caps.usage.as_ref().and_then(|p| usage::scanned(p, now).ok()).unwrap_or(used + run.bytes_scanned);
    let mut notes = vec![
        "Memory also sets CPU: a function that computes may get slower with less, and save less. Try the size in dev first.".to_string(),
        "Kept for 30 days: savings_findings and dcli's Savings module include these findings without scanning again.".to_string(),
    ];
    if let Some(why) = &run.stopped {
        notes.push(format!(
            "The analysis {}: the functions above are those read until then, possibly from part of their logs.",
            sanitize(why)
        ));
    }
    if let Some(e) = &run.keep_error {
        notes.push(format!("Could not keep the analysis for later: {}", sanitize(e)));
    }
    if !over.is_empty() {
        notes.push(format!("{} more functions did not fit in what may be scanned: ask for them by name in another call.", over.len()));
    }
    aws_data(json!({
        "env": name,
        "region": env.region,
        "days": days,
        "scanned": size(run.bytes_scanned),
        "scannedGb": gb(run.bytes_scanned),
        "scanCost": cost_of(run.bytes_scanned),
        "month": { "scannedGb": gb(after), "capGb": gb(caps.per_month), "leftGb": gb((caps.per_month - after).max(0.0)) },
        "functions": run.uses.iter().map(|u| json!({
            "function": u.function,
            "arm64": u.arm64,
            "memoryMb": u.memory_mb,
            "p99UsedMb": u.p99_mb.round(),
            "maxUsedMb": u.max_mb.round(),
            "fitsMb": u.fitting_mb(),
            "smallerMb": u.smaller_mb(),
            "invocations": u.invocations,
            "reportLines": u.samples,
            "avgBilledMs": u.avg_billed_ms.round(),
        })).collect::<Vec<_>>(),
        "stopped": run.stopped,
        "withoutInvocations": run.silent,
        "notFitting": listed(&over),
        "noLogs": no_logs,
        "sharedLogGroups": estimate.shared,
        "notFound": not_found,
        "findings": found.iter().map(|f| json!({
            "id": finding_id(f),
            "resource": f.resource,
            "summary": f.summary,
            "recommendation": f.recommendation,
            "reduction": f.reduction,
            "impact": f.impact.label(),
            "evidence": f.evidence,
            "caveats": f.caveats,
            "billed": f.billed,
            "price": f.price.as_ref().map(|p| json!({
                "text": p.describe(),
                "monthly": money(p.monthly),
                "now": money(p.now),
                "later": money(p.later),
                "renewal": p.renewal.map(date),
                "currency": p.currency,
                "rates": p.rate_lines(),
                "notes": p.notes,
            })),
        })).collect::<Vec<_>>(),
        "notes": notes,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{server, session};
    use super::*;

    fn call(s: &Server, args: Value) -> (bool, String) {
        let replies = session(s, &[json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "lambda_memory", "arguments": args } })]);
        let r = &replies[0]["result"];
        (r["isError"].as_bool().unwrap_or(false), r["content"][0]["text"].as_str().unwrap_or_default().to_string())
    }

    #[test]
    fn the_most_expensive_functions_that_fit_are_read() {
        let ranked = vec![
            ("big".to_string(), 900.0, 600.0),
            ("huge-logs".to_string(), 500.0, 5000.0),
            ("small".to_string(), 100.0, 300.0),
            ("idle".to_string(), 0.0, 1.0),
        ];
        let (chosen, left) = choose(&ranked, false, 1000.0);
        assert_eq!(chosen, ["big", "small"]);
        assert_eq!(left, ["huge-logs"]);
        // Asked by name, all of them (the cap is checked on their total)…
        assert_eq!(choose(&ranked, true, 1.0).0.len(), 4);
        // …but those whose groups received nothing: there is nothing to read.
        let quiet = vec![("quiet".to_string(), 50.0, 0.0), ("busy".to_string(), 50.0, 10.0)];
        assert_eq!(choose(&quiet, true, 1.0).0, ["busy"]);
        assert_eq!(choose(&quiet, false, 100.0).0, ["busy"]);
    }

    #[test]
    fn scans_are_priced_with_the_bill() {
        let costs = Snapshot {
            usage: vec![crate::costs::UsageCost {
                service: "AmazonCloudWatch".into(),
                usage_type: "USE1-DataScanned-Bytes".into(),
                cost: 5.38,
                quantity: 1076.0,
                unit: "GB".into(),
            }],
            ..Default::default()
        };
        let (cost, rate) = scan_cost(Some(&costs), "us-east-1", 600.0 * GB).unwrap();
        assert!((cost - 3.0).abs() < 1e-9, "{cost}");
        assert_eq!(rate, "US$ 0.005 per GB");
        assert!(scan_cost(None, "us-east-1", GB).is_none());
    }

    #[test]
    fn off_unless_the_user_allows_it() {
        let s = server(None, None);
        let (error, text) = call(&s, json!({ "env": "dev" }));
        assert!(error);
        assert!(text.contains("Logs Insights is off") && text.contains("--insights-gb 1000"), "{text}");
        // Arguments are checked like any tool's.
        let (error, text) = call(&s, json!({ "env": "dev", "function": ["x"] }));
        assert!(error && text.contains("unknown field `function`"), "{text}");
        let s = server(None, None).with_insights(1000.0, 3000.0, None);
        assert!((s.insights.per_run - 1000.0 * GB).abs() < 1.0 && (s.insights.per_month - 3000.0 * GB).abs() < 1.0);
    }

    #[test]
    fn no_price_of_its_own_and_billed_runs_are_not_read_only() {
        // Off: the rate is the bill's when the costs were read, none of dcli's own otherwise.
        let (_, text) = call(&server(None, None), json!({ "env": "dev" }));
        assert!(!text.contains("US$"), "{text}");
        let bill = Snapshot {
            usage: vec![crate::costs::UsageCost {
                service: "AmazonCloudWatch".into(),
                usage_type: "USE1-DataScanned-Bytes".into(),
                cost: 5.0,
                quantity: 1000.0,
                unit: "GB".into(),
            }],
            ..Default::default()
        };
        let (_, text) = call(&server(None, Some(bill)), json!({ "env": "dev" }));
        assert!(text.contains("costs US$ 0.005 per GB on this account's bill"), "{text}");
        let listed = super::super::tools::list();
        let tool = listed.iter().find(|t| t["name"] == "lambda_memory").unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], false);
        assert_eq!(tool["annotations"]["idempotentHint"], false);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert!(tool["description"].as_str().unwrap().contains("only once the user agrees"));
    }

    #[test]
    fn nothing_is_scanned_without_a_count_or_a_month_cap() {
        // A month of 0 GB holds nothing.
        let path = std::env::temp_dir().join(format!("dcli-mcp-memory-{}", std::process::id())).join("mcp-usage.json");
        let s = server(None, None).with_insights(1000.0, 0.0, Some(path));
        let (error, text) = call(&s, json!({ "env": "dev" }));
        assert!(error && text.contains("a month is 0"), "{text}");
        // Without a home directory to count in, it does not scan.
        let s = server(None, None).with_insights(1000.0, 3000.0, None);
        let (error, text) = call(&s, json!({ "env": "dev" }));
        assert!(error && text.contains("does not scan without counting"), "{text}");
    }
}

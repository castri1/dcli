//! CloudWatch Logs: log groups kept forever, by size, and the groups that ingest the most.
//! What AWS services write for you (Lambda's logs among them) is billed as vended logs, at
//! another rate and in tiers: those groups are priced with that usage type.

use super::{Finding, Fix, GB, Impact, Scan, billing, metric_data, per_month, query, query_id, size, total};
use crate::aws::logs::{self, LogGroup};
use crate::aws::{self, Aws};
use crate::costs::Snapshot;
use crate::util::fmt_ago;

const MODULE: &str = "Logs";
/// Log groups looked at, at most.
const MAX_GROUPS: usize = 2000;
/// Bytes stored by a group kept forever: a finding of its own from 1 GB (smaller groups
/// are summed up in one), medium impact from 100 GB, high from 1 TB.
const STORED_OWN: f64 = GB;
const STORED_MEDIUM: f64 = 100.0 * GB;
const STORED_HIGH: f64 = 1024.0 * GB;
/// Bytes ingested a month: the largest groups from 10 GB, medium from 100 GB, high from 1 TB.
const INGESTED_LOW: f64 = 10.0 * GB;
const INGESTED_MEDIUM: f64 = 100.0 * GB;
const INGESTED_HIGH: f64 = 1024.0 * GB;
/// Largest ingesters listed, at most.
const TOP_INGESTERS: usize = 10;
/// Groups named in the summed-up finding of small groups.
const NAMED: usize = 10;

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let groups = all_groups(scan.aws)?;
    let ingested = ingestion(scan.aws, &groups, scan.days)?;
    let mut found = forever(&groups, &ingested, scan.days);
    found.extend(ingesters(&groups, &ingested, scan.days, crate::costs::cached(scan.env).as_ref()));
    Ok(found)
}

/// Whether a group receives vended logs: Lambda functions' own groups, and those AWS services
/// deliver to under `/aws/vendedlogs/`.
pub(crate) fn vended(group: &str) -> bool {
    group.starts_with("/aws/lambda/") || group.starts_with("/aws/vendedlogs/")
}

/// The usage type a group's ingestion is billed as: vended logs for vended groups, unless the
/// bill read shows ingestion only as plain log data (an account billed before vended pricing).
pub(crate) fn ingestion_usage(group: &str, costs: Option<&Snapshot>) -> &'static str {
    let has = |s: &Snapshot, name: &str| {
        s.usage.iter().any(|u| u.service == "AmazonCloudWatch" && billing::split_region(&u.usage_type).1 == name && u.quantity > 0.0)
    };
    match costs {
        _ if !vended(group) => billing::LOGS_INGESTED,
        Some(s) if !has(s, billing::LOGS_VENDED) && has(s, billing::LOGS_INGESTED) => billing::LOGS_INGESTED,
        _ => billing::LOGS_VENDED,
    }
}

pub(crate) fn all_groups(aws: &Aws) -> aws::Result<Vec<LogGroup>> {
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let (page, next) = logs::describe_log_groups(aws, None, token.as_deref())?;
        all.extend(page);
        match next {
            Some(t) if all.len() < MAX_GROUPS => token = Some(t),
            _ => return Ok(all),
        }
    }
}

/// Bytes each group received over the window (IncomingBytes), in the order of `groups`.
fn ingestion(aws: &Aws, groups: &[LogGroup], days: u32) -> aws::Result<Vec<f64>> {
    let queries: Vec<_> = groups
        .iter()
        .enumerate()
        .map(|(i, g)| query(query_id("inb", i), "AWS/Logs", "IncomingBytes", &[("LogGroupName", &g.log_group_name)], "Sum", 86_400))
        .collect();
    Ok(metric_data(aws, &queries, days)?.iter().map(total).collect())
}

fn stored(g: &LogGroup) -> f64 {
    g.stored_bytes.unwrap_or(0) as f64
}

/// Groups without retention: one finding per large group, one for all the small ones.
fn forever(groups: &[LogGroup], ingested: &[f64], days: u32) -> Vec<Finding> {
    let mut found = Vec::new();
    let mut small: Vec<&LogGroup> = Vec::new();
    for (g, received) in groups.iter().zip(ingested.iter().copied().chain(std::iter::repeat(0.0))) {
        if g.retention_in_days.is_some() || stored(g) <= 0.0 {
            continue;
        }
        if stored(g) < STORED_OWN {
            small.push(g);
            continue;
        }
        let mut finding = Finding::new(
            MODULE,
            &g.log_group_name,
            &g.log_group_name,
            format!("kept forever: {} stored", size(stored(g))),
            "Keep the events 30 days, or what your policy asks",
            format!("{} stored, none of it expires", size(stored(g))),
            Impact::of(stored(g), STORED_MEDIUM, STORED_HIGH),
        )
        .kind("log-retention")
        .score(stored(g) / STORED_HIGH)
        .billed_at_most(billing::LOGS_STORED, stored(g) / GB)
        .evidence(format!("stored {} (compressed), no retention", size(stored(g))))
        .evidence(format!("received {} in the last {days} days", size(received)))
        .caveat("Deleted events cannot be recovered: export to S3 first what must be kept.")
        .caveat("What remains depends on how much arrived within the days kept.")
        .fix(Fix::LogRetention { group: g.log_group_name.clone() });
        if let Some(created) = g.creation_time {
            finding = finding.evidence(format!("created {}", fmt_ago(created as f64 / 1000.0)));
        }
        found.push(finding);
    }
    if !small.is_empty() {
        small.sort_by(|a, b| stored(b).total_cmp(&stored(a)));
        let sum: f64 = small.iter().map(|g| stored(g)).sum();
        let n = small.len();
        let mut finding = Finding::new(
            MODULE,
            "",
            format!("{n} small log group{}", if n == 1 { "" } else { "s" }),
            format!("kept forever: {} stored together", size(sum)),
            "Set a retention on each (Logs module, or with the AWS CLI)",
            format!("{} stored, none of it expires", size(sum)),
            Impact::of(sum, STORED_MEDIUM, STORED_HIGH),
        )
        .kind("log-retention")
        .score(sum / STORED_HIGH)
        .billed_at_most(billing::LOGS_STORED, sum / GB)
        .caveat("Deleted events cannot be recovered: export to S3 first what must be kept.");
        for g in small.iter().take(NAMED) {
            finding = finding.evidence(format!("{} — {}", g.log_group_name, size(stored(g))));
        }
        if n > NAMED {
            finding = finding.evidence(format!("and {} more", n - NAMED));
        }
        found.push(finding);
    }
    found
}

/// The groups that ingest the most, priced with what `costs` (the bill read) bills them as.
fn ingesters(groups: &[LogGroup], ingested: &[f64], days: u32, costs: Option<&Snapshot>) -> Vec<Finding> {
    let mut busy: Vec<(&LogGroup, f64)> =
        groups.iter().zip(ingested).map(|(g, bytes)| (g, bytes * per_month(days))).filter(|(_, m)| *m >= INGESTED_LOW).collect();
    busy.sort_by(|a, b| b.1.total_cmp(&a.1));
    busy.into_iter()
        .take(TOP_INGESTERS)
        .map(|(g, monthly)| {
            let retention = g.retention_in_days.map_or("kept forever".to_string(), |d| format!("kept {d} days"));
            let usage = ingestion_usage(&g.log_group_name, costs);
            let lambda = g.log_group_name.starts_with("/aws/lambda/");
            let recommendation = if lambda {
                "Log less: set the function's log level (Lambda logging config: application log level WARN or ERROR, system level WARN) and drop debug lines"
            } else {
                "Log less: drop debug lines and sample the noisiest ones"
            };
            let finding = Finding::new(
                MODULE,
                &g.log_group_name,
                &g.log_group_name,
                format!("receives {} a month", size(monthly)),
                recommendation,
                format!("{} ingested a month", size(monthly)),
                Impact::of(monthly, INGESTED_MEDIUM, INGESTED_HIGH),
            )
            .kind("log-ingestion")
            .score(monthly / INGESTED_HIGH)
            .billed_at_most(usage, monthly / GB)
            .evidence(format!("received {} in the last {days} days (IncomingBytes)", size(monthly / per_month(days))))
            .evidence(format!("{retention}, {} stored", size(stored(g))))
            .caveat("Ingestion is billed per GB received: only logging less lowers it.");
            if usage == billing::LOGS_VENDED {
                finding
                    .evidence("billed as vended logs (VendedLog-Bytes)")
                    .caveat("Vended logs are billed in tiers that fall as the account's volume grows: the last GB saved can cost a little less than the average.")
                    .caveat(if lambda {
                        "Lambda can also send its logs to S3 or Firehose instead, billed at lower vended-log rates, for logs kept rather than searched."
                    } else {
                        "The service writing them can usually log less or send them to S3 instead."
                    })
            } else {
                finding.caveat("For logs searched only now and then, a new group in the Infrequent Access class bills less per GB (the class is set when a group is created, and it lacks some features).")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(name: &str, gb: f64, retention: Option<i64>) -> LogGroup {
        LogGroup { log_group_name: name.into(), stored_bytes: Some((gb * GB) as i64), retention_in_days: retention, creation_time: None }
    }

    #[test]
    fn groups_kept_forever_by_size() {
        let groups = [
            group("/aws/lambda/big", 1200.0, None),
            group("/aws/lambda/kept", 500.0, Some(30)),
            group("/ecs/medium", 150.0, None),
            group("/aws/lambda/tiny", 0.2, None),
            group("/aws/lambda/tinier", 0.1, None),
            group("/aws/lambda/empty", 0.0, None),
        ];
        let found = forever(&groups, &[0.0; 6], 14);
        let names: Vec<&str> = found.iter().map(|f| f.resource.as_str()).collect();
        assert_eq!(names, ["/aws/lambda/big", "/ecs/medium", "2 small log groups"]);
        assert_eq!(found[0].impact, Impact::High);
        assert_eq!(found[1].impact, Impact::Medium);
        assert_eq!(found[0].fix, Some(Fix::LogRetention { group: "/aws/lambda/big".into() }));
        assert_eq!(found[2].fix, None);
        assert_eq!(found[2].target, "");
        assert_eq!(found[2].evidence, ["/aws/lambda/tiny — 205 MB", "/aws/lambda/tinier — 102 MB"]);
        assert_eq!(found[2].summary, "kept forever: 307 MB stored together");
    }

    #[test]
    fn largest_ingesters() {
        let groups = [group("/a", 1.0, Some(7)), group("/b", 1.0, None), group("/c", 1.0, None)];
        // Over 14 days: 70 GB (≈ 152 GB a month), 2 GB (≈ 4 GB a month: not listed), 600 GB.
        let found = ingesters(&groups, &[70.0 * GB, 2.0 * GB, 600.0 * GB], 14, None);
        let names: Vec<&str> = found.iter().map(|f| f.resource.as_str()).collect();
        assert_eq!(names, ["/c", "/a"]);
        assert_eq!(found[0].impact, Impact::High);
        assert_eq!(found[1].impact, Impact::Medium);
        assert_eq!(found[1].reduction, "152 GB ingested a month");
        assert!(found.iter().all(|f| f.kind == "log-ingestion" && f.billed[0].usage == billing::LOGS_INGESTED));
    }

    #[test]
    fn lambda_groups_are_priced_as_vended_logs() {
        use crate::costs::UsageCost;
        let cw = |usage_type: &str, cost: f64, quantity: f64| UsageCost {
            service: "AmazonCloudWatch".into(),
            usage_type: usage_type.into(),
            cost,
            quantity,
            unit: "GB".into(),
        };
        // The bill has vended logs only (Lambda's), as the user's prod account does.
        let vended_only = Snapshot { usage: vec![cw("USE1-VendedLog-Bytes", 1275.93, 2556.5)], ..Default::default() };
        let groups = [group("/aws/lambda/orders", 1.0, Some(14)), group("/ecs/api", 1.0, Some(14))];
        let mut found = ingesters(&groups, &[300.0 * GB, 100.0 * GB], 14, Some(&vended_only));
        assert_eq!(found[0].billed[0].usage, billing::LOGS_VENDED);
        assert_eq!(found[1].billed[0].usage, billing::LOGS_INGESTED);
        assert!(found[0].recommendation.contains("application log level"));
        assert!(found[0].evidence.iter().any(|e| e.contains("vended")));
        // Priced at the bill's vended rate: about US$ 0.50 a GB.
        let mut lambda = Finding { service: "CloudWatch Logs", ..found.remove(0) };
        crate::savings::price::price_all(std::slice::from_mut(&mut lambda), Some(&vended_only), "us-east-1");
        let p = lambda.price.unwrap();
        let gb = 300.0 * per_month(14);
        assert!((p.monthly - gb * 1275.93 / 2556.5).abs() < 1e-6 && p.at_most, "{}", p.monthly);
        // A bill with plain ingestion only bills Lambda's logs that way.
        let plain_only = Snapshot { usage: vec![cw("DataProcessing-Bytes", 50.0, 100.0)], ..Default::default() };
        assert_eq!(ingestion_usage("/aws/lambda/x", Some(&plain_only)), billing::LOGS_INGESTED);
        assert_eq!(ingestion_usage("/aws/lambda/x", None), billing::LOGS_VENDED);
        assert_eq!(ingestion_usage("/aws/vendedlogs/states/x", None), billing::LOGS_VENDED);
        assert_eq!(ingestion_usage("/app/x", Some(&vended_only)), billing::LOGS_INGESTED);
    }
}

//! CloudWatch Logs: log groups kept forever, by size, and the groups that ingest the most.

use super::{Finding, Fix, GB, Impact, Scan, billing, metric_data, per_month, query, query_id, size, total};
use crate::aws::logs::{self, LogGroup};
use crate::aws::{self, Aws};
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
    found.extend(ingesters(&groups, &ingested, scan.days));
    Ok(found)
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

/// The groups that ingest the most.
fn ingesters(groups: &[LogGroup], ingested: &[f64], days: u32) -> Vec<Finding> {
    let mut busy: Vec<(&LogGroup, f64)> =
        groups.iter().zip(ingested).map(|(g, bytes)| (g, bytes * per_month(days))).filter(|(_, m)| *m >= INGESTED_LOW).collect();
    busy.sort_by(|a, b| b.1.total_cmp(&a.1));
    busy.into_iter()
        .take(TOP_INGESTERS)
        .map(|(g, monthly)| {
            let retention = g.retention_in_days.map_or("kept forever".to_string(), |d| format!("kept {d} days"));
            Finding::new(
                MODULE,
                &g.log_group_name,
                &g.log_group_name,
                format!("receives {} a month", size(monthly)),
                "Log less: drop debug lines and sample the noisiest ones",
                format!("{} ingested a month", size(monthly)),
                Impact::of(monthly, INGESTED_MEDIUM, INGESTED_HIGH),
            )
            .score(monthly / INGESTED_HIGH)
            .billed_at_most(billing::LOGS_INGESTED, monthly / GB)
            .evidence(format!("received {} in the last {days} days (IncomingBytes)", size(monthly / per_month(days))))
            .evidence(format!("{retention}, {} stored", size(stored(g))))
            .caveat("Ingestion is billed per GB received: only logging less lowers it.")
            .caveat("For logs searched only now and then, a new group in the Infrequent Access class bills less per GB (the class is set when a group is created, and it lacks some features).")
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
        let found = ingesters(&groups, &[70.0 * GB, 2.0 * GB, 600.0 * GB], 14);
        let names: Vec<&str> = found.iter().map(|f| f.resource.as_str()).collect();
        assert_eq!(names, ["/c", "/a"]);
        assert_eq!(found[0].impact, Impact::High);
        assert_eq!(found[1].impact, Impact::Medium);
        assert_eq!(found[1].reduction, "152 GB ingested a month");
    }
}

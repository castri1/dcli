//! Redshift: clusters idle for long stretches (candidates to pause on a schedule, or to
//! pause), nodes to spare, old manual snapshots, concurrency scaling beyond the free credits,
//! and previous-generation node types. No prices: a finding says what it removes in the unit
//! AWS bills (node-hours, GB of snapshots, hours of concurrency scaling).

use std::collections::HashMap;

use super::{Finding, Impact, Scan, per_month, qty};
use crate::aws::rds::Metrics;
use crate::aws::redshift::{self, Cluster, MetricDef, Scope, Unit, Want};
use crate::aws;
use crate::util::now_secs;

const MONTH_HOURS: f64 = 730.0;

// What is read for each cluster: hourly peaks, daily disk and scaling.
static CONNECTIONS_MAX: MetricDef =
    MetricDef { id: "connections_max", name: "DatabaseConnections", stat: "Maximum", label: "Connections", unit: Unit::Count, scope: Scope::Cluster };
static CPU_MAX: MetricDef = MetricDef { id: "cpu_max", name: "CPUUtilization", stat: "Maximum", label: "CPU", unit: Unit::Percent, scope: Scope::Cluster };
static DISK_MAX: MetricDef =
    MetricDef { id: "disk_max", name: "PercentageDiskSpaceUsed", stat: "Maximum", label: "Disk used", unit: Unit::Percent, scope: Scope::Cluster };
static SCALING: MetricDef =
    MetricDef { id: "scaling", name: "ConcurrencyScalingSeconds", stat: "Sum", label: "Concurrency scaling", unit: Unit::Count, scope: Scope::Cluster };

/// An hour is idle with no connection and CPU under this (%).
const IDLE_CPU: f64 = 5.0;
/// Idle this share of the hours or more: worth pausing on a schedule…
const IDLE_SHARE: f64 = 0.30;
/// …and this share: idle, full stop.
const ALL_IDLE_SHARE: f64 = 0.95;
/// An hour of the day idle on this share of days is part of the schedule's pause.
const IDLE_HOUR_DAYS: f64 = 0.8;
/// Fewer nodes when CPU and disk peaks stay under these (%)…
const SPARE_CPU: f64 = 30.0;
const SPARE_DISK: f64 = 40.0;
/// …keeping the peaks under this on the nodes left.
const TARGET_USE: f64 = 70.0;
const SNAPSHOT_DAYS: f64 = 90.0;
/// Concurrency scaling is free for an hour a day per cluster (credits).
const FREE_SCALING_HOURS_A_MONTH: f64 = 30.4;

/// Impact by node-hours a month: two nodes all month is high, half a node medium.
fn by_node_hours(h: f64) -> Impact {
    if h >= 1460.0 {
        Impact::High
    } else if h >= 365.0 {
        Impact::Medium
    } else {
        Impact::Low
    }
}

/// Impact by GB of snapshots: a terabyte is high, 100 GB medium.
fn by_gb(gb: f64) -> Impact {
    if gb >= 1000.0 {
        Impact::High
    } else if gb >= 100.0 {
        Impact::Medium
    } else {
        Impact::Low
    }
}

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let clusters = redshift::describe_clusters(scan.aws, None)?;
    if clusters.is_empty() {
        return Ok(Vec::new());
    }
    let days = scan.days.max(1);
    let now = now_secs();
    let start = now - f64::from(days) * 86_400.0;
    let live: Vec<&Cluster> = clusters.iter().filter(|c| !c.paused()).collect();
    let want = |defs: &[&'static MetricDef]| -> Vec<Want> {
        live.iter().flat_map(|c| defs.iter().map(|def| Want { cluster: c.id.clone(), node: None, def })).collect()
    };
    let (hourly, daily) = if live.is_empty() {
        (Metrics::new((start, now)), Metrics::new((start, now)))
    } else {
        (
            redshift::read_metrics(scan.aws, &want(&[&CONNECTIONS_MAX, &CPU_MAX]), start, now, 3600)?,
            redshift::read_metrics(scan.aws, &want(&[&DISK_MAX, &SCALING]), start, now, 86_400)?,
        )
    };
    // Snapshots are a bonus: without permission to list them, the other findings still stand.
    let snapshots = redshift::describe_snapshots(scan.aws, None).unwrap_or_default();
    let old = redshift::old_manual_snapshots(&snapshots, SNAPSHOT_DAYS, now);
    Ok(findings(&clusters, &hourly, &daily, &old, days))
}

fn findings(clusters: &[Cluster], hourly: &Metrics, daily: &Metrics, old: &HashMap<String, (f64, usize, f64)>, days: u32) -> Vec<Finding> {
    let mut out = Vec::new();
    for c in clusters {
        if !c.paused() {
            let idle = idle(c, hourly, days);
            let all_idle = idle.as_ref().is_some_and(|(share, _)| *share >= ALL_IDLE_SHARE);
            out.extend(idle.map(|(_, f)| f));
            // An idle cluster is not made smaller: it is paused.
            if !all_idle {
                out.extend(spare_nodes(c, hourly, daily));
            }
            out.extend(scaling(c, daily, days));
        }
        out.extend(previous_generation(c));
        if let Some(&(mb, count, oldest)) = old.get(&c.id) {
            out.push(old_snapshots(&c.id, mb, count, oldest));
        }
    }
    out
}

/// Hours with no connection and almost no CPU: pause on a schedule, or pause.
fn idle(c: &Cluster, m: &Metrics, days: u32) -> Option<(f64, Finding)> {
    let connections = m.points(&c.id, "connections_max");
    if (connections.len() as f64) < f64::from(days) * 24.0 / 2.0 {
        return None;
    }
    let cpu: HashMap<i64, f64> = m.points(&c.id, "cpu_max").iter().map(|(t, v)| (*t as i64, *v)).collect();
    let idle_at: Vec<f64> = connections.iter().filter(|(t, v)| *v == 0.0 && cpu.get(&(*t as i64)).is_none_or(|c| *c < IDLE_CPU)).map(|(t, _)| *t).collect();
    let share = idle_at.len() as f64 / connections.len() as f64;
    if share < IDLE_SHARE {
        return None;
    }
    let nodes = f64::from(c.nodes_count);
    let mut evidence = vec![format!(
        "{} of {} hours with no connection and CPU under {IDLE_CPU:.0}% ({:.0}%)",
        idle_at.len(),
        connections.len(),
        share * 100.0
    )];
    if let Some(hours) = idle_hours(connections, &idle_at, days) {
        evidence.push(format!("idle every day at {hours} (UTC)"));
    }
    let weekend = |t: f64| matches!(((t / 86_400.0).floor() as i64 + 4).rem_euclid(7), 0 | 6);
    let weekend_hours = connections.iter().filter(|(t, _)| weekend(*t)).count();
    if weekend_hours >= 24 {
        let idle_weekend = idle_at.iter().filter(|t| weekend(**t)).count();
        evidence.push(format!("weekends: idle {:.0}% of the hours", idle_weekend as f64 / weekend_hours as f64 * 100.0));
    }
    let f = if share >= ALL_IDLE_SHARE {
        let hours = nodes * MONTH_HOURS;
        Finding::new(
            "Redshift",
            c.id.clone(),
            c.id.clone(),
            format!("no connections in {days} days ({})", c.shape()),
            "Pause it (p in Redshift) while nothing needs it, or take a final snapshot and delete it",
            format!("{} node-hours a month of {}", qty(hours), c.node_type),
            by_node_hours(hours).max(Impact::Medium),
        )
        .score(hours)
    } else {
        let hours = nodes * MONTH_HOURS * share;
        Finding::new(
            "Redshift",
            c.id.clone(),
            c.id.clone(),
            format!("idle {:.0}% of the hours ({})", share * 100.0, c.shape()),
            "Pause it on a schedule during the hours it is idle (Redshift scheduled actions: pause and resume)",
            format!("up to {} node-hours a month of {}", qty(hours), c.node_type),
            by_node_hours(hours),
        )
        .score(hours)
    };
    let mut f = f;
    for e in evidence {
        f = f.evidence(e);
    }
    f = f
        .caveat("Pausing stops running queries and closes connections; resuming takes a few minutes.")
        .caveat("While paused, storage and snapshots are still billed; only the nodes stop.")
        .caveat("Scheduled pause and resume need an IAM role Redshift's scheduler can assume.");
    Some((share, f))
}

/// The hours of the day (UTC) idle on most days, as ranges: `00:00–07:00, 20:00–24:00`.
fn idle_hours(observed: &[(f64, f64)], idle_at: &[f64], days: u32) -> Option<String> {
    let hour = |t: f64| ((t / 3600.0).floor() as i64).rem_euclid(24) as usize;
    let mut seen = [0usize; 24];
    let mut idle = [0usize; 24];
    for (t, _) in observed {
        seen[hour(*t)] += 1;
    }
    for t in idle_at {
        idle[hour(*t)] += 1;
    }
    let min_days = (f64::from(days) / 2.0).max(1.0);
    let quiet: Vec<bool> = (0..24).map(|h| seen[h] as f64 >= min_days && idle[h] as f64 >= seen[h] as f64 * IDLE_HOUR_DAYS).collect();
    if !quiet.iter().any(|q| *q) || quiet.iter().all(|q| *q) {
        return None;
    }
    let mut ranges = Vec::new();
    let mut h = 0;
    while h < 24 {
        if quiet[h] {
            let start = h;
            while h < 24 && quiet[h] {
                h += 1;
            }
            ranges.push(format!("{start:02}:00–{h:02}:00"));
        } else {
            h += 1;
        }
    }
    Some(ranges.join(", "))
}

/// CPU and disk peaks low enough for fewer nodes.
fn spare_nodes(c: &Cluster, hourly: &Metrics, daily: &Metrics) -> Option<Finding> {
    if c.nodes_count < 3 {
        return None;
    }
    let cpu = hourly.max(&c.id, "cpu_max")?;
    let disk = daily.max(&c.id, "disk_max")?;
    if cpu >= SPARE_CPU || disk >= SPARE_DISK {
        return None;
    }
    let nodes = f64::from(c.nodes_count);
    let needed = (nodes * cpu.max(disk) / TARGET_USE).ceil().max(2.0);
    if needed >= nodes {
        return None;
    }
    let fewer = nodes - needed;
    let hours = fewer * MONTH_HOURS;
    let f = Finding::new(
        "Redshift",
        c.id.clone(),
        c.id.clone(),
        format!("{} nodes peaked at {cpu:.0}% CPU and {disk:.0}% disk", c.nodes_count),
        format!("Resize to {} × {needed:.0} (R in Redshift lists the counts elastic resize allows)", c.node_type),
        format!("{} node-hours a month of {} ({} → {needed:.0} nodes)", qty(hours), c.node_type, c.nodes_count),
        by_node_hours(hours),
    )
    .score(hours)
    .evidence(format!("highest hourly CPU in the window: {cpu:.0}%"))
    .evidence(format!("highest daily disk use: {disk:.0}%"))
    .evidence(format!("on {needed:.0} nodes those would be about {:.0}% and {:.0}%", cpu * nodes / needed, disk * nodes / needed))
    .caveat("Fewer nodes also means less memory and fewer slices for concurrent queries: watch the WLM queue after resizing.")
    .caveat("Elastic resize allows only some node counts; a classic resize takes hours.");
    Some(if c.ra3() { f.caveat("On RA3 the data lives in managed storage, so local disk use limits less than CPU.") } else { f })
}

/// Concurrency scaling beyond the free hour a day.
fn scaling(c: &Cluster, daily: &Metrics, days: u32) -> Option<Finding> {
    let monthly = daily.sum(&c.id, "scaling") / 3600.0 * per_month(days);
    let beyond = monthly - FREE_SCALING_HOURS_A_MONTH;
    if beyond < 10.0 {
        return None;
    }
    let impact = if beyond >= 100.0 {
        Impact::High
    } else if beyond >= 30.0 {
        Impact::Medium
    } else {
        Impact::Low
    };
    Some(
        Finding::new(
            "Redshift",
            c.id.clone(),
            c.id.clone(),
            format!("concurrency scaling ran about {} hours a month", qty(monthly)),
            "See which queries queue at the peaks (Queries tab) and spread or tune them, or cap concurrency scaling in the WLM queues",
            format!("about {} hours a month of concurrency scaling beyond the free credits", qty(beyond)),
            impact,
        )
        .score(beyond)
        .evidence(format!("{} seconds of ConcurrencyScalingSeconds in {days} days", qty(daily.sum(&c.id, "scaling"))))
        .caveat("Each cluster earns an hour of free concurrency scaling a day (credits accrue up to 30 hours); beyond that it is billed per second.")
        .caveat("Scaling keeps queries from queueing at peaks: less of it can mean longer waits."),
    )
}

/// DC2 and DS2 nodes: RA3 separates compute from storage.
fn previous_generation(c: &Cluster) -> Option<Finding> {
    if !(c.node_type.starts_with("dc2") || c.node_type.starts_with("ds2")) {
        return None;
    }
    let hours = f64::from(c.nodes_count) * MONTH_HOURS;
    Some(
        Finding::new(
            "Redshift",
            c.id.clone(),
            c.id.clone(),
            format!("{}: a previous-generation node type", c.shape()),
            "Consider RA3 nodes: resize to an RA3 type (R), or restore a snapshot into an RA3 cluster and switch over",
            format!("{} node-hours a month on {} instead of RA3", qty(hours), c.node_type),
            if c.nodes_count >= 4 { Impact::Medium } else { Impact::Low },
        )
        .score(hours)
        .caveat("RA3 bills its managed storage apart from the nodes: compare the whole configuration before moving.")
        .caveat("A resize between node families can take hours; test the workload on RA3 first."),
    )
}

fn old_snapshots(cluster: &str, mb: f64, count: usize, oldest: f64) -> Finding {
    let gb = mb / 1024.0;
    Finding::new(
        "Redshift",
        cluster,
        format!("{cluster} snapshots"),
        format!("{count} manual snapshot{} older than {SNAPSHOT_DAYS:.0} days, the oldest {oldest:.0} days", if count == 1 { "" } else { "s" }),
        "Delete the snapshots no longer needed, or give them a retention period",
        format!("{} GB of snapshot storage a month", qty(gb)),
        by_gb(gb),
    )
    .score(gb)
    .caveat("A manual snapshot may be the only copy someone counts on: check before deleting it.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(id: &str, node_type: &str, nodes: u32) -> Cluster {
        Cluster { id: id.into(), node_type: node_type.into(), nodes_count: nodes, status: "available".into(), ..Default::default() }
    }

    /// Hourly points over `days` days from Monday 2026-10-19 00:00 UTC; `busy(hour of day)`.
    fn hourly(id: &str, days: u32, busy: impl Fn(usize) -> bool) -> Metrics {
        let start = 1_792_368_000.0;
        let mut m = Metrics::new((start, start + f64::from(days) * 86_400.0));
        let hours = (0..days * 24).map(|h| start + f64::from(h) * 3600.0);
        let conn: Vec<(f64, f64)> = hours.clone().map(|t| (t, if busy(((t / 3600.0) as usize) % 24) { 12.0 } else { 0.0 })).collect();
        let cpu: Vec<(f64, f64)> = hours.map(|t| (t, if busy(((t / 3600.0) as usize) % 24) { 40.0 } else { 1.0 })).collect();
        m.insert(id, "connections_max", conn);
        m.insert(id, "cpu_max", cpu);
        m
    }

    #[test]
    fn idle_nights_are_a_schedule_and_idle_days_a_pause() {
        let c = cluster("dev", "ra3.xlplus", 2);
        let m = hourly("dev", 14, |h| (8..20).contains(&h));
        let (share, f) = idle(&c, &m, 14).expect("idle half the time");
        assert!((share - 0.5).abs() < 1e-9);
        assert!(f.summary.starts_with("idle 50% of the hours"), "{f:?}");
        assert_eq!(f.reduction, "up to 730 node-hours a month of ra3.xlplus");
        assert_eq!(f.impact, Impact::Medium);
        assert!(f.evidence.iter().any(|e| e == "idle every day at 00:00–08:00, 20:00–24:00 (UTC)"), "{:?}", f.evidence);
        let m = hourly("dev", 14, |_| false);
        let (_, f) = idle(&c, &m, 14).unwrap();
        assert!(f.summary.starts_with("no connections in 14 days"));
        assert_eq!(f.reduction, "1.5 k node-hours a month of ra3.xlplus");
        assert_eq!(f.impact, Impact::High);
        // Busy all the time, or too little data: nothing.
        assert!(idle(&c, &hourly("dev", 14, |_| true), 14).is_none());
        assert!(idle(&c, &hourly("dev", 3, |_| false), 14).is_none());
    }

    #[test]
    fn spare_nodes_keep_peaks_under_the_target() {
        let c = cluster("prod", "ra3.4xlarge", 8);
        let mut hourly = Metrics::new((0.0, 1.0));
        hourly.insert("prod", "cpu_max", vec![(0.0, 20.0), (1.0, 25.0)]);
        let mut daily = Metrics::new((0.0, 1.0));
        daily.insert("prod", "disk_max", vec![(0.0, 30.0)]);
        let f = spare_nodes(&c, &hourly, &daily).expect("8 nodes at 30% peak");
        // 8 × 30 / 70 = 3.4 → 4 nodes.
        assert!(f.recommendation.starts_with("Resize to ra3.4xlarge × 4"), "{f:?}");
        assert_eq!(f.reduction, "2.9 k node-hours a month of ra3.4xlarge (8 → 4 nodes)");
        assert_eq!(f.impact, Impact::High);
        daily.insert("prod", "disk_max", vec![(0.0, 55.0)]);
        assert!(spare_nodes(&c, &hourly, &daily).is_none(), "disk too full");
        assert!(spare_nodes(&cluster("s", "ra3.xlplus", 2), &hourly, &daily).is_none(), "two nodes stay");
    }

    #[test]
    fn scaling_beyond_free_credits_and_old_types() {
        let c = cluster("prod", "ra3.4xlarge", 4);
        let mut daily = Metrics::new((0.0, 1.0));
        // 14 days of 2 hours a day: about 61 hours a month, 30 free.
        daily.insert("prod", "scaling", (0..14).map(|d| (f64::from(d), 7200.0)).collect());
        let f = scaling(&c, &daily, 14).expect("beyond the free hour");
        assert_eq!(f.reduction, "about 30 hours a month of concurrency scaling beyond the free credits");
        assert_eq!(f.impact, Impact::Medium);
        daily.insert("prod", "scaling", vec![(0.0, 3600.0)]);
        assert!(scaling(&c, &daily, 14).is_none());
        let old = previous_generation(&cluster("legacy", "dc2.large", 2)).unwrap();
        assert_eq!(old.reduction, "1.5 k node-hours a month on dc2.large instead of RA3");
        assert!(previous_generation(&c).is_none());
        let s = old_snapshots("legacy", 2048.0 * 600.0, 3, 400.0);
        assert_eq!((s.reduction.as_str(), s.impact), ("1.2 k GB of snapshot storage a month", Impact::High));
        assert!(s.summary.starts_with("3 manual snapshots older than 90 days"));
    }
}

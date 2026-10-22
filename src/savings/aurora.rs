//! Aurora: idle clusters, readers barely used, instances a size too big, Serverless v2
//! minimum capacity above use, Intel classes that could be Graviton, Standard storage with
//! heavy I/O (an I/O-Optimized candidate), and old manual snapshots. No prices: a finding says
//! what it removes in the unit AWS bills (instance-hours and their vCPUs, ACU-hours, I/O
//! requests, GB of snapshot storage).

use std::collections::BTreeMap;

use super::{Finding, Impact, Scan};
use crate::aws::rds::{self, Cluster, Instance, Inventory, MetricDef, Metrics, Only, Snapshot, Unit, Want};
use crate::aws;
use crate::util::now_secs;

const MONTH_HOURS: f64 = 730.0;

// What is read for each instance and cluster: one value per day.
static CONNECTIONS_MAX: MetricDef =
    MetricDef { id: "connections_max", name: "DatabaseConnections", stat: "Maximum", label: "Connections", unit: Unit::Count, only: Only::All };
static CPU_MAX: MetricDef = MetricDef { id: "cpu_max", name: "CPUUtilization", stat: "Maximum", label: "CPU", unit: Unit::Percent, only: Only::All };
static MEMORY_MIN: MetricDef =
    MetricDef { id: "memory_min", name: "FreeableMemory", stat: "Minimum", label: "Freeable memory", unit: Unit::Bytes, only: Only::All };
static ACU_AVG: MetricDef =
    MetricDef { id: "acu_avg", name: "ServerlessDatabaseCapacity", stat: "Average", label: "Capacity", unit: Unit::Acu, only: Only::Serverless };
static IO_READS: MetricDef = MetricDef { id: "io_reads", name: "VolumeReadIOPs", stat: "Sum", label: "Billed read I/Os", unit: Unit::Count, only: Only::All };
static IO_WRITES: MetricDef =
    MetricDef { id: "io_writes", name: "VolumeWriteIOPs", stat: "Sum", label: "Billed write I/Os", unit: Unit::Count, only: Only::All };

/// A reader that never went above this CPU (%) and one connection is barely used.
const QUIET_READER_CPU: f64 = 10.0;
/// An instance is a size too big when, on the next class down, its CPU peak would stay
/// under this (%)…
const SMALLER_CPU_MAX: f64 = 75.0;
/// …and the memory the smaller class lacks was free, plus this share of the smaller one.
const SMALLER_MEMORY_MARGIN: f64 = 0.10;
/// Serverless v2: the capacity that keeps CPU at this share is what the cluster needs.
const ACU_CPU_TARGET: f64 = 0.70;
/// Billed I/O requests a month from which Standard storage is worth comparing with
/// I/O-Optimized, and from which it matters a lot.
const IO_MEDIUM: f64 = 1e9;
const IO_HIGH: f64 = 5e9;
/// Manual snapshots older than this many days.
const SNAPSHOT_DAYS: f64 = 90.0;

/// Impact by vCPU-hours a month removed (or moved): a 4-vCPU instance running all month
/// (2,920) is high, a third of that is medium.
fn by_vcpu_hours(h: f64) -> Impact {
    if h >= 2920.0 {
        Impact::High
    } else if h >= 1000.0 {
        Impact::Medium
    } else {
        Impact::Low
    }
}

/// Impact by ACU-hours a month: 2 ACU all month is high, half an ACU medium.
fn by_acu_hours(h: f64) -> Impact {
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

/// `2,190`
fn num(v: f64) -> String {
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

/// `17.5 billion`, `582 million`
fn big(v: f64) -> String {
    if v >= 1e9 {
        format!("{:.1} billion", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.0} million", v / 1e6)
    } else {
        num(v)
    }
}

fn gib(bytes: f64) -> f64 {
    bytes / (1024.0 * 1024.0 * 1024.0)
}

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let inventory = rds::inventory(scan.aws, None)?;
    if inventory.clusters.is_empty() {
        return Ok(Vec::new());
    }
    let days = f64::from(scan.days.max(1));
    let now = now_secs();
    let mut wants = Vec::new();
    for i in &inventory.instances {
        let defs: &[&'static MetricDef] = if i.serverless() { &[&CONNECTIONS_MAX, &CPU_MAX, &MEMORY_MIN, &ACU_AVG] } else { &[&CONNECTIONS_MAX, &CPU_MAX, &MEMORY_MIN] };
        wants.extend(defs.iter().map(|def| Want { source: i.id.clone(), cluster: false, def }));
    }
    for c in &inventory.clusters {
        wants.extend([&IO_READS, &IO_WRITES].map(|def| Want { source: c.id.clone(), cluster: true, def }));
    }
    let metrics = rds::read_metrics(scan.aws, &wants, now - days * 86_400.0, now, 86_400)?;
    // Snapshots are a bonus: without permission to list them, the other findings still stand.
    let snapshots = rds::manual_snapshots(scan.aws).unwrap_or_default();
    Ok(findings(&inventory, &metrics, &snapshots, days, now))
}

/// What the usage of `inventory` over `days` (daily values in `m`) says could cost less.
fn findings(inventory: &Inventory, m: &Metrics, snapshots: &[Snapshot], days: f64, now: f64) -> Vec<Finding> {
    let mut out = Vec::new();
    for c in &inventory.clusters {
        let instances = inventory.instances_of(&c.id);
        if instances.is_empty() {
            continue;
        }
        // Enough days of data to say something.
        let seen = |i: &Instance| m.points(&i.id, "connections_max").len() as f64 >= (days / 2.0).max(1.0);
        if instances.iter().all(|i| seen(i) && m.max(&i.id, "connections_max") == Some(0.0)) {
            out.push(idle_cluster(c, &instances, m, days));
            continue;
        }
        for i in &instances {
            if !seen(i) {
                continue;
            }
            if let Some(f) = quiet_reader(c, &instances, i, m, days) {
                out.push(f);
                continue;
            }
            out.extend(oversized(c, &instances, i, m, days));
            out.extend(serverless_minimum(c, i, m, days));
        }
        out.extend(graviton(c, &instances));
        out.extend(io_heavy(c, m, days));
    }
    out.extend(old_snapshots(snapshots, now));
    out
}

/// What an instance runs a month: instance-hours with their vCPUs, or ACU-hours at the
/// Serverless minimum.
fn monthly_use(c: &Cluster, i: &Instance) -> (String, Impact) {
    if i.serverless() {
        let min = c.serverless.as_ref().map_or(0.5, |s| s.min);
        let hours = min * MONTH_HOURS;
        return (format!("{} ACU-hours a month at least ({} ACU minimum)", num(hours), rds::fmt_acu(min)), by_acu_hours(hours));
    }
    match rds::class_specs(&i.class) {
        Some((vcpus, _)) => {
            let h = f64::from(vcpus) * MONTH_HOURS;
            (format!("{} instance-hours a month of {} ({} vCPU-hours)", num(MONTH_HOURS), i.class, num(h)), by_vcpu_hours(h))
        }
        None => (format!("{} instance-hours a month of {}", num(MONTH_HOURS), i.class), Impact::Medium),
    }
}

fn idle_cluster(c: &Cluster, instances: &[&Instance], m: &Metrics, days: f64) -> Finding {
    let uses: Vec<(String, Impact)> = instances.iter().map(|i| monthly_use(c, i)).collect();
    let impact = if instances.len() > 1 { Impact::High } else { uses[0].1.max(Impact::Medium) };
    let reduction = if uses.len() == 1 { uses[0].0.clone() } else { format!("{} instances: {}", uses.len(), uses.iter().map(|u| u.0.as_str()).collect::<Vec<_>>().join("; ")) };
    let mut f = Finding::new(
        "Aurora",
        c.id.clone(),
        c.id.clone(),
        if instances.len() == 1 {
            format!("no connections in {days:.0} days on its only instance")
        } else {
            format!("no connections in {days:.0} days on its {} instances", instances.len())
        },
        "delete it after a final snapshot, or stop it while it is not needed (a stopped cluster starts again after 7 days)",
        reduction,
        impact,
    )
    .evidence(format!("highest daily connection count over {days:.0} days: 0 on {}", instances.iter().map(|i| i.id.as_str()).collect::<Vec<_>>().join(", ")));
    if let Some(cpu) = instances.iter().filter_map(|i| m.max(&i.id, "cpu_max")).reduce(f64::max) {
        f = f.evidence(format!("CPU peaked at {cpu:.0}% (background work only)"));
    }
    f.caveat("Connections are sampled every minute: a job that connects briefly now and then can be missed. Check who uses it before deleting.")
        .caveat("Storage and backups are billed while it is stopped.")
}

fn quiet_reader(c: &Cluster, instances: &[&Instance], i: &Instance, m: &Metrics, days: f64) -> Option<Finding> {
    if i.writer {
        return None;
    }
    let connections = m.max(&i.id, "connections_max")?;
    let cpu = m.max(&i.id, "cpu_max")?;
    if connections > 1.0 || cpu >= QUIET_READER_CPU {
        return None;
    }
    let readers = instances.iter().filter(|r| !r.writer).count();
    let (reduction, impact) = monthly_use(c, i);
    let (recommendation, impact) = if readers > 1 {
        (format!("remove {}: the other readers keep serving reads and failing over", i.id), impact)
    } else {
        (format!("remove {} if the cluster can do without a failover target", i.id), Impact::Low)
    };
    Some(
        Finding::new("Aurora", c.id.clone(), format!("{} / {}", c.id, i.id), format!("reader {} barely used in {days:.0} days", i.id), recommendation, reduction, impact)
            .evidence(format!("at most {connections:.0} connection{} a day", if connections == 1.0 { "" } else { "s" }))
            .evidence(format!("CPU peaked at {cpu:.0}%"))
            .caveat("Readers are failover targets: without one, a failover has to create a new writer and takes longer.")
            .caveat("The reader endpoint spreads connections over the readers: check that applications do not pin this one by name."),
    )
}

fn oversized(c: &Cluster, instances: &[&Instance], i: &Instance, m: &Metrics, days: f64) -> Option<Finding> {
    if i.serverless() {
        return None;
    }
    let (vcpus, memory) = rds::class_specs(&i.class)?;
    let smaller = rds::smaller_class(&i.class)?;
    let (new_vcpus, new_memory) = rds::class_specs(&smaller)?;
    let cpu = m.max(&i.id, "cpu_max")?;
    let free = gib(m.min(&i.id, "memory_min")?);
    let projected = cpu * f64::from(vcpus) / f64::from(new_vcpus);
    let needed = memory - new_memory + new_memory * SMALLER_MEMORY_MARGIN;
    if projected >= SMALLER_CPU_MAX || free < needed {
        return None;
    }
    let hours = f64::from(vcpus - new_vcpus) * MONTH_HOURS;
    let mut f = Finding::new(
        "Aurora",
        c.id.clone(),
        format!("{} / {}", c.id, i.id),
        format!("{} peaked at {cpu:.0}% CPU with at least {free:.0} of {memory:.0} GiB free in {days:.0} days", i.id),
        format!("move {} from {} to {smaller}", i.id, i.class),
        format!("{} vCPU-hours a month ({vcpus} → {new_vcpus} vCPUs, {memory:.0} → {new_memory:.0} GiB)", num(hours)),
        by_vcpu_hours(hours),
    )
    .evidence(format!("on {smaller} the same work would peak near {projected:.0}% CPU"))
    .caveat("These are daily peaks of 1-minute averages: shorter bursts do not show.")
    .caveat("Free memory also caches data: with less of it, more reads go to storage.");
    if instances.len() > 1 {
        f = f.caveat("The writer and the readers fail over to each other: keep classes that can carry the writer's load.");
    }
    Some(f)
}

fn serverless_minimum(c: &Cluster, i: &Instance, m: &Metrics, days: f64) -> Option<Finding> {
    if !i.serverless() {
        return None;
    }
    let range = c.serverless.as_ref()?;
    let capacity = m.points(&i.id, "acu_avg");
    let used = capacity.iter().map(|p| p.1).reduce(f64::max)?;
    let cpu = m.max(&i.id, "cpu_max")?;
    let need = used * cpu / 100.0 / ACU_CPU_TARGET;
    let new_min = ((need * 2.0).ceil() / 2.0).max(0.5);
    if new_min >= range.min {
        return None;
    }
    // Hours spent at the minimum: days whose average capacity stayed at it.
    let at_min = capacity.iter().filter(|p| p.1 <= range.min * 1.05).count() as f64 / capacity.len() as f64;
    let acu_hours = (range.min - new_min) * MONTH_HOURS * at_min;
    if acu_hours < 1.0 {
        return None;
    }
    Some(
        Finding::new(
            "Aurora",
            c.id.clone(),
            format!("{} / {}", c.id, i.id),
            format!("{} stays at its {} ACU minimum on {:.0}% of days with CPU at most {cpu:.0}%", i.id, rds::fmt_acu(range.min), at_min * 100.0),
            format!("lower the minimum capacity of {} from {} to {} ACU", c.id, rds::fmt_acu(range.min), rds::fmt_acu(new_min)),
            format!("up to {} ACU-hours a month ({} → {} ACU while quiet)", num(acu_hours), rds::fmt_acu(range.min), rds::fmt_acu(new_min)),
            by_acu_hours(acu_hours),
        )
        .evidence(format!("capacity averaged at most {used:.1} ACU a day over {days:.0} days"))
        .evidence(format!("about {need:.1} ACU would keep CPU under {:.0}%", ACU_CPU_TARGET * 100.0))
        .caveat("Each ACU brings about 2 GiB of memory: a lower minimum caches less while quiet, so the first queries after a calm period can be slower.")
        .caveat("Recent engine versions can also pause at 0 ACU, for clusters that can wait a few seconds to resume."),
    )
}

fn graviton(c: &Cluster, instances: &[&Instance]) -> Option<Finding> {
    let moves: Vec<(&Instance, String)> = instances.iter().filter_map(|i| Some((*i, rds::graviton_class(&i.class)?))).collect();
    if moves.is_empty() {
        return None;
    }
    let vcpu_hours: f64 = moves.iter().filter_map(|(i, _)| rds::class_specs(&i.class)).map(|(v, _)| f64::from(v) * MONTH_HOURS).sum();
    let list = moves.iter().map(|(i, to)| format!("{} {} → {to}", i.id, i.class)).collect::<Vec<_>>().join(", ");
    Some(
        Finding::new(
            "Aurora",
            c.id.clone(),
            c.id.clone(),
            if moves.len() == 1 { "1 instance on an Intel class".to_string() } else { format!("{} instances on Intel classes", moves.len()) },
            format!("move to Graviton: {list}"),
            format!("{} vCPU-hours a month on Graviton instead of Intel", num(vcpu_hours)),
            if vcpu_hours >= 2920.0 { Impact::Medium } else { Impact::Low },
        )
        .evidence(format!("engine {}", c.engine_label()))
        .caveat("Check that the engine version offers the class: older versions (Aurora MySQL 2, for one) need an upgrade first.")
        .caveat("Changing the class restarts the instance; do the readers first, then fail over."),
    )
}

fn io_heavy(c: &Cluster, m: &Metrics, days: f64) -> Option<Finding> {
    if c.io_optimized() || !(m.has(&c.id, "io_reads") || m.has(&c.id, "io_writes")) {
        return None;
    }
    let (reads, writes) = (m.sum(&c.id, "io_reads"), m.sum(&c.id, "io_writes"));
    let monthly = (reads + writes) * 30.0 / days;
    if monthly < IO_MEDIUM {
        return None;
    }
    Some(
        Finding::new(
            "Aurora",
            c.id.clone(),
            c.id.clone(),
            format!("Standard storage billed {} I/O requests in {days:.0} days", big(reads + writes)),
            "compare with I/O-Optimized storage, which bills no I/O requests",
            format!("about {} I/O requests a month no longer billed per request", big(monthly)),
            if monthly >= IO_HIGH { Impact::High } else { Impact::Medium },
        )
        .evidence(format!("{} reads and {} writes (VolumeReadIOPs, VolumeWriteIOPs)", big(reads), big(writes)))
        .caveat("I/O-Optimized charges more for instances and storage instead: it pays off when I/O is a large part of this cluster's bill. Compare those lines of the bill before switching.")
        .caveat("The storage type can be changed once every 30 days."),
    )
}

fn old_snapshots(snapshots: &[Snapshot], now: f64) -> Vec<Finding> {
    let mut by_cluster: BTreeMap<&str, Vec<&Snapshot>> = BTreeMap::new();
    for s in snapshots {
        if s.created.is_some_and(|t| now - t >= SNAPSHOT_DAYS * 86_400.0) && s.status == "available" {
            by_cluster.entry(s.cluster.as_str()).or_default().push(s);
        }
    }
    by_cluster
        .into_iter()
        .map(|(cluster, list)| {
            let gb: f64 = list.iter().filter_map(|s| s.allocated).map(|g| g as f64).sum();
            let oldest = list.iter().filter_map(|s| s.created).fold(now, f64::min);
            let mut f = Finding::new(
                "Aurora",
                cluster,
                format!("{cluster} snapshots"),
                format!(
                    "{} manual snapshot{} older than {SNAPSHOT_DAYS:.0} days, the oldest {:.0} days",
                    list.len(),
                    if list.len() == 1 { "" } else { "s" },
                    (now - oldest) / 86_400.0
                ),
                "delete the snapshots no longer needed",
                format!("{} GB of snapshot storage a month", num(gb)),
                by_gb(gb),
            );
            for s in list {
                let size = s.allocated.map(|g| format!(", {g} GB")).unwrap_or_default();
                let age = s.created.map(|t| format!(", {:.0} days old", (now - t) / 86_400.0)).unwrap_or_default();
                f = f.evidence(format!("{}{size}{age}", s.id));
            }
            f.caveat("Snapshots are often kept on purpose (compliance, before a migration).")
                .caveat("A snapshot is billed for the data it holds, which can be less than its allocated size.")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::rds::{Capacity, Member};

    const DAY: f64 = 86_400.0;

    fn daily(m: &mut Metrics, source: &str, id: &str, values: &[f64]) {
        m.insert(source, id, values.iter().enumerate().map(|(d, v)| (d as f64 * DAY, *v)).collect());
    }

    fn cluster(id: &str, instances: &[(&str, &str, bool)]) -> (Cluster, Vec<Instance>) {
        let c = Cluster {
            id: id.into(),
            engine: "aurora-postgresql".into(),
            engine_version: "16.4".into(),
            status: "available".into(),
            storage_type: Some("aurora".into()),
            members: instances.iter().map(|(i, _, w)| Member { instance: i.to_string(), writer: *w, ..Default::default() }).collect(),
            ..Default::default()
        };
        let list = instances
            .iter()
            .map(|(i, class, w)| Instance { id: i.to_string(), cluster: id.into(), class: class.to_string(), status: "available".into(), writer: *w, ..Default::default() })
            .collect();
        (c, list)
    }

    #[test]
    fn idle_clusters_are_one_finding() {
        let (c, instances) = cluster("old", &[("old-1", "db.r5.large", true)]);
        let inventory = Inventory { clusters: vec![c], instances, endpoints: Vec::new() };
        let mut m = Metrics::new((0.0, 14.0 * DAY));
        daily(&mut m, "old-1", "connections_max", &[0.0; 14]);
        daily(&mut m, "old-1", "cpu_max", &[2.0; 14]);
        daily(&mut m, "old-1", "memory_min", &[13.0 * 1024.0 * 1024.0 * 1024.0; 14]);
        let found = findings(&inventory, &m, &[], 14.0, 14.0 * DAY);
        assert_eq!(found.len(), 1, "idle comes alone (not Graviton too): {found:#?}");
        let f = &found[0];
        assert_eq!((f.module, f.target.as_str(), f.impact), ("Aurora", "old", Impact::Medium));
        assert_eq!(f.reduction, "730 instance-hours a month of db.r5.large (1,460 vCPU-hours)");
        assert!(f.summary.starts_with("no connections in 14 days"));
    }

    #[test]
    fn busy_clusters_get_sizing_storage_and_class_findings() {
        let (mut c, instances) = cluster("orders", &[("w", "db.r6g.xlarge", true), ("r1", "db.r6g.xlarge", false), ("r2", "db.r5.large", false)]);
        c.storage_type = Some("aurora".into());
        let inventory = Inventory { clusters: vec![c], instances, endpoints: Vec::new() };
        let mut m = Metrics::new((0.0, 14.0 * DAY));
        let gb = |g: f64| g * 1024.0 * 1024.0 * 1024.0;
        for (id, conns, cpu, free) in [("w", 300.0, 90.0, gb(8.0)), ("r1", 60.0, 30.0, gb(20.0)), ("r2", 10.0, 70.0, gb(9.0))] {
            daily(&mut m, id, "connections_max", &[conns; 14]);
            daily(&mut m, id, "cpu_max", &[cpu; 14]);
            daily(&mut m, id, "memory_min", &[free; 14]);
        }
        daily(&mut m, "orders", "io_reads", &[4e8; 14]);
        daily(&mut m, "orders", "io_writes", &[1e8; 14]);
        let found = findings(&inventory, &m, &[], 14.0, 14.0 * DAY);
        let about: Vec<(&str, &str)> = found.iter().map(|f| (f.resource.as_str(), f.recommendation.as_str())).collect();
        assert_eq!(found.len(), 3, "{about:#?}");
        let smaller = &found[0];
        assert_eq!(smaller.recommendation, "move r1 from db.r6g.xlarge to db.r6g.large");
        assert_eq!((smaller.reduction.as_str(), smaller.impact), ("1,460 vCPU-hours a month (4 → 2 vCPUs, 32 → 16 GiB)", Impact::Medium));
        assert_eq!(found[1].recommendation, "move to Graviton: r2 db.r5.large → db.r7g.large");
        assert_eq!(found[1].impact, Impact::Low);
        let io = &found[2];
        assert_eq!(io.reduction, "about 15.0 billion I/O requests a month no longer billed per request");
        assert_eq!(io.impact, Impact::High);
        assert!(found.iter().all(|f| !f.reduction.contains('$') && !f.reduction.contains('%')));
    }

    #[test]
    fn serverless_minimum_above_use() {
        let (mut c, instances) = cluster("sessions", &[("s-1", "db.serverless", true)]);
        c.serverless = Some(Capacity { min: 4.0, max: 16.0 });
        let inventory = Inventory { clusters: vec![c], instances, endpoints: Vec::new() };
        let mut m = Metrics::new((0.0, 14.0 * DAY));
        daily(&mut m, "s-1", "connections_max", &[6.0; 14]);
        daily(&mut m, "s-1", "cpu_max", &[10.0; 14]);
        daily(&mut m, "s-1", "memory_min", &[gib_bytes(6.0); 14]);
        daily(&mut m, "s-1", "acu_avg", &[4.0; 14]);
        let found = findings(&inventory, &m, &[], 14.0, 14.0 * DAY);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].recommendation, "lower the minimum capacity of sessions from 4 to 1 ACU");
        assert_eq!((found[0].reduction.as_str(), found[0].impact), ("up to 2,190 ACU-hours a month (4 → 1 ACU while quiet)", Impact::High));
    }

    fn gib_bytes(g: f64) -> f64 {
        g * 1024.0 * 1024.0 * 1024.0
    }

    #[test]
    fn old_manual_snapshots_by_cluster() {
        let now = 1_000.0 * DAY;
        let snap = |id: &str, days: f64, gb: u64| Snapshot { id: id.into(), cluster: "orders".into(), created: Some(now - days * DAY), allocated: Some(gb), status: "available".into() };
        let found = old_snapshots(&[snap("a", 200.0, 350), snap("b", 1.0, 410), snap("c", 120.0, 700)], now);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].summary, "2 manual snapshots older than 90 days, the oldest 200 days");
        assert_eq!((found[0].reduction.as_str(), found[0].impact), ("1,050 GB of snapshot storage a month", Impact::High));
        assert_eq!(found[0].evidence, ["a, 350 GB, 200 days old", "c, 700 GB, 120 days old"]);
        assert_eq!(num(1234567.0), "1,234,567");
        assert_eq!(big(1.75e10), "17.5 billion");
    }
}

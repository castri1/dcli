//! DMS: replication instances with no task running for a week (often only full loads that
//! finished), instances a size too big for what runs on them, and Multi-AZ outside
//! production. No prices: a finding says what it removes in instance-hours and their vCPUs.

use std::collections::HashMap;

use super::{Finding, Impact, Scan, billing, metric_data, query, query_id};
use crate::aws;
use crate::aws::dms::{self, Instance, Replication};
use crate::config::is_production;
use crate::util::{fmt_ago, now_secs};

const MONTH_HOURS: f64 = 730.0;
/// An instance with no task running for this many days is idle.
const IDLE_DAYS: f64 = 7.0;
/// A size down is worth it when, on the smaller class, the CPU peak would stay under this (%)…
const SMALLER_CPU_MAX: f64 = 60.0;
/// …and the memory in use would take at most this share of the smaller class's memory
/// (the rest holds changes waiting to be applied).
const SMALLER_MEMORY_SHARE: f64 = 0.6;
/// Words in an environment, profile or instance name that say "not production".
const NON_PRODUCTION: [&str; 8] = ["dev", "test", "qa", "stag", "sandbox", "sbx", "uat", "demo"];

/// Impact by vCPU-hours a month removed: a 4-vCPU instance running all month (2,920) is
/// high, about a third of that medium.
fn by_vcpu_hours(h: f64) -> Impact {
    if h >= 2920.0 {
        Impact::High
    } else if h >= 1000.0 {
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
    out
}

/// What an instance did over the scan: its CPU peak (%) and its least freeable memory (bytes).
#[derive(Debug, Clone, Copy, Default)]
struct Usage {
    cpu_max: Option<f64>,
    free_min: Option<f64>,
}

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let instances = dms::list_instances(scan.aws)?;
    if instances.is_empty() {
        return Ok(Vec::new());
    }
    let tasks = dms::list_replications(scan.aws, &instances, false)?;
    // CPU and memory of the instances with something running: candidates for a smaller class.
    let busy: Vec<&Instance> = instances.iter().filter(|i| tasks.iter().any(|t| on(t, i) && t.running())).collect();
    let mut usage: HashMap<String, Usage> = HashMap::new();
    if !busy.is_empty() {
        let mut queries = Vec::new();
        for (n, i) in busy.iter().enumerate() {
            let dims = [("ReplicationInstanceIdentifier", i.id.as_str())];
            queries.push(query(query_id("cpu", n), "AWS/DMS", "CPUUtilization", &dims, "Maximum", 3600));
            queries.push(query(query_id("mem", n), "AWS/DMS", "FreeableMemory", &dims, "Minimum", 3600));
        }
        let series = metric_data(scan.aws, &queries, scan.days)?;
        for (n, i) in busy.iter().enumerate() {
            let values = |id: String| series.iter().find(|s| s.id == id).map(|s| s.points.iter().map(|p| p.1).collect::<Vec<f64>>()).unwrap_or_default();
            let cpu = values(query_id("cpu", n));
            let mem = values(query_id("mem", n));
            usage.insert(i.id.clone(), Usage { cpu_max: cpu.into_iter().reduce(f64::max), free_min: mem.into_iter().reduce(f64::min) });
        }
    }
    Ok(findings(&instances, &tasks, &usage, (scan.env, &scan.aws.profile), scan.days, now_secs()))
}

fn on(t: &Replication, i: &Instance) -> bool {
    t.instance_arn.as_ref() == Some(&i.arn)
}

/// `names`: the dcli environment and the AWS profile, which say whether this is production.
fn findings(instances: &[Instance], tasks: &[Replication], usage: &HashMap<String, Usage>, names: (&str, &str), days: u32, now: f64) -> Vec<Finding> {
    let mut out = Vec::new();
    for i in instances {
        let mine: Vec<&Replication> = tasks.iter().filter(|t| on(t, i)).collect();
        if let Some(f) = idle(i, &mine, now) {
            out.push(f);
            continue;
        }
        if let Some(u) = usage.get(&i.id)
            && let Some(f) = oversized(i, u, days)
        {
            out.push(f);
        }
        if let Some(f) = multi_az(i, names) {
            out.push(f);
        }
    }
    out
}

/// `730 instance-hours a month of dms.r5.large (1,460 vCPU-hours)`, doubled with Multi-AZ.
fn instance_hours(i: &Instance) -> (f64, String) {
    let copies = if i.multi_az { 2.0 } else { 1.0 };
    let hours = MONTH_HOURS * copies;
    let vcpus = dms::class_specs(&i.class).map_or(0.0, |(v, _)| f64::from(v));
    let az = if i.multi_az { ", Multi-AZ" } else { "" };
    let text = if vcpus > 0.0 {
        format!("{} instance-hours a month of {}{az} ({} vCPU-hours)", num(hours), i.class, num(hours * vcpus))
    } else {
        format!("{} instance-hours a month of {}{az}", num(hours), i.class)
    };
    (hours * vcpus.max(1.0), text)
}

fn idle(i: &Instance, mine: &[&Replication], now: f64) -> Option<Finding> {
    if mine.iter().any(|t| t.running() || !matches!(t.status.as_str(), "stopped" | "failed" | "ready")) {
        return None;
    }
    // Since the last task stopped, or since it was created when nothing ever ran.
    let since = mine.iter().filter_map(|t| t.stopped_at()).reduce(f64::max).or(i.created)?;
    let days = (now - since) / 86_400.0;
    if days < IDLE_DAYS {
        return None;
    }
    let summary = if mine.is_empty() {
        format!("no replication task on it, created {:.0} days ago", days)
    } else if mine.iter().all(|t| t.finished_full_load()) {
        let what = if mine.len() == 1 { "its only task is a full load that" } else { "its tasks are full loads that all" };
        format!("{what} finished {:.0} days ago", days)
    } else if mine.len() == 1 {
        format!("its only task has not run in {:.0} days", days)
    } else {
        format!("none of its {} tasks has run in {:.0} days", mine.len(), days)
    };
    let (vcpu_hours, reduction) = instance_hours(i);
    let mut f = Finding::new(
        "DMS",
        i.id.clone(),
        i.id.clone(),
        summary,
        "Delete it once its tasks are no longer needed (they are deleted or moved first); create one again for the next migration",
        reduction,
        by_vcpu_hours(vcpu_hours),
    )
    .score(vcpu_hours)
    .billed(&billing::dms_instance(&i.class), MONTH_HOURS)
    .evidence(format!("{} · {}", dms::class_label(&i.class), if i.multi_az { "Multi-AZ" } else { "single AZ" }));
    if i.multi_az {
        f = f.billed(&billing::dms_standby(&i.class), MONTH_HOURS);
    }
    for t in mine {
        let when = t.stopped_at().map(|s| format!(", stopped {}", fmt_ago(s))).unwrap_or_default();
        f = f.evidence(format!("{}: {} ({}){when}", t.id, t.status, t.migration.label()));
    }
    f = f.caveat("A task needs an instance to run: resuming one later means creating an instance and moving the task to it.");
    if mine.iter().any(|t| t.migration.has_cdc()) {
        f = f.caveat("A stopped CDC task can only resume while the source still keeps its change log from when it stopped.");
    }
    Some(f)
}

fn oversized(i: &Instance, u: &Usage, days: u32) -> Option<Finding> {
    let (cpu, free) = (u.cpu_max?, u.free_min?);
    if dms::class_family(&i.class).starts_with('t') {
        return None;
    }
    let smaller = dms::smaller_class(&i.class)?;
    let ((vcpus, gib), (new_vcpus, new_gib)) = (dms::class_specs(&i.class)?, dms::class_specs(&smaller)?);
    let gib_bytes = 1024.0 * 1024.0 * 1024.0;
    let used = (gib * gib_bytes - free).max(0.0);
    let projected_cpu = cpu * f64::from(vcpus) / f64::from(new_vcpus);
    if projected_cpu > SMALLER_CPU_MAX || used > new_gib * gib_bytes * SMALLER_MEMORY_SHARE {
        return None;
    }
    let copies = if i.multi_az { 2.0 } else { 1.0 };
    let vcpu_hours = f64::from(vcpus - new_vcpus) * MONTH_HOURS * copies;
    let mut f = Finding::new(
        "DMS",
        i.id.clone(),
        i.id.clone(),
        format!("peaked at {cpu:.0}% CPU with at least {:.1} of {gib:.0} GiB free in {days} days", free / gib_bytes),
        format!("Move it to {smaller} ({new_vcpus} vCPU, {new_gib:.0} GiB)"),
        format!("{} vCPU-hours a month ({vcpus} → {new_vcpus} vCPUs, {gib:.0} → {new_gib:.0} GiB)", num(vcpu_hours)),
        by_vcpu_hours(vcpu_hours),
    )
    .score(vcpu_hours)
    .evidence(format!("highest hourly CPU in {days} days: {cpu:.0}% (about {projected_cpu:.0}% on {smaller})"))
    .evidence(format!("least freeable memory: {:.1} GiB of {gib:.0} (about {:.1} GiB in use)", free / gib_bytes, used / gib_bytes))
    .caveat("Memory holds changes waiting to be applied: on a smaller class, peaks may go to disk (CDCChangesDiskTarget) and add latency.")
    .caveat("Changing the class restarts the instance and interrupts its tasks: pick a quiet hour (S in the DMS module).")
    .billed_instead(&billing::dms_instance(&i.class), &billing::dms_instance(&smaller), MONTH_HOURS);
    if i.multi_az {
        f = f.billed_instead(&billing::dms_standby(&i.class), &billing::dms_standby(&smaller), MONTH_HOURS);
    }
    Some(f)
}

/// Multi-AZ where the environment, the profile or the instance's name says development, test
/// or staging, and none of them says production.
fn multi_az(i: &Instance, (env, profile): (&str, &str)) -> Option<Finding> {
    if !i.multi_az || is_production(env) || is_production(profile) || is_production(&i.id) {
        return None;
    }
    let hint = |name: &str| NON_PRODUCTION.iter().find(|w| name.to_lowercase().contains(**w)).map(|w| (name.to_string(), *w));
    let (name, word) = hint(env).or_else(|| hint(&i.id)).or_else(|| hint(profile))?;
    let vcpus = dms::class_specs(&i.class).map_or(1.0, |(v, _)| f64::from(v));
    let vcpu_hours = MONTH_HOURS * vcpus;
    Some(
        Finding::new(
            "DMS",
            i.id.clone(),
            i.id.clone(),
            "Multi-AZ outside production: a standby instance runs all the time",
            "Turn Multi-AZ off if a failover is not needed here",
            format!("{} standby instance-hours a month of {} ({} vCPU-hours)", num(MONTH_HOURS), i.class, num(vcpu_hours)),
            by_vcpu_hours(vcpu_hours),
        )
        .score(vcpu_hours)
        .billed(&billing::dms_standby(&i.class), MONTH_HOURS)
        .evidence(format!("{name} reads as non-production ({word})"))
        .caveat(format!(
            "Production is judged from the names of the environment, the AWS profile and the instance ({env}, {profile}, {}): ignore this if it is production.",
            i.id
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::dms::{Kind, Migration, Stats};

    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    fn instance(id: &str, class: &str, multi_az: bool) -> Instance {
        Instance { id: id.into(), arn: format!("arn:{id}"), class: class.into(), multi_az, created: Some(0.0), status: "available".into(), ..Default::default() }
    }

    fn task(id: &str, on: &str, migration: Migration, status: &str, stopped: Option<f64>) -> Replication {
        Replication {
            kind: Kind::Task,
            id: id.into(),
            arn: format!("arn:task:{id}"),
            resource_id: id.into(),
            instance: Some(on.into()),
            instance_arn: Some(format!("arn:{on}")),
            migration,
            status: status.into(),
            stats: Stats { stop: stopped, full_load_percent: Some(100.0), ..Default::default() },
            last_failure: None,
            stop_reason: (status == "stopped" && migration == Migration::FullLoad).then(|| "Stop Reason FULL_LOAD_ONLY_FINISHED".into()),
            source_arn: String::new(),
            target_arn: String::new(),
            table_mappings: None,
            settings: None,
            recovery_checkpoint: None,
            cdc_start_position: None,
            created: None,
            started: None,
        }
    }

    #[test]
    fn idle_instances_and_finished_full_loads() {
        let now = 100.0 * 86_400.0;
        let instances = [instance("old", "dms.c5.xlarge", false), instance("staging", "dms.t3.medium", true), instance("busy", "dms.r5.large", false)];
        let tasks = [
            task("archive-copy", "old", Migration::FullLoad, "stopped", Some(now - 45.0 * 86_400.0)),
            task("legacy", "staging", Migration::FullLoadAndCdc, "failed", Some(now - 20.0 * 86_400.0)),
            task("orders", "busy", Migration::Cdc, "running", None),
            task("recent", "busy", Migration::Cdc, "stopped", Some(now - 86_400.0)),
        ];
        let found = findings(&instances, &tasks, &HashMap::new(), ("main", "default"), 14, now);
        assert_eq!(found.len(), 2, "{found:#?}");
        assert_eq!(found[0].summary, "its only task is a full load that finished 45 days ago");
        assert_eq!(found[0].reduction, "730 instance-hours a month of dms.c5.xlarge (2,920 vCPU-hours)");
        assert_eq!(found[0].impact, Impact::High);
        assert_eq!(found[1].summary, "its only task has not run in 20 days");
        assert_eq!(found[1].reduction, "1,460 instance-hours a month of dms.t3.medium, Multi-AZ (2,920 vCPU-hours)");
        assert!(found[1].caveats.iter().any(|c| c.contains("change log")));
        // An idle instance is not also flagged for Multi-AZ: removing it removes both.
        assert!(found.iter().all(|f| !f.summary.starts_with("Multi-AZ")));
    }

    #[test]
    fn a_size_down_when_cpu_and_memory_allow() {
        let i = instance("reporting", "dms.r5.2xlarge", false);
        let quiet = Usage { cpu_max: Some(7.0), free_min: Some(57.0 * GIB) };
        let f = oversized(&i, &quiet, 14).unwrap();
        assert_eq!(f.recommendation, "Move it to dms.r5.xlarge (4 vCPU, 32 GiB)");
        assert_eq!(f.reduction, "2,920 vCPU-hours a month (8 → 4 vCPUs, 64 → 32 GiB)");
        assert_eq!(f.impact, Impact::High);
        // Busy CPU, or memory that would not fit, keep the class.
        assert!(oversized(&i, &Usage { cpu_max: Some(45.0), ..quiet }, 14).is_none());
        assert!(oversized(&i, &Usage { free_min: Some(40.0 * GIB), ..quiet }, 14).is_none());
        assert!(oversized(&instance("small", "dms.r5.large", false), &quiet, 14).is_none());
    }

    #[test]
    fn multi_az_outside_production() {
        let f = multi_az(&instance("dms-dev-reporting", "dms.r5.large", true), ("main", "default")).unwrap();
        assert_eq!((f.billed[0].usage.as_str(), f.billed[0].quantity), ("Multi-AZUsg:dms.r5.large", 730.0));
        assert!(multi_az(&instance("dms-reporting", "dms.r5.large", true), ("main", "corp-staging")).is_some());
        assert!(multi_az(&instance("dms-reporting", "dms.r5.large", true), ("qa", "default")).is_some());
        assert!(multi_az(&instance("dms-reporting", "dms.r5.large", true), ("main", "default")).is_none());
        assert!(multi_az(&instance("dms-reporting", "dms.r5.large", true), ("prod", "corp-dev")).is_none());
        assert!(multi_az(&instance("dms-prod-dev", "dms.r5.large", true), ("main", "default")).is_none());
        assert!(multi_az(&instance("dms-dev", "dms.r5.large", false), ("dev", "default")).is_none());
    }
}

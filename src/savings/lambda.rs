//! Lambda: x86 functions that could run on arm64, provisioned concurrency left idle,
//! timeouts far above what functions take, and, on demand (Logs Insights bills the data it
//! scans), memory assigned against memory used.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;

use super::{
    Finding, Impact, MONTH, Scan, days_label, metric_data, par_map, peak, per_month, percent, qty, query, query_id, size, total,
};
use crate::aws::lambda::{self, FunctionConfiguration};
use crate::aws::metrics::Series;
use crate::aws::{self, Aws, AwsError, logs};
use crate::tui::chart::fmt_ms;
use crate::util::now_secs;

const MODULE: &str = "Lambda";
const DAY: u32 = 86_400;
/// Functions looked at, at most.
const MAX_FUNCTIONS: usize = 2000;

/// GB-seconds a month (duration × memory): medium and high impact from these.
const GBS_MEDIUM: f64 = 500_000.0;
const GBS_HIGH: f64 = 5_000_000.0;
/// Provisioned GB-seconds a month left idle (provisioned concurrency is billed for every
/// second it exists): medium and high impact from these.
const PROVISIONED_MEDIUM: f64 = 2_000_000.0;
const PROVISIONED_HIGH: f64 = 20_000_000.0;
/// Provisioned concurrency whose busiest hour used less than this share is mostly idle.
const PROVISIONED_IDLE: f64 = 0.5;
/// A timeout of at least a minute and this many times the slowest daily p99 is noted.
const TIMEOUT_FACTOR: f64 = 10.0;
/// Invocations needed before suggesting a memory size.
const MIN_SAMPLES: f64 = 50.0;
/// The memory sizes suggested (MB): Lambda takes any size, these read better.
const STEPS: [i64; 25] = [
    128, 192, 256, 320, 384, 448, 512, 640, 768, 896, 1024, 1280, 1536, 1792, 2048, 2560, 3072, 3584, 4096, 5120, 6144, 7168,
    8192, 9216, 10240,
];

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let functions = all_functions(scan.aws)?;
    let usage = usage(scan.aws, &functions, scan.days)?;
    let mut found = Vec::new();
    for f in &functions {
        let u = usage.get(&f.function_name).copied().unwrap_or_default();
        found.extend(arm64(f, &u, scan.days));
        found.extend(timeout(f, &u));
    }
    found.extend(provisioned(scan.aws, &functions, scan.days)?);
    Ok(found)
}

pub fn all_functions(aws: &Aws) -> aws::Result<Vec<FunctionConfiguration>> {
    let mut all = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let (page, next) = lambda::list_functions(aws, marker.as_deref())?;
        all.extend(page);
        match next {
            Some(m) if all.len() < MAX_FUNCTIONS => marker = Some(m),
            _ => return Ok(all),
        }
    }
}

/// A function's use over the window, from its metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub invocations: f64,
    /// Average duration, weighted by each day's invocations (ms).
    pub mean_ms: f64,
    /// The slowest day's p99 duration (ms).
    pub p99_ms: f64,
}

fn usage(aws: &Aws, functions: &[FunctionConfiguration], days: u32) -> aws::Result<HashMap<String, Usage>> {
    let mut queries = Vec::new();
    for (i, f) in functions.iter().enumerate() {
        let dims = [("FunctionName", f.function_name.as_str())];
        queries.push(query(query_id("inv", i), "AWS/Lambda", "Invocations", &dims, "Sum", DAY));
        queries.push(query(query_id("dur", i), "AWS/Lambda", "Duration", &dims, "Average", DAY));
        queries.push(query(query_id("pnn", i), "AWS/Lambda", "Duration", &dims, "p99", DAY));
    }
    let series = metric_data(aws, &queries, days)?;
    let by_id: HashMap<&str, &Series> = series.iter().map(|s| (s.id.as_str(), s)).collect();
    let empty = Series::default();
    Ok(functions
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let get = |p: &str| *by_id.get(query_id(p, i).as_str()).unwrap_or(&&empty);
            (f.function_name.clone(), usage_of(get("inv"), get("dur"), get("pnn")))
        })
        .collect())
}

fn usage_of(invocations: &Series, duration: &Series, p99: &Series) -> Usage {
    let per_day: HashMap<i64, f64> = invocations.points.iter().map(|(t, v)| (*t as i64, *v)).collect();
    let (mut sum, mut weight) = (0.0, 0.0);
    for (t, avg) in &duration.points {
        let w = per_day.get(&(*t as i64)).copied().unwrap_or(0.0);
        sum += avg * w;
        weight += w;
    }
    Usage {
        invocations: total(invocations),
        mean_ms: if weight > 0.0 { sum / weight } else { 0.0 },
        p99_ms: peak(p99).unwrap_or(0.0),
    }
}

fn memory_mb(f: &FunctionConfiguration) -> i64 {
    f.memory_size.unwrap_or(128)
}

fn on_x86(f: &FunctionConfiguration) -> bool {
    f.architectures.is_empty() || f.architectures.iter().any(|a| a == "x86_64")
}

/// GB-seconds of `invocations` lasting `ms` each with `memory` MB.
fn gb_seconds(invocations: f64, ms: f64, memory: f64) -> f64 {
    invocations * ms / 1000.0 * memory / 1024.0
}

fn arm64(f: &FunctionConfiguration, u: &Usage, days: u32) -> Option<Finding> {
    // go1.x never ran on arm64 (its successor, provided.al2023, does).
    if !on_x86(f) || u.invocations <= 0.0 || f.runtime.as_deref() == Some("go1.x") {
        return None;
    }
    let monthly = u.invocations * per_month(days);
    let gbs = gb_seconds(monthly, u.mean_ms, memory_mb(f) as f64);
    let image = f.package_type.as_deref() == Some("Image");
    let mut finding = Finding::new(
        MODULE,
        &f.function_name,
        &f.function_name,
        "runs on x86_64: a candidate for arm64 (Graviton)",
        "Move it to arm64 after building and testing it there",
        format!("{} GB-s a month on x86_64", qty(gbs)),
        Impact::of(gbs, GBS_MEDIUM, GBS_HIGH),
    )
    .score(gbs / GBS_HIGH)
    .evidence(format!("{} invocations in {days} days (≈ {} a month)", qty(u.invocations), qty(monthly)))
    .evidence(format!("average duration {}, {} MB of memory", fmt_ms(u.mean_ms), memory_mb(f)))
    .evidence(format!("runtime {}, {} package", f.runtime.as_deref().unwrap_or("custom"), f.package_type.as_deref().unwrap_or("Zip")));
    if !f.layers.is_empty() {
        finding = finding.evidence(format!("{} layer{}", f.layers.len(), if f.layers.len() == 1 { "" } else { "s" }));
    }
    finding = if image {
        finding.caveat("A container image must be rebuilt for arm64.")
    } else {
        finding.caveat("Native dependencies and layers must exist for arm64; plain Node.js, Python or Java code usually runs as is.")
    };
    Some(finding.caveat("Durations can change on arm64: compare them in dev first."))
}

/// A timeout to suggest for functions whose slowest p99 is `p99_ms`: three times it, on a
/// round number of seconds.
fn suggested_timeout(p99_ms: f64) -> i64 {
    let need = (p99_ms * 3.0 / 1000.0).ceil() as i64;
    [3, 5, 10, 15, 30, 60, 120, 300, 600].into_iter().find(|&s| s >= need).unwrap_or(900)
}

fn timeout(f: &FunctionConfiguration, u: &Usage) -> Option<Finding> {
    let timeout_s = f.timeout?;
    let timeout_ms = timeout_s as f64 * 1000.0;
    if timeout_s < 60 || u.p99_ms <= 0.0 || timeout_ms < u.p99_ms * TIMEOUT_FACTOR {
        return None;
    }
    let suggested = suggested_timeout(u.p99_ms);
    Some(
        Finding::new(
            MODULE,
            &f.function_name,
            &f.function_name,
            format!("timeout {timeout_s} s, slowest daily p99 {}", fmt_ms(u.p99_ms)),
            format!("Lower the timeout to {suggested} s"),
            format!("a stuck invocation bills up to {suggested} s instead of {timeout_s} s"),
            Impact::Low,
        )
        .score(timeout_ms / u.p99_ms / 1000.0)
        .evidence(format!("p99 duration of the slowest day {}, average {}", fmt_ms(u.p99_ms), fmt_ms(u.mean_ms)))
        .evidence(format!("timeout {timeout_s} s"))
        .caveat("p99 hides rare slow calls (cold starts, retries): check the maximum duration before lowering it."),
    )
}

/// Provisioned concurrency whose busiest hour used under half of it.
fn provisioned(aws: &Aws, functions: &[FunctionConfiguration], days: u32) -> aws::Result<Vec<Finding>> {
    let configs = par_map(functions, |f| lambda::provisioned_concurrency(aws, &f.function_name));
    let mut list = Vec::new();
    for (f, result) in functions.iter().zip(configs) {
        match result {
            Ok(configs) => list.extend(configs.into_iter().filter(|c| c.allocated_provisioned_concurrent_executions > 0).map(|c| (f, c))),
            // Not allowed to read it, or the function went away: the other checks still count.
            Err(e) if e.is_login_required() => return Err(e),
            Err(_) => {}
        }
    }
    if list.is_empty() {
        return Ok(Vec::new());
    }
    let queries: Vec<_> = list
        .iter()
        .enumerate()
        .map(|(i, (f, c))| {
            let resource = format!("{}:{}", f.function_name, c.qualifier());
            let dims = [("FunctionName", f.function_name.as_str()), ("Resource", resource.as_str())];
            query(query_id("pcu", i), "AWS/Lambda", "ProvisionedConcurrencyUtilization", &dims, "Maximum", 3600)
        })
        .collect();
    let series = metric_data(aws, &queries, days)?;
    Ok(list
        .iter()
        .zip(&series)
        .filter_map(|((f, c), s)| provisioned_finding(f, c.qualifier(), c.allocated_provisioned_concurrent_executions, peak(s).unwrap_or(0.0), days))
        .collect())
}

fn provisioned_finding(f: &FunctionConfiguration, qualifier: &str, allocated: i64, peak_use: f64, days: u32) -> Option<Finding> {
    if peak_use >= PROVISIONED_IDLE {
        return None;
    }
    let keep = (allocated as f64 * peak_use * 1.25).ceil() as i64;
    let removed = allocated - keep;
    let gbs = removed as f64 * memory_mb(f) as f64 / 1024.0 * MONTH;
    let recommendation =
        if keep == 0 { format!("Remove it from {qualifier}: nothing used it") } else { format!("Lower it to {keep} on {qualifier}") };
    Some(
        Finding::new(
            MODULE,
            &f.function_name,
            format!("{}:{qualifier}", f.function_name),
            format!("{allocated} provisioned, the busiest hour used {}", percent(peak_use)),
            recommendation,
            format!("−{} provisioned GB-s a month ({removed} of {allocated})", qty(gbs)),
            Impact::of(gbs, PROVISIONED_MEDIUM, PROVISIONED_HIGH),
        )
        .score(gbs / PROVISIONED_HIGH)
        .evidence(format!("highest hourly ProvisionedConcurrencyUtilization in {days} days: {}", percent(peak_use)))
        .evidence(format!("{allocated} instances × {} MB, provisioned around the clock", memory_mb(f)))
        .caveat("Provisioned concurrency keeps cold starts away at peaks: keep what the latency target needs.")
        .caveat("A schedule (Application Auto Scaling) can provision it only at busy hours."),
    )
}

// ---- memory used (Logs Insights) -------------------------------------------------------------

/// Reads the REPORT line of every invocation: the memory each one used and its billed time.
const MEMORY_QUERY: &str = "filter @type = \"REPORT\"\n\
| stats count(*) as invocations, max(@memorySize) as memorySize, max(@maxMemoryUsed) as maxUsed, \
pct(@maxMemoryUsed, 99) as p99Used, avg(@billedDuration) as avgBilledMs by @log";

/// Log groups per Insights query, at most.
const GROUPS_PER_QUERY: usize = 50;

/// What a memory analysis would scan.
#[derive(Debug, Clone, Default)]
pub struct Estimate {
    /// The functions analysed: those with a log group of their own.
    pub functions: Vec<FunctionConfiguration>,
    /// Functions left out because they share their log group (the REPORT lines do not say
    /// which function wrote them).
    pub shared: Vec<String>,
    /// Bytes the groups received over the window (what Insights would scan).
    pub bytes: f64,
    pub days: u32,
    /// Bytes received each day, oldest first, all groups together.
    pub daily: Vec<f64>,
}

impl Estimate {
    /// The same functions over the last `days` days (at most the days measured).
    pub fn with_days(&self, days: u32) -> Estimate {
        let days = days.min(self.days).max(1);
        let bytes = self.daily.iter().rev().take(days as usize).sum();
        Estimate { bytes, days, ..self.clone() }
    }

    /// `23 functions · ≈ 4.2 GB of logs over 14 days`.
    pub fn describe(&self) -> String {
        let n = self.functions.len();
        format!("{n} function{} · ≈ {} of logs over {}", if n == 1 { "" } else { "s" }, size(self.bytes), days_label(self.days))
    }
}

/// What analysing the memory of `functions` (every function when `None`) would scan.
pub fn memory_estimate(aws: &Aws, functions: Option<Vec<FunctionConfiguration>>, days: u32) -> aws::Result<Estimate> {
    // A log group is shared when any function of the account writes to it, not only one of
    // those analysed: one function's own analysis would otherwise count every writer's lines.
    let all = all_functions(aws)?;
    let mut writers: HashMap<String, usize> = HashMap::new();
    for f in &all {
        *writers.entry(f.log_group()).or_default() += 1;
    }
    let functions = functions.unwrap_or(all);
    let (functions, shared): (Vec<_>, Vec<_>) = functions.into_iter().partition(|f| writers.get(&f.log_group()).is_none_or(|n| *n <= 1));
    let queries: Vec<_> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| query(query_id("inb", i), "AWS/Logs", "IncomingBytes", &[("LogGroupName", &f.log_group())], "Sum", DAY))
        .collect();
    let mut by_day: std::collections::BTreeMap<i64, f64> = Default::default();
    for series in metric_data(aws, &queries, days)? {
        for (t, bytes) in series.points {
            *by_day.entry(t as i64).or_default() += bytes;
        }
    }
    let daily: Vec<f64> = by_day.into_values().collect();
    let bytes = daily.iter().sum();
    Ok(Estimate { functions, shared: shared.into_iter().map(|f| f.function_name).collect(), bytes, days, daily })
}

/// The memory a function used, from its REPORT lines.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryUse {
    pub function: String,
    /// Assigned (MB).
    pub memory_mb: i64,
    /// Used by 99 % of invocations (MB).
    pub p99_mb: f64,
    /// Used at most (MB).
    pub max_mb: f64,
    /// REPORT lines read.
    pub samples: f64,
    /// Invocations over the window, from the metrics (the REPORT lines when unknown).
    pub invocations: f64,
    pub avg_billed_ms: f64,
    pub days: u32,
    /// When it was read (Unix seconds).
    pub at: f64,
}

impl MemoryUse {
    /// The size that fits: p99 plus a quarter, and at least the largest use plus a
    /// twentieth, on one of the usual steps.
    pub fn fitting_mb(&self) -> i64 {
        let need = (self.p99_mb * 1.25).max(self.max_mb * 1.05).max(128.0);
        STEPS.into_iter().find(|&s| s as f64 >= need).unwrap_or(10240)
    }

    /// A smaller size worth setting: at least a quarter below the assigned one.
    pub fn smaller_mb(&self) -> Option<i64> {
        let fit = self.fitting_mb();
        (self.samples >= MIN_SAMPLES && fit < self.memory_mb && fit as f64 <= self.memory_mb as f64 * 0.75).then_some(fit)
    }

    /// One line for the Lambda module.
    pub fn hint(&self) -> String {
        let used = format!("p99 used {:.0} of {} MB ({})", self.p99_mb, self.memory_mb, days_label(self.days));
        match self.smaller_mb() {
            Some(mb) => format!("{used} · {mb} MB would do"),
            None if self.samples < MIN_SAMPLES => format!("{used} · too few invocations to say"),
            None => format!("{used} · fits"),
        }
    }

    /// GB-seconds a month the smaller size removes, if the duration stays the same.
    pub fn monthly_reduction(&self) -> Option<f64> {
        let smaller = self.smaller_mb()?;
        let monthly = self.invocations * per_month(self.days);
        Some(gb_seconds(monthly, self.avg_billed_ms, (self.memory_mb - smaller) as f64))
    }
}

/// What a memory analysis found, and the bytes Insights scanned (and billed).
#[derive(Debug, Clone, Default)]
pub struct MemoryRun {
    pub uses: Vec<MemoryUse>,
    pub bytes_scanned: f64,
    /// Functions analysed without any REPORT line in the window.
    pub silent: Vec<String>,
}

/// Runs the memory analysis of an estimate: Insights queries of up to 50 groups, one after
/// the other. `progress` hears how many groups are done.
pub fn memory_analysis(aws: &Aws, estimate: &Estimate, progress: &dyn Fn(usize, usize)) -> aws::Result<MemoryRun> {
    let end = now_secs() as i64;
    let start = end - i64::from(estimate.days) * 86_400;
    let by_group: HashMap<String, &FunctionConfiguration> = estimate.functions.iter().map(|f| (f.log_group(), f)).collect();
    let invocations = invocations(aws, &estimate.functions, estimate.days)?;
    let groups: Vec<String> = estimate.functions.iter().map(FunctionConfiguration::log_group).collect();
    let mut run = MemoryRun::default();
    let mut seen = HashSet::new();
    for (n, batch) in groups.chunks(GROUPS_PER_QUERY).enumerate() {
        progress(n * GROUPS_PER_QUERY, groups.len());
        let id = logs::start_query(aws, batch, (start, end), MEMORY_QUERY, None, Some(10_000))?;
        let results = wait(aws, &id)?;
        run.bytes_scanned += results.statistics.bytes_scanned;
        for row in &results.results {
            let field = |name: &str| row.iter().find(|f| f.field == name).map(|f| f.value.as_str()).unwrap_or_default();
            let number = |name: &str| field(name).trim().parse::<f64>().unwrap_or(0.0);
            let group = field("@log").split_once(':').map_or(field("@log"), |(_, g)| g);
            let Some(f) = by_group.get(group) else { continue };
            seen.insert(f.function_name.clone());
            let assigned = memory_mb(f);
            // Insights reports memory in bytes; scaling by the size it reports keeps the
            // figures in the MB Lambda uses whatever the unit.
            let reported = number("memorySize");
            let mb = |v: f64| if reported > 0.0 { v / reported * assigned as f64 } else { v / 1e6 };
            let samples = number("invocations");
            run.uses.push(MemoryUse {
                function: f.function_name.clone(),
                memory_mb: assigned,
                p99_mb: mb(number("p99Used")),
                max_mb: mb(number("maxUsed")),
                samples,
                invocations: invocations.get(&f.function_name).copied().filter(|n| *n > 0.0).unwrap_or(samples),
                avg_billed_ms: number("avgBilledMs"),
                days: estimate.days,
                at: now_secs(),
            });
        }
    }
    progress(groups.len(), groups.len());
    run.silent = estimate.functions.iter().map(|f| f.function_name.clone()).filter(|n| !seen.contains(n)).collect();
    run.uses.sort_by(|a, b| a.function.cmp(&b.function));
    remember(aws, &run.uses);
    Ok(run)
}

fn invocations(aws: &Aws, functions: &[FunctionConfiguration], days: u32) -> aws::Result<HashMap<String, f64>> {
    let queries: Vec<_> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| query(query_id("inv", i), "AWS/Lambda", "Invocations", &[("FunctionName", &f.function_name)], "Sum", DAY))
        .collect();
    let series = metric_data(aws, &queries, days)?;
    Ok(functions.iter().zip(&series).map(|(f, s)| (f.function_name.clone(), total(s))).collect())
}

/// Polls a query until it ends (Insights queries time out after an hour at most).
fn wait(aws: &Aws, id: &str) -> aws::Result<logs::QueryResults> {
    for _ in 0..3600 {
        let results = logs::get_query_results(aws, id)?;
        if !results.running() {
            return match results.status.as_str() {
                "Complete" => Ok(results),
                status => Err(AwsError::Network(format!("the Logs Insights query ended {}", status.to_lowercase()))),
            };
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let _ = logs::stop_query(aws, id);
    Err(AwsError::Network("the Logs Insights query took too long".into()))
}

/// The finding for a function that could use less memory.
pub fn memory_finding(u: &MemoryUse) -> Option<Finding> {
    let smaller = u.smaller_mb()?;
    let gbs = u.monthly_reduction()?;
    let share = (u.memory_mb - smaller) as f64 / u.memory_mb as f64;
    Some(
        Finding::new(
            MODULE,
            &u.function,
            &u.function,
            format!("uses {:.0} of {} MB (p99)", u.p99_mb, u.memory_mb),
            format!("Set the memory to {smaller} MB"),
            format!("−{} GB-s a month (−{})", qty(gbs), percent(share)),
            Impact::of(gbs, GBS_MEDIUM, GBS_HIGH),
        )
        .score(gbs / GBS_HIGH)
        .evidence(format!(
            "p99 memory used {:.0} MB, at most {:.0} MB, over {} invocations ({} of REPORT lines)",
            u.p99_mb,
            u.max_mb,
            qty(u.samples),
            days_label(u.days)
        ))
        .evidence(format!("average billed duration {}", fmt_ms(u.avg_billed_ms)))
        .evidence(format!("≈ {} invocations a month", qty(u.invocations * per_month(u.days))))
        .caveat("Less memory also means less CPU: a function that computes may take longer and save less. Try the size in dev first.")
        .caveat("The reduction assumes the duration stays the same."),
    )
}

/// (profile, region, function).
type MemoryKey = (String, String, String);

/// Memory analyses of this run of dcli: the Lambda module shows the one of its function,
/// whoever ran it.
fn memory() -> &'static Mutex<HashMap<MemoryKey, MemoryUse>> {
    static MEMORY: OnceLock<Mutex<HashMap<MemoryKey, MemoryUse>>> = OnceLock::new();
    MEMORY.get_or_init(Default::default)
}

fn remember(aws: &Aws, uses: &[MemoryUse]) {
    let mut map = memory().lock().unwrap();
    for u in uses {
        map.insert((aws.profile.clone(), aws.region.clone(), u.function.clone()), u.clone());
    }
}

/// The last memory analysis of `function` in this run of dcli.
pub fn remembered(aws: &Aws, function: &str) -> Option<MemoryUse> {
    memory().lock().unwrap().get(&(aws.profile.clone(), aws.region.clone(), function.to_string())).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function(name: &str, memory: i64, arch: &str, timeout: i64) -> FunctionConfiguration {
        FunctionConfiguration {
            function_name: name.into(),
            memory_size: Some(memory),
            architectures: vec![arch.into()],
            timeout: Some(timeout),
            runtime: Some("nodejs22.x".into()),
            ..Default::default()
        }
    }

    fn series(points: &[(f64, f64)]) -> Series {
        Series { points: points.to_vec(), ..Default::default() }
    }

    #[test]
    fn usage_weights_durations_by_invocations() {
        let u = usage_of(&series(&[(0.0, 100.0), (86_400.0, 300.0)]), &series(&[(0.0, 100.0), (86_400.0, 500.0)]), &series(&[(0.0, 900.0), (86_400.0, 1200.0)]));
        assert_eq!(u.invocations, 400.0);
        assert_eq!(u.mean_ms, (100.0 * 100.0 + 500.0 * 300.0) / 400.0);
        assert_eq!(u.p99_ms, 1200.0);
    }

    #[test]
    fn x86_functions_with_traffic_are_arm64_candidates() {
        let u = Usage { invocations: 1_400_000.0, mean_ms: 800.0, p99_ms: 2000.0 };
        let f = arm64(&function("resize", 2048, "x86_64", 60), &u, 14).unwrap();
        // 3.04 M invocations a month × 0.8 s × 2 GB.
        assert_eq!(f.reduction, "4.9 M GB-s a month on x86_64");
        assert_eq!(f.impact, Impact::Medium);
        assert!(arm64(&function("resize", 2048, "arm64", 60), &u, 14).is_none());
        assert!(arm64(&function("idle", 2048, "x86_64", 60), &Usage::default(), 14).is_none());
        let mut go = function("old", 512, "x86_64", 60);
        go.runtime = Some("go1.x".into());
        assert!(arm64(&go, &u, 14).is_none());
    }

    #[test]
    fn notes_timeouts_far_above_the_slowest_p99() {
        let u = |p99| Usage { invocations: 10.0, mean_ms: 100.0, p99_ms: p99 };
        let f = timeout(&function("report", 512, "arm64", 900), &u(1200.0)).unwrap();
        assert_eq!(f.recommendation, "Lower the timeout to 5 s");
        assert_eq!(f.impact, Impact::Low);
        // Short timeouts, timeouts the function gets near to, and functions without data stay.
        assert!(timeout(&function("api", 512, "arm64", 30), &u(100.0)).is_none());
        assert!(timeout(&function("batch", 512, "arm64", 300), &u(60_000.0)).is_none());
        assert!(timeout(&function("idle", 512, "arm64", 300), &u(0.0)).is_none());
        assert_eq!(suggested_timeout(250_000.0), 900);
    }

    #[test]
    fn idle_provisioned_concurrency() {
        let f = function("checkout", 1024, "arm64", 30);
        let found = provisioned_finding(&f, "live", 50, 0.12, 14).unwrap();
        // The busiest hour used 6 of 50: keep 8 (a quarter of margin), remove 42 × 1 GB.
        assert_eq!(found.recommendation, "Lower it to 8 on live");
        assert_eq!(found.reduction, "−110.5 M provisioned GB-s a month (42 of 50)");
        assert_eq!(found.impact, Impact::High);
        assert_eq!(provisioned_finding(&f, "live", 5, 0.0, 14).unwrap().recommendation, "Remove it from live: nothing used it");
        assert!(provisioned_finding(&f, "live", 50, 0.6, 14).is_none());
    }

    fn used(memory: i64, p99: f64, max: f64) -> MemoryUse {
        MemoryUse {
            function: "f".into(),
            memory_mb: memory,
            p99_mb: p99,
            max_mb: max,
            samples: 1000.0,
            invocations: 1_000_000.0,
            avg_billed_ms: 1000.0,
            days: 14,
            at: 0.0,
        }
    }

    #[test]
    fn suggests_memory_with_margin_on_round_steps() {
        assert_eq!(used(3008, 410.0, 420.0).smaller_mb(), Some(640));
        assert_eq!(used(512, 96.0, 101.0).smaller_mb(), Some(128));
        // Never under 128 MB, and the largest use counts too.
        assert_eq!(used(128, 60.0, 61.0).smaller_mb(), None);
        assert_eq!(used(1024, 300.0, 900.0).smaller_mb(), None);
        // Less than a quarter smaller is not worth it.
        assert_eq!(used(2048, 1400.0, 1500.0).smaller_mb(), None);
        assert_eq!(used(2048, 1730.0, 1800.0).fitting_mb(), 2560);
        let mut few = used(3008, 410.0, 420.0);
        few.samples = 10.0;
        assert_eq!(few.smaller_mb(), None);
        assert!(few.hint().ends_with("too few invocations to say"));
        assert_eq!(used(3008, 410.0, 420.0).hint(), "p99 used 410 of 3008 MB (14 days) · 640 MB would do");
    }

    #[test]
    fn estimates_shorter_windows_from_the_last_days() {
        let e = Estimate { bytes: 10.0, days: 4, daily: vec![1.0, 2.0, 3.0, 4.0], ..Default::default() };
        assert_eq!((e.with_days(2).bytes, e.with_days(2).days), (7.0, 2));
        assert_eq!((e.with_days(14).bytes, e.with_days(14).days), (10.0, 4));
        assert_eq!(e.with_days(1).describe(), "0 functions · ≈ 0 MB of logs over 1 day");
    }

    #[test]
    fn memory_findings_count_gb_seconds() {
        let f = memory_finding(&used(3008, 410.0, 420.0)).unwrap();
        // 2.17 M invocations a month × 1 s × (3008 − 640) / 1024 GB.
        assert_eq!(f.reduction, "−5 M GB-s a month (−79 %)");
        assert_eq!(f.recommendation, "Set the memory to 640 MB");
        assert_eq!(f.impact, Impact::High);
        assert!(memory_finding(&used(1024, 900.0, 950.0)).is_none());
    }
}

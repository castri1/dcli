//! Lambda: x86 functions that could run on arm64, provisioned concurrency left idle,
//! timeouts far above what functions take, and, on demand (Logs Insights bills the data it
//! scans), memory assigned against memory used. Each memory analysis is kept per environment
//! ([`keep`]), so later scans, and Claude through `dcli mcp`, use it without scanning again.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{
    Finding, Impact, MONTH, Scan, billing, days_label, metric_data, par_map, peak, per_month, percent, qty, query, query_id, size,
    total,
};
use crate::aws::lambda::{self, FunctionConfiguration};
use crate::aws::metrics::Series;
use crate::aws::{self, Aws, logs};
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
    let account = costs_account(scan.env);
    let kept = fresh_kept(&kept(scan.env, Place::of(scan.aws, account.as_deref())), &functions, now_secs());
    remember(scan.aws, &kept);
    found.extend(kept_findings(&kept));
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

pub(crate) fn memory_mb(f: &FunctionConfiguration) -> i64 {
    f.memory_size.unwrap_or(128)
}

pub(crate) fn on_x86(f: &FunctionConfiguration) -> bool {
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
    .kind("arm64")
    .score(gbs / GBS_HIGH)
    .billed_instead(billing::LAMBDA_GBS, billing::LAMBDA_GBS_ARM, gbs)
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
        .kind("timeout")
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
        .kind("provisioned-concurrency")
        .score(gbs / PROVISIONED_HIGH)
        .billed(if on_x86(f) { billing::LAMBDA_PROVISIONED } else { billing::LAMBDA_PROVISIONED_ARM }, gbs)
        .evidence(format!("highest hourly ProvisionedConcurrencyUtilization in {days} days: {}", percent(peak_use)))
        .evidence(format!("{allocated} instances × {} MB, provisioned around the clock", memory_mb(f)))
        .caveat("Provisioned concurrency keeps cold starts away at peaks: keep what the latency target needs.")
        .caveat("A schedule (Application Auto Scaling) can provision it only at busy hours."),
    )
}

// ---- memory used (Logs Insights) -------------------------------------------------------------

/// Reads the report of every invocation: the share of its memory each one used and its billed
/// time. Text logs have a REPORT line (`@maxMemoryUsed`, `@memorySize`, `@billedDuration`); JSON
/// logs (LoggingConfig.LogFormat JSON) a `platform.report` event with the same figures in MB
/// under `record.metrics`. A share of the size each invocation had keeps the units out.
const MEMORY_QUERY: &str = "filter @type = \"REPORT\" or type = \"platform.report\"\n\
| fields coalesce(@maxMemoryUsed / @memorySize, record.metrics.maxMemoryUsedMB / record.metrics.memorySizeMB) as usedShare, \
coalesce(@billedDuration, record.metrics.billedDurationMs) as billedMs\n\
| stats count(*) as invocations, max(usedShare) as maxShare, pct(usedShare, 99) as p99Share, avg(billedMs) as avgBilledMs by @log";

/// Seconds the window of an analysis ends before now: CloudWatch's IncomingBytes lags, and the
/// estimate and the query must read the same logs.
const LAG_SECS: i64 = 600;

/// The window an analysis of `days` days reads, as of `now`: whole hours (CloudWatch rounds a
/// daily metric's start to the hour), ending a little before now.
pub fn window(days: u32, now: f64) -> (i64, i64) {
    let end = (now as i64 - LAG_SECS).div_euclid(3600) * 3600;
    (end - i64::from(days.max(1)) * 86_400, end)
}

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
    /// Bytes each function's group received over the window, in the order of `functions`.
    pub per_function: Vec<f64>,
    /// The window measured, (start, end) in Unix seconds: the analysis reads exactly this.
    pub window: (i64, i64),
}

impl Estimate {
    /// The same functions over the last `days` days (at most the days measured).
    pub fn with_days(&self, days: u32) -> Estimate {
        let days = days.min(self.days).max(1);
        let bytes = self.daily.iter().rev().take(days as usize).sum();
        // The daily figures are every group's together: each function gets the same share.
        let share = if self.bytes > 0.0 { bytes / self.bytes } else { 0.0 };
        let per_function = self.per_function.iter().map(|b| b * share).collect();
        let end = self.window.1;
        Estimate { bytes, days, per_function, window: (end - i64::from(days) * 86_400, end), ..self.clone() }
    }

    /// Only the functions named in `names` (in the order given), with what their groups
    /// received; the days measured are scaled to their share.
    pub fn only(&self, names: &[String]) -> Estimate {
        let mut out = Estimate { shared: self.shared.clone(), days: self.days, window: self.window, ..Default::default() };
        for name in names {
            if let Some(i) = self.functions.iter().position(|f| f.function_name == *name) {
                out.functions.push(self.functions[i].clone());
                out.per_function.push(self.per_function.get(i).copied().unwrap_or(0.0));
            }
        }
        out.bytes = out.per_function.iter().sum();
        let share = if self.bytes > 0.0 { out.bytes / self.bytes } else { 0.0 };
        out.daily = self.daily.iter().map(|d| d * share).collect();
        out
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
    let mut per_function = Vec::with_capacity(functions.len());
    // The window the analysis then reads.
    let (start, end) = window(days, now_secs());
    let mut measured = Vec::with_capacity(queries.len());
    for chunk in queries.chunks(500) {
        measured.extend(crate::aws::metrics::get_metric_data(aws, chunk, start as f64, end as f64)?);
    }
    for series in measured {
        per_function.push(total(&series));
        for (t, bytes) in series.points {
            *by_day.entry(t as i64).or_default() += bytes;
        }
    }
    per_function.resize(functions.len(), 0.0);
    let daily: Vec<f64> = by_day.into_values().collect();
    let bytes = daily.iter().sum();
    let shared = shared.into_iter().map(|f| f.function_name).collect();
    Ok(Estimate { functions, shared, bytes, days, daily, per_function, window: (start, end) })
}

/// The GB-seconds each of `functions` used over the last `days` days (duration × memory, from
/// its metrics): what ranks them by what they cost.
pub fn gb_seconds_by_function(aws: &Aws, functions: &[FunctionConfiguration], days: u32) -> aws::Result<HashMap<String, f64>> {
    let usage = usage(aws, functions, days)?;
    Ok(functions
        .iter()
        .map(|f| {
            let u = usage.get(&f.function_name).copied().unwrap_or_default();
            (f.function_name.clone(), gb_seconds(u.invocations, u.mean_ms, memory_mb(f) as f64))
        })
        .collect())
}

/// The memory a function used, from its REPORT lines.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryUse {
    pub function: String,
    /// It runs on arm64 (its GB-seconds are billed as such).
    pub arm64: bool,
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
    /// The AWS account and region it was read in (empty when unknown): an environment that now
    /// points elsewhere does not reuse it.
    pub account: String,
    pub region: String,
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
    /// Functions analysed without any report in the window.
    pub silent: Vec<String>,
    /// Why the analysis could not be kept for later scans, if it could not.
    pub keep_error: Option<String>,
    /// Why it stopped before reading every function (the GB allowed reached, a query that
    /// failed): what it found until then is in `uses`, possibly partly.
    pub stopped: Option<String>,
}

/// Runs the memory analysis of an estimate in environment `env`: Insights queries of up to 50
/// groups, one after the other. `progress` hears how many groups are done. The result is kept
/// for later scans ([`keep`]).
pub fn memory_analysis(aws: &Aws, env: &str, estimate: &Estimate, progress: &dyn Fn(usize, usize)) -> aws::Result<MemoryRun> {
    memory_analysis_within(aws, env, estimate, None, progress)
}

/// [`memory_analysis`] that scans at most `budget` bytes: a query is stopped as soon as the
/// analysis reaches it, and no other starts. An error means nothing was scanned; once a query
/// has scanned, a failure ends the analysis with what it found ([`MemoryRun::stopped`]) and the
/// bytes it scanned (a query whose figures never came counts its whole estimate).
pub fn memory_analysis_within(
    aws: &Aws,
    env: &str,
    estimate: &Estimate,
    budget: Option<f64>,
    progress: &dyn Fn(usize, usize),
) -> aws::Result<MemoryRun> {
    let (start, end) = if estimate.window.1 > 0 { estimate.window } else { window(estimate.days, now_secs()) };
    let by_group: HashMap<String, &FunctionConfiguration> = estimate.functions.iter().map(|f| (f.log_group(), f)).collect();
    let invocations = invocations(aws, &estimate.functions, estimate.days)?;
    let (account, region) = (account_of(aws, env), aws.region.clone());
    let groups: Vec<String> = estimate.functions.iter().map(FunctionConfiguration::log_group).collect();
    let mut run = MemoryRun::default();
    let mut seen = HashSet::new();
    for (n, batch) in groups.chunks(GROUPS_PER_QUERY).enumerate() {
        if budget.is_some_and(|b| run.bytes_scanned >= b * STOP_AT) {
            run.stopped = Some(format!("stopped at the GB allowed ({} scanned)", size(run.bytes_scanned)));
            break;
        }
        progress(n * GROUPS_PER_QUERY, groups.len());
        // What this batch should scan, from the estimate: what a query counts when its own
        // figures never come.
        let expected: f64 = batch
            .iter()
            .filter_map(|g| estimate.functions.iter().position(|f| f.log_group() == *g))
            .map(|i| estimate.per_function.get(i).copied().unwrap_or(0.0))
            .sum::<f64>()
            * 1.05;
        let id = match logs::start_query(aws, batch, (start, end), MEMORY_QUERY, None, Some(10_000)) {
            Ok(id) => id,
            Err(e) if n == 0 => return Err(e),
            Err(e) => {
                run.stopped = Some(format!("a Logs Insights query could not start: {}", e.short()));
                break;
            }
        };
        let waited = wait_within(aws, &id, run.bytes_scanned, budget, expected);
        run.bytes_scanned += waited.bytes;
        for row in waited.results.iter().flat_map(|r| &r.results) {
            let field = |name: &str| row.iter().find(|f| f.field == name).map(|f| f.value.as_str()).unwrap_or_default();
            let number = |name: &str| field(name).trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0).unwrap_or(0.0);
            let group = field("@log").split_once(':').map_or(field("@log"), |(_, g)| g);
            let Some(f) = by_group.get(group) else { continue };
            seen.insert(f.function_name.clone());
            let assigned = memory_mb(f);
            // Shares of the size each invocation had, in the MB assigned now.
            let mb = |share: f64| share.min(1.0) * assigned as f64;
            let samples = number("invocations");
            run.uses.push(MemoryUse {
                function: f.function_name.clone(),
                arm64: !on_x86(f),
                memory_mb: assigned,
                p99_mb: mb(number("p99Share")),
                max_mb: mb(number("maxShare")),
                samples,
                invocations: invocations.get(&f.function_name).copied().filter(|n| *n > 0.0).unwrap_or(samples),
                avg_billed_ms: number("avgBilledMs"),
                days: estimate.days,
                at: now_secs(),
                account: account.clone(),
                region: region.clone(),
            });
        }
        if let Some(why) = waited.stopped {
            run.stopped = Some(why);
            break;
        }
    }
    progress(groups.len(), groups.len());
    run.silent = estimate.functions.iter().map(|f| f.function_name.clone()).filter(|n| !seen.contains(n)).collect();
    run.uses.sort_by(|a, b| a.function.cmp(&b.function));
    remember(aws, &run.uses);
    run.keep_error = keep(env, &run.uses).err().map(|e| e.to_string());
    Ok(run)
}

/// Share of the budget at which a running query is stopped: Insights may scan a little more
/// before it stops.
const STOP_AT: f64 = 0.97;

/// The account `env` reads (the costs read for it, else STS); empty when neither says.
fn account_of(aws: &Aws, env: &str) -> String {
    crate::costs::cached(env)
        .map(|c| c.account)
        .filter(|a| crate::aws::sts::is_account_id(a))
        .or_else(|| crate::aws::sts::caller_account(aws).ok())
        .unwrap_or_default()
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

/// How a query ended: its results (when complete, or partial when it was stopped), the bytes
/// it scanned, and why it stopped early.
#[derive(Debug, Default)]
struct Waited {
    results: Option<logs::QueryResults>,
    bytes: f64,
    stopped: Option<String>,
}

/// Polls a query until it ends, stopping it once the analysis (`prior` bytes before it) reaches
/// `budget`, when its results cannot be read, or after an hour: a query is never left running.
/// A query whose figures never came counts `expected` bytes.
fn wait_within(aws: &Aws, id: &str, prior: f64, budget: Option<f64>, expected: f64) -> Waited {
    poll(id, prior, budget, expected, |id| logs::get_query_results(aws, id), |id| logs::stop_query(aws, id).is_ok(), std::thread::sleep)
}

/// [`wait_within`] with the calls given (tests).
fn poll(
    id: &str,
    prior: f64,
    budget: Option<f64>,
    expected: f64,
    get: impl Fn(&str) -> aws::Result<logs::QueryResults>,
    stop: impl Fn(&str) -> bool,
    sleep: impl Fn(Duration),
) -> Waited {
    let mut seen: f64 = 0.0;
    let mut last: Option<logs::QueryResults> = None;
    for _ in 0..7200 {
        let results = match get(id) {
            Ok(r) => r,
            Err(e) => {
                stop(id);
                let bytes = if last.is_some() { seen } else { seen.max(expected) };
                return Waited { results: last, bytes, stopped: Some(format!("the Logs Insights query could not be read: {}", e.short())) };
            }
        };
        let scanned = results.statistics.bytes_scanned;
        seen = seen.max(if scanned.is_finite() { scanned } else { 0.0 });
        if !results.running() {
            return match results.status.as_str() {
                "Complete" => Waited { results: Some(results), bytes: seen, stopped: None },
                status => Waited {
                    stopped: Some(format!("the Logs Insights query ended {}", status.to_lowercase())),
                    results: Some(results),
                    bytes: seen,
                },
            };
        }
        if let Some(b) = budget.filter(|b| prior + seen >= b * STOP_AT) {
            stop(id);
            // The figures once stopped: what it scanned in the end, and its partial results.
            let last_figures = get(id).ok();
            let bytes = last_figures.as_ref().map_or(seen, |r| seen.max(r.statistics.bytes_scanned));
            return Waited {
                results: last_figures.or(Some(results)),
                bytes,
                stopped: Some(format!("stopped at the GB allowed ({} of {})", size(prior + bytes), size(b))),
            };
        }
        // Closer to the budget, a closer look.
        let near = budget.is_some_and(|b| prior + seen >= b * 0.75);
        last = Some(results);
        sleep(if near { Duration::from_millis(250) } else { Duration::from_millis(500) });
    }
    stop(id);
    Waited { results: last, bytes: seen.max(expected), stopped: Some("the Logs Insights query took too long".into()) }
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
        .kind("memory")
        .billed(if u.arm64 { billing::LAMBDA_GBS_ARM } else { billing::LAMBDA_GBS }, gbs)
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

/// The last memory analysis of `function` in this run of dcli (or kept from an earlier one,
/// once [`preload`] or a scan read them).
pub fn remembered(aws: &Aws, function: &str) -> Option<MemoryUse> {
    memory().lock().unwrap().get(&(aws.profile.clone(), aws.region.clone(), function.to_string())).cloned()
}

// ---- memory analyses kept on disk ------------------------------------------------------------

/// Days a kept analysis still makes findings.
pub const KEPT_DAYS: f64 = 30.0;
/// Days an analysis stays in the file at all.
const KEPT_AT_MOST_DAYS: f64 = 90.0;
/// Functions kept per environment, at most.
const KEPT_MAX: usize = 5000;

/// What the file holds: the latest analysis of each function of one environment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Kept {
    env: String,
    uses: Vec<MemoryUse>,
}

/// `~/.deployment-cli/cache/lambda-memory-<env>.json`, the name escaped as the costs cache's.
pub fn kept_path(env: &str) -> Option<std::path::PathBuf> {
    let name = format!("lambda-memory-{}.json", crate::costs::file_name(env));
    Some(crate::util::home_dir()?.join(".deployment-cli").join("cache").join(name))
}

/// Keeps `uses` as the latest memory analysis of their functions in `env` (private file; the
/// other functions' analyses stay).
pub fn keep(env: &str, uses: &[MemoryUse]) -> std::io::Result<()> {
    let path = kept_path(env).ok_or_else(|| std::io::Error::other("cannot find the home directory"))?;
    keep_at(&path, env, uses, now_secs())
}

fn keep_at(path: &std::path::Path, env: &str, uses: &[MemoryUse], now: f64) -> std::io::Result<()> {
    if uses.is_empty() {
        return Ok(());
    }
    let _one = crate::platform::lock(path)?;
    // A file that exists but cannot be read is left for the user, never written over.
    let file = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<Kept>(&text)
            .map_err(|e| std::io::Error::other(format!("{} is not valid ({e}): fix or delete it", path.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Kept::default(),
        Err(e) => return Err(e),
    };
    // Another environment's (`Prod` and `prod` share a file where names ignore case), or read
    // in another account or region than these: not this environment's any more.
    let mut all = if file.env == env { file.uses } else { Vec::new() };
    let elsewhere = |old: &MemoryUse| {
        uses.iter().any(|u| {
            (!u.account.is_empty() && !old.account.is_empty() && u.account != old.account)
                || (!u.region.is_empty() && !old.region.is_empty() && u.region != old.region)
        })
    };
    all.retain(|old| {
        !uses.iter().any(|u| u.function == old.function) && now - old.at <= KEPT_AT_MOST_DAYS * 86_400.0 && !elsewhere(old)
    });
    all.extend(uses.iter().cloned());
    all.sort_by(|a, b| b.at.total_cmp(&a.at));
    all.truncate(KEPT_MAX);
    all.sort_by(|a, b| a.function.cmp(&b.function));
    let kept = Kept { env: env.to_string(), uses: all };
    crate::platform::write_private(path, serde_json::to_string(&kept).unwrap_or_default().as_bytes())
}

/// Where an environment reads now: its region, and its account when known.
#[derive(Debug, Clone, Copy)]
pub struct Place<'a> {
    pub region: &'a str,
    pub account: Option<&'a str>,
}

impl<'a> Place<'a> {
    /// `aws`'s region, and the account of the costs read for `env` when they were.
    pub fn of(aws: &'a Aws, account: Option<&'a str>) -> Place<'a> {
        Place { region: &aws.region, account }
    }

    /// Whether an analysis was read here (an unknown account or region does not rule it out).
    fn holds(&self, u: &MemoryUse) -> bool {
        (u.region.is_empty() || u.region == self.region) && (u.account.is_empty() || self.account.is_none_or(|a| a == u.account))
    }
}

/// The account of the costs read for `env`, if they were.
pub fn costs_account(env: &str) -> Option<String> {
    crate::costs::cached(env).map(|c| c.account).filter(|a| crate::aws::sts::is_account_id(a))
}

/// The memory analyses kept for `env` and read where it reads now, checked: the file could
/// have been edited.
pub fn kept(env: &str, place: Place) -> Vec<MemoryUse> {
    kept_path(env).map(|p| kept_at(&p, env, place, now_secs())).unwrap_or_default()
}

fn kept_at(path: &std::path::Path, env: &str, place: Place, now: f64) -> Vec<MemoryUse> {
    let Some(file) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Kept>(&t).ok()) else {
        return Vec::new();
    };
    if file.env != env {
        return Vec::new();
    }
    let ok = |v: f64| v.is_finite() && v >= 0.0;
    file.uses
        .into_iter()
        .filter(|u| {
            ok(u.p99_mb)
                && ok(u.max_mb)
                && ok(u.samples)
                && ok(u.invocations)
                && ok(u.avg_billed_ms)
                && (128..=10_240).contains(&u.memory_mb)
                && (1..=455).contains(&u.days)
                && u.at.is_finite()
                && u.at > 0.0
                && u.at <= now + 86_400.0
                && !u.function.is_empty()
                && u.function.len() <= 170
                && !u.function.chars().any(crate::util::is_unsafe)
                && place.holds(u)
        })
        .map(|u| MemoryUse {
            // What Lambda reports never exceeds the memory assigned.
            p99_mb: u.p99_mb.min(u.memory_mb as f64),
            max_mb: u.max_mb.min(u.memory_mb as f64),
            ..u
        })
        .take(KEPT_MAX)
        .collect()
}

/// Of `kept`, the analyses of the last [`KEPT_DAYS`] of functions that still exist with the
/// memory and architecture they had then.
fn fresh_kept(kept: &[MemoryUse], functions: &[FunctionConfiguration], now: f64) -> Vec<MemoryUse> {
    let by_name: HashMap<&str, &FunctionConfiguration> = functions.iter().map(|f| (f.function_name.as_str(), f)).collect();
    kept.iter()
        .filter(|u| now - u.at <= KEPT_DAYS * 86_400.0)
        .filter(|u| by_name.get(u.function.as_str()).is_some_and(|f| memory_mb(f) == u.memory_mb && on_x86(f) != u.arm64))
        .cloned()
        .collect()
}

/// The memory findings of kept analyses, each saying when it was read.
fn kept_findings(kept: &[MemoryUse]) -> Vec<Finding> {
    kept.iter()
        .filter_map(|u| {
            let (y, m, d, ..) = crate::util::utc_parts(u.at as i64);
            memory_finding(u).map(|f| f.evidence(format!("from the memory analysis of {y:04}-{m:02}-{d:02} (Logs Insights)")))
        })
        .collect()
}

/// Makes the analyses kept for `env` known to [`remembered`] (the Lambda module's hints).
pub fn preload(aws: &Aws, env: &str) {
    let now = now_secs();
    let account = costs_account(env);
    let fresh: Vec<MemoryUse> = kept(env, Place::of(aws, account.as_deref())).into_iter().filter(|u| now - u.at <= KEPT_DAYS * 86_400.0).collect();
    let mut map = memory().lock().unwrap();
    for u in fresh {
        // An analysis of this run, newer, stays.
        map.entry((aws.profile.clone(), aws.region.clone(), u.function.clone())).or_insert(u);
    }
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
        // Priced as the difference between the x86_64 and arm64 rates of the bill.
        assert_eq!(f.billed.len(), 1);
        assert_eq!((f.billed[0].usage.as_str(), f.billed[0].instead.as_deref()), ("Lambda-GB-Second", Some("Lambda-GB-Second-ARM")));
        assert!((f.billed[0].quantity - 4.9e6).abs() < 0.1e6);
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
        assert_eq!(found.billed[0].usage, "Lambda-Provisioned-Concurrency-ARM", "an arm64 function");
        assert_eq!(provisioned_finding(&f, "live", 5, 0.0, 14).unwrap().recommendation, "Remove it from live: nothing used it");
        assert!(provisioned_finding(&f, "live", 50, 0.6, 14).is_none());
    }

    fn used(memory: i64, p99: f64, max: f64) -> MemoryUse {
        MemoryUse {
            function: "f".into(),
            arm64: false,
            memory_mb: memory,
            p99_mb: p99,
            max_mb: max,
            samples: 1000.0,
            invocations: 1_000_000.0,
            avg_billed_ms: 1000.0,
            days: 14,
            at: 0.0,
            ..Default::default()
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
        let end = 1_793_000_000 / 3600 * 3600;
        let e = Estimate {
            bytes: 10.0,
            days: 4,
            daily: vec![1.0, 2.0, 3.0, 4.0],
            per_function: vec![6.0, 4.0],
            window: (end - 4 * 86_400, end),
            ..Default::default()
        };
        let two = e.with_days(2);
        assert_eq!((two.bytes, two.days), (7.0, 2));
        // The window the analysis reads is the one measured, and each function scales with it.
        assert_eq!(two.window, (end - 2 * 86_400, end));
        assert!((two.per_function[0] - 4.2).abs() < 1e-9 && (two.per_function[1] - 2.8).abs() < 1e-9, "{:?}", two.per_function);
        assert_eq!((e.with_days(14).bytes, e.with_days(14).days), (10.0, 4));
        assert_eq!(e.with_days(1).describe(), "0 functions · ≈ 0 MB of logs over 1 day");
    }

    #[test]
    fn windows_end_on_a_whole_hour_before_now() {
        let now = 1_793_001_234.0;
        let (start, end) = window(7, now);
        assert_eq!(end % 3600, 0);
        assert!(f64::from(end as i32) <= now - LAG_SECS as f64 && now - end as f64 <= 3600.0 + LAG_SECS as f64);
        assert_eq!(end - start, 7 * 86_400);
        // Text REPORT lines and JSON platform.report events are both read.
        assert!(MEMORY_QUERY.contains("@type = \"REPORT\"") && MEMORY_QUERY.contains("type = \"platform.report\""));
        assert!(MEMORY_QUERY.contains("record.metrics.maxMemoryUsedMB"));
    }

    fn results(status: &str, bytes: f64) -> logs::QueryResults {
        logs::QueryResults { status: status.into(), statistics: logs::QueryStatistics { bytes_scanned: bytes, ..Default::default() }, ..Default::default() }
    }

    /// Polls a query whose figures come from `seq` (one per call), counting the stops.
    fn polled(seq: Vec<aws::Result<logs::QueryResults>>, prior: f64, budget: Option<f64>, expected: f64) -> (Waited, usize) {
        let seq = std::cell::RefCell::new(seq.into_iter());
        let stops = std::cell::Cell::new(0);
        let waited = poll(
            "q",
            prior,
            budget,
            expected,
            |_| seq.borrow_mut().next().unwrap_or_else(|| Ok(results("Cancelled", 0.0))),
            |_| {
                stops.set(stops.get() + 1);
                true
            },
            |_| {},
        );
        (waited, stops.get())
    }

    #[test]
    fn queries_stop_at_the_budget_and_on_errors() {
        // Complete within the budget: what it scanned, not stopped.
        let (w, stops) = polled(vec![Ok(results("Running", 10.0)), Ok(results("Complete", 50.0))], 0.0, Some(100.0), 60.0);
        assert_eq!((w.bytes, stops, w.stopped.is_none()), (50.0, 0, true));
        // Reaching the budget (with what earlier queries scanned) stops it, and its last figures count.
        let (w, stops) =
            polled(vec![Ok(results("Running", 30.0)), Ok(results("Running", 70.0)), Ok(results("Cancelled", 72.0))], 30.0, Some(100.0), 60.0);
        assert_eq!((w.bytes, stops), (72.0, 1));
        assert!(w.stopped.unwrap().contains("GB allowed"));
        // Figures that cannot be read: stopped, and the estimate counts when none came.
        let (w, stops) = polled(vec![Err(aws::AwsError::Network("timeout".into()))], 0.0, Some(100.0), 60.0);
        assert_eq!((w.bytes, stops), (60.0, 1));
        assert!(w.stopped.unwrap().contains("could not be read"));
        let (w, _) = polled(vec![Ok(results("Running", 20.0)), Err(aws::AwsError::Network("x".into()))], 0.0, None, 60.0);
        assert_eq!(w.bytes, 20.0, "what it had scanned when last seen");
        // A query that failed on its own says so.
        let (w, stops) = polled(vec![Ok(results("Failed", 5.0))], 0.0, Some(100.0), 60.0);
        assert_eq!((w.bytes, stops), (5.0, 0));
        assert!(w.stopped.unwrap().contains("ended failed"));
    }

    #[test]
    fn memory_findings_count_gb_seconds() {
        let f = memory_finding(&used(3008, 410.0, 420.0)).unwrap();
        // 2.17 M invocations a month × 1 s × (3008 − 640) / 1024 GB.
        assert_eq!(f.reduction, "−5 M GB-s a month (−79 %)");
        assert_eq!(f.recommendation, "Set the memory to 640 MB");
        assert_eq!(f.impact, Impact::High);
        assert_eq!(f.kind, "memory");
        assert!(memory_finding(&used(1024, 900.0, 950.0)).is_none());
    }

    #[test]
    fn analyses_are_kept_per_environment_and_checked_when_read() {
        let dir = std::env::temp_dir().join(format!("dcli-lambda-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("lambda-memory-prod.json");
        let now = 1_793_000_000.0;
        let here = Place { region: "us-east-1", account: Some("111111111111") };
        let at = |u: MemoryUse| MemoryUse { account: "111111111111".into(), region: "us-east-1".into(), ..u };
        let a = at(MemoryUse { function: "a".into(), at: now - 86_400.0, ..used(3008, 410.0, 420.0) });
        let b = at(MemoryUse { function: "b".into(), at: now - 86_400.0, ..used(1024, 200.0, 220.0) });
        keep_at(&path, "prod", &[a.clone(), b.clone()], now).unwrap();
        // A later analysis of `a` replaces it; `b` stays.
        let a2 = MemoryUse { p99_mb: 500.0, at: now, ..a.clone() };
        keep_at(&path, "prod", std::slice::from_ref(&a2), now).unwrap();
        assert_eq!(kept_at(&path, "prod", here, now), [a2.clone(), b.clone()]);
        assert!(kept_at(&path, "dev", here, now).is_empty(), "another environment's file is not read");
        // Read in another account or region: not this environment's any more.
        assert!(kept_at(&path, "prod", Place { region: "us-east-1", account: Some("222222222222") }, now).is_empty());
        assert!(kept_at(&path, "prod", Place { region: "eu-west-1", account: None }, now).is_empty());
        assert_eq!(kept_at(&path, "prod", Place { region: "us-east-1", account: None }, now).len(), 2, "an unknown account rules nothing out");
        // `Prod` and `prod` share a file where names ignore case: one's analyses never become the other's.
        keep_at(&path, "Prod", &[at(MemoryUse { function: "c".into(), at: now, ..used(1024, 200.0, 220.0) })], now).unwrap();
        assert!(kept_at(&path, "prod", here, now).is_empty());
        assert_eq!(kept_at(&path, "Prod", here, now).iter().map(|u| u.function.as_str()).collect::<Vec<_>>(), ["c"]);
        // The environment now reads another account: what was read before goes.
        keep_at(&path, "Prod", &[at(a.clone())], now).unwrap();
        let moved = MemoryUse { account: "222222222222".into(), ..a.clone() };
        keep_at(&path, "Prod", &[MemoryUse { function: "d".into(), ..moved }], now).unwrap();
        assert_eq!(kept_at(&path, "Prod", Place { region: "us-east-1", account: None }, now).len(), 1);
        keep_at(&path, "prod", &[a2.clone(), b.clone()], now).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // An edited file: bad figures are dropped, memory used never above the memory assigned.
        let edited = serde_json::json!({ "env": "prod", "uses": [
            { "function": "c", "memoryMb": 1024, "p99Mb": 9999.0, "maxMb": 9999.0, "samples": 100.0, "invocations": 100.0, "avgBilledMs": 10.0, "days": 7, "at": now },
            { "function": "d", "memoryMb": 1024, "p99Mb": -1.0, "days": 7, "at": now },
            { "function": "e\u{1b}[2J", "memoryMb": 1024, "days": 7, "at": now },
            { "function": "f", "memoryMb": 99999, "days": 7, "at": now },
            { "function": "g", "memoryMb": 1024, "days": 7, "at": now + 9e9 },
        ]});
        std::fs::write(&path, edited.to_string()).unwrap();
        let read = kept_at(&path, "prod", here, now);
        assert_eq!(read.iter().map(|u| u.function.as_str()).collect::<Vec<_>>(), ["c"]);
        assert_eq!((read[0].p99_mb, read[0].max_mb), (1024.0, 1024.0));
        // A file that cannot be read is never written over.
        std::fs::write(&path, "not json").unwrap();
        assert!(keep_at(&path, "prod", &[a], now).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kept_analyses_make_findings_while_they_still_hold() {
        let now = 1_793_000_000.0;
        let day = 86_400.0;
        let fresh = MemoryUse { function: "a".into(), at: now - 2.0 * day, ..used(3008, 410.0, 420.0) };
        let old = MemoryUse { function: "b".into(), at: now - 40.0 * day, ..used(3008, 410.0, 420.0) };
        let resized = MemoryUse { function: "c".into(), at: now - day, ..used(3008, 410.0, 420.0) };
        let moved = MemoryUse { function: "d".into(), at: now - day, ..used(3008, 410.0, 420.0) };
        let functions =
            [function("a", 3008, "x86_64", 30), function("b", 3008, "x86_64", 30), function("c", 1024, "x86_64", 30), function("d", 3008, "arm64", 30)];
        let still = fresh_kept(&[fresh, old, resized, moved], &functions, now);
        assert_eq!(still.iter().map(|u| u.function.as_str()).collect::<Vec<_>>(), ["a"], "not too old, same memory and architecture");
        let found = kept_findings(&still);
        assert_eq!(found.len(), 1);
        assert!(found[0].evidence.iter().any(|e| e == "from the memory analysis of 2026-10-24 (Logs Insights)"), "{:?}", found[0].evidence);
    }

    #[test]
    fn estimates_keep_each_function_s_share() {
        let e = Estimate {
            functions: vec![function("a", 128, "x86_64", 3), function("b", 128, "x86_64", 3), function("c", 128, "x86_64", 3)],
            per_function: vec![10.0, 30.0, 60.0],
            bytes: 100.0,
            days: 2,
            daily: vec![40.0, 60.0],
            ..Default::default()
        };
        let only = e.only(&["c".to_string(), "a".to_string(), "zz".to_string()]);
        assert_eq!(only.functions.iter().map(|f| f.function_name.as_str()).collect::<Vec<_>>(), ["c", "a"]);
        assert_eq!((only.bytes, only.per_function.clone()), (70.0, vec![60.0, 10.0]));
        assert_eq!(only.daily, [28.0, 42.0]);
    }
}

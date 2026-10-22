//! `dcli dms`: DMS replication tasks with their tables and CDC latency, replication
//! instances and endpoints; resuming, reloading and stopping tasks, scaling an instance,
//! testing an endpoint's connection, and a Markdown report of a task for an AI agent.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::aws::Aws;
use crate::aws::dms::{self, Endpoint, Instance, PrepMode, Replication, StartType, TableState, TableStat};
use crate::aws::lambda::Range;
use crate::cli::{ask, aws_for, confirm_write, env_and, interactive, paint, print_json};
use crate::config::Config;
use crate::tui::chart::{Agg, columns, fmt_count, sparkline};
use crate::util::{ellipsize, fmt_ago, fmt_bytes, now_secs, program};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Task identifier; without it, lists the tasks (exit code 1 when one failed or has
    /// tables in error)
    task: Option<String>,
    /// The task's tables: state, rows loaded, changes applied, validation
    #[arg(long, group = "view")]
    tables: bool,
    /// --tables: only those in this state (error, loading, queued, completed, cancelled), or
    /// with validation problems (validation)
    #[arg(long, value_name = "STATE")]
    state: Option<String>,
    /// CDC latency, changes waiting and on disk, the instance's memory and CPU, over --since
    #[arg(long, group = "view")]
    metrics: bool,
    /// Window for --metrics: 1h, 24h or 7d
    #[arg(long, default_value = "24h", value_name = "WINDOW")]
    since: String,
    /// Resume the task where it stopped (or start it the first time)
    #[arg(long, group = "view")]
    resume: bool,
    /// Load tables again: every table of the task (asks you to type its name), the tables
    /// listed (schema.table,…), or the ones in error (errors)
    #[arg(long, group = "view", num_args = 0..=1, value_name = "TABLES")]
    reload: Option<Option<String>>,
    /// --reload of tables: only validate them again, without reloading their rows
    #[arg(long, requires = "reload")]
    validate_only: bool,
    /// Stop the task
    #[arg(long, group = "view")]
    stop: bool,
    /// The replication instances: class, status, tasks on each
    #[arg(long, group = "view")]
    instances: bool,
    /// The endpoints and their last connection tests
    #[arg(long, group = "view")]
    endpoints: bool,
    /// Change the class of this replication instance (with --class); it restarts, and its
    /// tasks are interrupted meanwhile
    #[arg(long, group = "view", value_name = "INSTANCE", requires = "class")]
    scale: Option<String>,
    /// --scale: the new class (dms.r5.xlarge)
    #[arg(long, value_name = "CLASS")]
    class: Option<String>,
    /// --scale: apply now instead of in the next maintenance window
    #[arg(long)]
    now: bool,
    /// Test the connection from a replication instance to this endpoint
    #[arg(long, group = "view", value_name = "ENDPOINT")]
    test_endpoint: Option<String>,
    /// --test-endpoint: the instance to test from (default: the one its tasks use)
    #[arg(long, value_name = "INSTANCE")]
    instance: Option<String>,
    /// Markdown report of the task for an AI agent: failure, tables in error, latency, log errors, settings
    #[arg(long, group = "view")]
    report: bool,
    /// Do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let (env, task) = env_and(config, args.env.clone(), args.task.clone())?;
    let aws = aws_for(config, &env)?;
    if args.instances {
        return instances(&aws, args.json);
    }
    if args.endpoints {
        return endpoints(&aws, args.json);
    }
    if let Some(instance) = &args.scale {
        return scale(&aws, &env, instance, args.class.as_deref().unwrap_or_default(), args.now, args.yes);
    }
    if let Some(endpoint) = &args.test_endpoint {
        return test_endpoint(&aws, &env, endpoint, args.instance.as_deref(), args.json, args.yes);
    }
    let Some(task) = task else {
        if args.tables || args.metrics || args.resume || args.reload.is_some() || args.stop || args.report {
            bail!("which task? {} dms [ENV] TASK …", program());
        }
        return list(&aws, &env, args.json);
    };
    if args.report {
        print!("{}", dms::gather_report(&aws, &env, &aws.region, &task, now_secs())?);
        return Ok(ExitCode::SUCCESS);
    }
    let all = dms::list_instances(&aws)?;
    let r = dms::find(&aws, &task, &all)?;
    if args.tables {
        return tables(&aws, &r, args.state.as_deref(), args.json);
    }
    if args.metrics {
        let range = Range::parse(&args.since).ok_or_else(|| anyhow!("--since takes 1h, 24h or 7d"))?;
        return metrics(&aws, &r, &all, range, args.json);
    }
    if args.resume {
        return resume(&aws, &env, &r, args.yes);
    }
    if let Some(which) = &args.reload {
        return match which.as_deref() {
            None => reload_task(&aws, &env, &r, args.yes),
            Some(list) => reload_tables(&aws, &env, &r, list, args.validate_only, args.yes),
        };
    }
    if args.stop {
        return stop(&aws, &env, &r, args.yes);
    }
    show(&aws, &r, &all, args.json)
}

fn health_paint(r: &Replication) -> String {
    let code = match r.health() {
        dms::Health::Failed | dms::Health::TableErrors => "31",
        dms::Health::Running => "32",
        dms::Health::Changing => "94",
        _ => "90",
    };
    paint(&r.status_label(), code)
}

fn task_json(r: &Replication) -> Value {
    json!({
        "id": r.id,
        "arn": r.arn,
        "status": r.status,
        "migrationType": r.migration.api(),
        "instance": r.instance,
        "fullLoadPercent": r.stats.full_load_percent,
        "tablesLoaded": r.stats.tables_loaded,
        "tablesLoading": r.stats.tables_loading,
        "tablesQueued": r.stats.tables_queued,
        "tablesErrored": r.stats.tables_errored,
        "lastFailure": r.last_failure,
        "stopReason": r.stop_reason,
        "started": r.stats.start,
        "stopped": r.stopped_at(),
        "sourceEndpointArn": r.source_arn,
        "targetEndpointArn": r.target_arn,
    })
}

fn list(aws: &Aws, env: &str, json: bool) -> Result<ExitCode> {
    let instances = dms::list_instances(aws)?;
    let tasks = dms::list_replications(aws, &instances, false)?;
    let trouble = tasks.iter().any(Replication::in_trouble);
    let code = if trouble { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    if json {
        print_json(&Value::Array(tasks.iter().map(task_json).collect()));
        return Ok(code);
    }
    if tasks.is_empty() {
        println!("No DMS tasks in {env}.");
        return Ok(code);
    }
    let width = tasks.iter().map(|t| t.id.chars().count()).max().unwrap_or(4).max(4) + 2;
    let iw = tasks.iter().filter_map(|t| t.instance.as_ref()).map(|i| i.chars().count()).max().unwrap_or(8).max(8) + 2;
    println!("{}", paint(&format!("{:<width$}{:<8}{:<26}{:<iw$}{}", "task", "type", "status", "instance", "tables"), "90"));
    for t in &tasks {
        let status = format!("{:<26}", t.status_label());
        let status = status.replacen(&t.status_label(), &health_paint(t), 1);
        let s = &t.stats;
        let tables = if s.tables_errored > 0 {
            paint(&format!("{} in error · {} loaded", s.tables_errored, s.tables_loaded), "31")
        } else {
            format!("{} loaded{}", s.tables_loaded, if s.tables_loading + s.tables_queued > 0 { format!(" · {} to go", s.tables_loading + s.tables_queued) } else { String::new() })
        };
        println!("{:<width$}{:<8}{status}{:<iw$}{tables}", t.id, t.migration.short(), t.instance.as_deref().unwrap_or("?"));
        if let Some(f) = &t.last_failure {
            println!("{:<width$}{}", "", paint(&ellipsize(f, 110), "31"));
        }
    }
    Ok(code)
}

fn show(aws: &Aws, r: &Replication, all: &[Instance], json: bool) -> Result<ExitCode> {
    let instance = all.iter().find(|i| Some(&i.arn) == r.instance_arn.as_ref());
    let endpoints = dms::list_endpoints(aws).unwrap_or_default();
    let m = dms::metrics(aws, r, Range::Hour, now_secs()).ok();
    let diagnosis = m.as_ref().map(|m| dms::diagnose(r, m, instance.and_then(Instance::memory_bytes))).unwrap_or_default();
    if json {
        let mut out = task_json(r);
        out["diagnosis"] = json!(diagnosis.iter().map(|d| d.text.clone()).collect::<Vec<_>>());
        out["settings"] = json!(r.settings.as_deref().map(dms::settings_summary));
        out["tableMappings"] = json!(r.table_mappings.as_deref().map(dms::mapping_lines));
        print_json(&out);
        return Ok(ExitCode::SUCCESS);
    }
    let kv = |k: &str, v: String| println!("  {} {v}", paint(&format!("{k:<14}"), "90"));
    println!("{}  {}", paint(&r.id, "1"), health_paint(r));
    kv("Type", r.migration.label().into());
    kv("Instance", instance.map_or_else(|| r.instance.clone().unwrap_or_else(|| "?".into()), |i| format!("{} ({})", i.id, i.class_label())));
    let endpoint = |arn: &str| match endpoints.iter().find(|e| e.arn == arn) {
        Some(e) => format!("{} · {} {}", e.id, e.engine_label, e.address()),
        None => dms::resource_id(arn),
    };
    kv("Source", endpoint(&r.source_arn));
    kv("Target", endpoint(&r.target_arn));
    let s = &r.stats;
    if r.migration.has_full_load() {
        kv(
            "Full load",
            format!(
                "{} · {} loaded, {} loading, {} queued, {} in error",
                s.full_load_percent.map_or_else(|| "?".into(), |p| format!("{p:.0}%")),
                s.tables_loaded,
                s.tables_loading,
                s.tables_queued,
                s.tables_errored
            ),
        );
    }
    if let Some(t) = s.start {
        kv("Started", fmt_ago(t));
    }
    if let Some(t) = r.stopped_at() {
        kv("Stopped", fmt_ago(t));
    }
    if let Some(reason) = &r.stop_reason {
        kv("Stop reason", reason.clone());
    }
    if let Some(f) = &r.last_failure {
        kv("Last failure", paint(f, "31"));
    }
    if !diagnosis.is_empty() {
        println!();
        for d in &diagnosis {
            let code = match d.severity {
                dms::Severity::Good => "32",
                dms::Severity::Warn => "33",
                dms::Severity::Bad => "31",
            };
            println!("  {} {}", paint("●", code), d.text);
        }
    }
    if let Some(settings) = &r.settings {
        println!();
        for (k, v) in dms::settings_summary(settings) {
            kv(&k, v);
        }
    }
    if let Some(mappings) = &r.table_mappings {
        println!("\n  {}", paint("Table mappings", "90"));
        for line in dms::mapping_lines(mappings) {
            println!("    {line}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Which tables `--state` keeps.
type TableTest = Box<dyn Fn(&TableStat) -> bool>;

fn state_filter(state: &str) -> Result<TableTest> {
    let wanted = match state.to_lowercase().as_str() {
        "validation" => return Ok(Box::new(TableStat::validation_problem)),
        "error" | "errors" => TableState::Error,
        "loading" => TableState::Loading,
        "queued" | "before" => TableState::Queued,
        "completed" | "done" => TableState::Completed,
        "cancelled" => TableState::Cancelled,
        other => bail!("--state takes error, loading, queued, completed, cancelled or validation (not {other})"),
    };
    Ok(Box::new(move |t: &TableStat| t.state() == wanted))
}

fn tables(aws: &Aws, r: &Replication, state: Option<&str>, json: bool) -> Result<ExitCode> {
    let filter = state.map(state_filter).transpose()?;
    let list: Vec<TableStat> = dms::table_statistics(aws, r)?.into_iter().filter(|t| filter.as_ref().is_none_or(|f| f(t))).collect();
    if json {
        let out: Vec<Value> = list
            .iter()
            .map(|t| {
                json!({
                    "schema": t.schema_name, "table": t.table_name, "state": t.table_state, "fullLoadRows": t.full_load_rows,
                    "fullLoadErrorRows": t.full_load_error_rows, "inserts": t.inserts, "updates": t.updates, "deletes": t.deletes,
                    "ddls": t.ddls, "validationState": t.validation_state, "validationFailedRecords": t.validation_failed_records,
                    "validationPendingRecords": t.validation_pending_records, "lastUpdate": t.last_update_time,
                })
            })
            .collect();
        print_json(&Value::Array(out));
        return Ok(ExitCode::SUCCESS);
    }
    if list.is_empty() {
        println!("No tables{}.", state.map(|s| format!(" in state {s}")).unwrap_or_default());
        return Ok(ExitCode::SUCCESS);
    }
    let width = list.iter().map(|t| t.name().chars().count()).max().unwrap_or(5).clamp(5, 48) + 2;
    println!("{}", paint(&format!("{:<width$}{:<13}{:>12}  {:<22}{}", "table", "state", "loaded rows", "CDC ins/upd/del", "validation"), "90"));
    for t in &list {
        let state = format!("{:<13}", t.state().label());
        let state = match t.state() {
            TableState::Error => paint(&state, "31"),
            TableState::Loading | TableState::Queued => paint(&state, "94"),
            _ => state,
        };
        let cdc = format!("{}/{}/{}", fmt_count(t.inserts as f64), fmt_count(t.updates as f64), fmt_count(t.deletes as f64));
        let validation = if t.validation_problem() { paint(&t.validation_label(), "33") } else { paint(&t.validation_label(), "90") };
        println!("{:<width$}{state}{:>12}  {cdc:<22}{validation}", ellipsize(&t.name(), width - 2), fmt_count(t.full_load_rows as f64));
    }
    Ok(ExitCode::SUCCESS)
}

fn metrics(aws: &Aws, r: &Replication, all: &[Instance], range: Range, json: bool) -> Result<ExitCode> {
    let m = dms::metrics(aws, r, range, now_secs())?;
    if json {
        let series: serde_json::Map<String, Value> = m.series.iter().map(|s| (s.id.clone(), json!(s.points))).collect();
        print_json(&json!({ "task": r.id, "window": [m.window.0, m.window.1], "series": series }));
        return Ok(ExitCode::SUCCESS);
    }
    const WIDTH: usize = 48;
    let line = |label: &str, id: &str, agg: Agg, numbers: String, color: &str| {
        let spark = sparkline(&columns(m.points(id), m.window, WIDTH, agg));
        println!("  {:<18} {}  {}", paint(label, "1"), paint(&spark, color), paint(&numbers, "90"));
    };
    let latest = |id: &str, fmt: fn(f64) -> String| match (m.last(id), m.max(id)) {
        (Some(l), Some(x)) => format!("now {} · max {}", fmt(l), fmt(x)),
        _ => "no data".into(),
    };
    println!("{} · last {}", paint(&r.id, "1"), range.label());
    if r.migration.has_cdc() {
        line("Latency, target", "latency_target", Agg::Max, latest("latency_target", dms::fmt_secs), "94");
        line("Latency, source", "latency_source", Agg::Max, latest("latency_source", dms::fmt_secs), "36");
        line("Changes waiting", "incoming", Agg::Max, latest("incoming", fmt_count), "36");
        line("On disk, target", "disk_target", Agg::Max, latest("disk_target", fmt_count), "33");
        line("On disk, source", "disk_source", Agg::Max, latest("disk_source", fmt_count), "33");
        line("Rows applied/s", "rows_target", Agg::Mean, latest("rows_target", fmt_count), "32");
    }
    if r.loading() {
        line("Full load rows/s", "full_load_rows", Agg::Mean, latest("full_load_rows", fmt_count), "32");
    }
    if r.instance.is_some() {
        let bytes = |v: f64| fmt_bytes(v as i64);
        line("Freeable memory", "i_freeable", Agg::Mean, latest("i_freeable", bytes), "35");
        line("Swap", "i_swap", Agg::Max, latest("i_swap", bytes), "35");
        line("CPU", "i_cpu", Agg::Max, latest("i_cpu", |v| format!("{v:.0}%")), "35");
    }
    let instance = all.iter().find(|i| Some(&i.arn) == r.instance_arn.as_ref());
    let diagnosis = dms::diagnose(r, &m, instance.and_then(Instance::memory_bytes));
    if !diagnosis.is_empty() {
        println!();
        for d in diagnosis {
            println!("  {}", d.text);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Asks before a write: y/N, or typing the environment name for production-looking ones.
fn confirm(env: &str, question: &str, yes: bool) -> Result<()> {
    if !confirm_write(env, question, yes)? {
        bail!("cancelled");
    }
    Ok(())
}

fn resume(aws: &Aws, env: &str, r: &Replication, yes: bool) -> Result<ExitCode> {
    if r.running() {
        println!("{} is already {}", r.id, r.status);
        return Ok(ExitCode::SUCCESS);
    }
    if r.finished_full_load() {
        bail!("{} is a full load that finished: there is nothing to resume. --reload loads it again", r.id);
    }
    let how = r.start_type();
    if let Some(warning) = resume_warning(r, now_secs()) {
        eprintln!("{}", paint(&warning, "33"));
    }
    let prep = PrepMode::of(r.settings.as_deref());
    if how == StartType::Start && prep.rewrites_target() {
        // A first start loads the tables like a full reload: the same name to type.
        eprintln!("Starting {} loads every table from the source. It {}.", r.id, prep.effect());
        type_task_name(r, env, "start it", yes)?;
    } else {
        confirm(env, &format!("{} {} ({})?", how.verb(), r.id, r.migration.label()), yes)?;
    }
    let fresh = dms::start(aws, r, how)?;
    println!("{} {} is {}", paint("✓", "32"), fresh.id, fresh.status);
    Ok(ExitCode::SUCCESS)
}

/// Days stopped from which a CDC task may no longer find the source's change log.
const LONG_STOP_DAYS: f64 = 1.0;

/// What to know before resuming a CDC task that stopped long ago.
pub fn resume_warning(r: &Replication, now: f64) -> Option<String> {
    let stopped = r.stopped_at()?;
    let days = (now - stopped) / 86_400.0;
    if !r.migration.has_cdc() || days < LONG_STOP_DAYS {
        return None;
    }
    let checkpoint = r.recovery_checkpoint.as_deref().map(|c| format!(" It resumes from checkpoint {c}.")).unwrap_or_default();
    Some(format!(
        "{} stopped {} ago: resuming needs the source to still keep its change log (binlog, WAL, redo) from then.{checkpoint}",
        r.id,
        dms::fmt_since(now - stopped)
    ))
}

/// Asks for the task's name (whatever the environment) before what rewrites its target.
fn type_task_name(r: &Replication, env: &str, what: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !interactive() {
        bail!("refusing to {what} without confirmation; pass --yes");
    }
    if ask(&format!("Type the task name '{}' to {what} ({env}):", r.id)) != r.id {
        bail!("cancelled");
    }
    Ok(())
}

fn reload_task(aws: &Aws, env: &str, r: &Replication, yes: bool) -> Result<ExitCode> {
    let prep = PrepMode::of(r.settings.as_deref());
    eprintln!("Reloading {} loads every table again from the source. It {}.", r.id, prep.effect());
    type_task_name(r, env, "reload it", yes)?;
    let fresh = dms::start(aws, r, StartType::Reload)?;
    println!("{} {} is reloading ({})", paint("✓", "32"), fresh.id, fresh.status);
    Ok(ExitCode::SUCCESS)
}

fn reload_tables(aws: &Aws, env: &str, r: &Replication, list: &str, validate_only: bool, yes: bool) -> Result<ExitCode> {
    let tables: Vec<(String, String)> = if list.trim().eq_ignore_ascii_case("errors") {
        dms::table_statistics(aws, r)?.into_iter().filter(|t| t.state() == TableState::Error).map(|t| (t.schema_name, t.table_name)).collect()
    } else {
        list.split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| t.split_once('.').map(|(s, n)| (s.to_string(), n.to_string())).ok_or_else(|| anyhow!("tables are schema.table: {t}")))
            .collect::<Result<_>>()?
    };
    if tables.is_empty() {
        println!("No tables to reload.");
        return Ok(ExitCode::SUCCESS);
    }
    let names: Vec<String> = tables.iter().map(|(s, t)| format!("{s}.{t}")).collect();
    let what = if validate_only {
        "Validate again".to_string()
    } else {
        format!("Reload (the task {} first)", PrepMode::of(r.settings.as_deref()).effect())
    };
    confirm(env, &format!("{what}: {} in {}?", names.join(", "), r.id), yes)?;
    dms::reload_tables(aws, r, &tables, validate_only)?;
    println!("{} {} table{} {}", paint("✓", "32"), tables.len(), if tables.len() == 1 { "" } else { "s" }, if validate_only { "validating again" } else { "reloading" });
    Ok(ExitCode::SUCCESS)
}

fn stop(aws: &Aws, env: &str, r: &Replication, yes: bool) -> Result<ExitCode> {
    if !r.running() {
        println!("{} is {}, not running", r.id, r.status);
        return Ok(ExitCode::SUCCESS);
    }
    let note = if r.migration.has_cdc() { " Changes on the source wait until it resumes." } else { "" };
    confirm(env, &format!("Stop {}?{note}", r.id), yes)?;
    let fresh = dms::stop(aws, r)?;
    println!("{} {} is {}", paint("✓", "32"), fresh.id, fresh.status);
    Ok(ExitCode::SUCCESS)
}

fn instances(aws: &Aws, json: bool) -> Result<ExitCode> {
    let all = dms::list_instances(aws)?;
    let tasks = dms::list_replications(aws, &all, false)?;
    if json {
        let out: Vec<Value> = all
            .iter()
            .map(|i| {
                let on: Vec<&str> = tasks.iter().filter(|t| t.instance_arn.as_ref() == Some(&i.arn)).map(|t| t.id.as_str()).collect();
                json!({
                    "id": i.id, "arn": i.arn, "class": i.class, "status": i.status, "engineVersion": i.engine_version,
                    "storageGb": i.storage_gb, "multiAz": i.multi_az, "publiclyAccessible": i.public, "pendingClass": i.pending_class,
                    "tasks": on,
                })
            })
            .collect();
        print_json(&Value::Array(out));
        return Ok(ExitCode::SUCCESS);
    }
    if all.is_empty() {
        println!("No replication instances.");
        return Ok(ExitCode::SUCCESS);
    }
    let width = all.iter().map(|i| i.id.chars().count()).max().unwrap_or(8).max(8) + 2;
    println!("{}", paint(&format!("{:<width$}{:<34}{:<12}{}", "instance", "class", "status", "tasks"), "90"));
    for i in &all {
        let on: Vec<&Replication> = tasks.iter().filter(|t| t.instance_arn.as_ref() == Some(&i.arn)).collect();
        let running = on.iter().filter(|t| t.running()).count();
        let mut flags = Vec::new();
        if i.multi_az {
            flags.push("Multi-AZ".to_string());
        }
        if i.public {
            flags.push(paint("public", "33"));
        }
        if let Some(c) = &i.pending_class {
            flags.push(format!("→ {c} pending"));
        }
        println!(
            "{:<width$}{:<34}{:<12}{} of {} running {}",
            i.id,
            i.class_label(),
            i.status,
            running,
            on.len(),
            flags.join(" · ")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn endpoints(aws: &Aws, json: bool) -> Result<ExitCode> {
    let list = dms::list_endpoints(aws)?;
    let tests = dms::connections(aws).unwrap_or_default();
    if json {
        let out: Vec<Value> = list
            .iter()
            .map(|e| {
                let t: Vec<Value> = tests
                    .iter()
                    .filter(|c| c.endpoint_arn == e.arn)
                    .map(|c| json!({ "instance": c.replication_instance_identifier, "status": c.status, "failure": c.last_failure_message }))
                    .collect();
                json!({ "id": e.id, "role": e.role, "engine": e.engine, "address": e.address(), "credentials": e.credentials, "ssl": e.ssl_mode, "tests": t })
            })
            .collect();
        print_json(&Value::Array(out));
        return Ok(ExitCode::SUCCESS);
    }
    let width = list.iter().map(|e| e.id.chars().count()).max().unwrap_or(8).max(8) + 2;
    for e in &list {
        let tested: Vec<String> = tests
            .iter()
            .filter(|c| c.endpoint_arn == e.arn)
            .map(|c| {
                let code = match c.status.as_str() {
                    "successful" => "32",
                    "failed" => "31",
                    _ => "94",
                };
                paint(&format!("{} {}", c.replication_instance_identifier, c.status), code)
            })
            .collect();
        println!("{:<width$}{:<8}{:<26}{}", e.id, e.role, ellipsize(&e.engine_label, 24), e.address());
        if !tested.is_empty() {
            println!("{:<width$}{}", "", tested.join(" · "));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn scale(aws: &Aws, env: &str, id: &str, class: &str, now: bool, yes: bool) -> Result<ExitCode> {
    let all = dms::list_instances(aws)?;
    let instance = all.iter().find(|i| i.id == id || i.arn == id).ok_or_else(|| anyhow!("no replication instance named '{id}'"))?;
    if instance.class == class {
        println!("{} is already {class}", instance.id);
        return Ok(ExitCode::SUCCESS);
    }
    let classes = dms::orderable_classes(aws, &instance.engine_version)?;
    if !classes.iter().any(|c| c == class) {
        bail!("{class} is not available for engine {} here; choose one of: {}", instance.engine_version, classes.join(", "));
    }
    let tasks = dms::list_replications(aws, &all, false)?;
    let running: Vec<&str> = tasks.iter().filter(|t| t.instance_arn.as_ref() == Some(&instance.arn) && t.running()).map(|t| t.id.as_str()).collect();
    let when = if now {
        if running.is_empty() {
            "now: it restarts".to_string()
        } else {
            format!("now: it restarts, interrupting {}", running.join(", "))
        }
    } else {
        format!("in its next maintenance window ({})", instance.maintenance_window.as_deref().unwrap_or("?"))
    };
    confirm(env, &format!("Change {} from {} to {} {when}?", instance.id, dms::class_label(&instance.class), dms::class_label(class)), yes)?;
    let fresh = dms::modify_class(aws, instance, class, now)?;
    let state = match &fresh.pending_class {
        Some(c) if !now => format!("{c} pending until the maintenance window"),
        _ => fresh.status.clone(),
    };
    println!("{} {} · {state}", paint("✓", "32"), fresh.id);
    Ok(ExitCode::SUCCESS)
}

/// How long a connection test is followed.
const TEST_WAIT: Duration = Duration::from_secs(120);

fn test_endpoint(aws: &Aws, env: &str, id: &str, instance: Option<&str>, json: bool, yes: bool) -> Result<ExitCode> {
    let endpoints = dms::list_endpoints(aws)?;
    let endpoint: &Endpoint = endpoints.iter().find(|e| e.id == id || e.arn == id).ok_or_else(|| anyhow!("no DMS endpoint named '{id}'"))?;
    let all = dms::list_instances(aws)?;
    let instance = match instance {
        Some(name) => all.iter().find(|i| i.id == name || i.arn == name).ok_or_else(|| anyhow!("no replication instance named '{name}'"))?,
        None => {
            let tasks = dms::list_replications(aws, &all, false)?;
            let used = tasks.iter().find(|t| t.source_arn == endpoint.arn || t.target_arn == endpoint.arn).and_then(|t| t.instance_arn.clone());
            match used.and_then(|arn| all.iter().find(|i| i.arn == arn)).or(if all.len() == 1 { all.first() } else { None }) {
                Some(i) => i,
                None => bail!("which instance should test it? --instance NAME"),
            }
        }
    };
    // It changes nothing, but it signs in to the database with the endpoint's credentials,
    // which can count toward lockouts or raise alerts there.
    confirm(env, &format!("Test the connection from {} to {} (it signs in to {})?", instance.id, endpoint.id, endpoint.address()), yes)?;
    if !json {
        eprintln!("Testing {} from {}…", endpoint.id, instance.id);
    }
    dms::test_connection(aws, &instance.arn, &endpoint.arn)?;
    let started = std::time::Instant::now();
    let result = loop {
        std::thread::sleep(Duration::from_secs(3));
        let found = dms::connections(aws)?.into_iter().find(|c| c.endpoint_arn == endpoint.arn && c.replication_instance_arn == instance.arn);
        match found {
            Some(c) if c.status != "testing" => break c,
            _ if started.elapsed() >= TEST_WAIT => bail!("the test is still running after {} s; see {} dms --endpoints", TEST_WAIT.as_secs(), program()),
            _ => {}
        }
    };
    let ok = result.status == "successful";
    if json {
        print_json(&json!({ "endpoint": endpoint.id, "instance": instance.id, "status": result.status, "failure": result.last_failure_message }));
    } else if ok {
        println!("{} {} can reach {} ({})", paint("✓", "32"), instance.id, endpoint.id, endpoint.address());
    } else {
        println!("{} {}: {}", paint("✗", "31"), result.status, result.last_failure_message.unwrap_or_default());
    }
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::dms::Migration;

    #[test]
    fn long_stops_warn_before_resuming() {
        let r = Replication {
            kind: dms::Kind::Task,
            id: "billing-cdc".into(),
            arn: String::new(),
            resource_id: String::new(),
            instance: None,
            instance_arn: None,
            migration: Migration::Cdc,
            status: "stopped".into(),
            stats: dms::Stats { stop: Some(1_000.0), ..Default::default() },
            last_failure: None,
            stop_reason: None,
            source_arn: String::new(),
            target_arn: String::new(),
            table_mappings: None,
            settings: None,
            recovery_checkpoint: Some("checkpoint:V1#27".into()),
            cdc_start_position: None,
            created: None,
            started: None,
        };
        let warning = resume_warning(&r, 1_000.0 + 3.0 * 86_400.0).unwrap();
        assert!(warning.contains("stopped 3 days ago") && warning.contains("checkpoint:V1#27"), "{warning}");
        assert_eq!(resume_warning(&r, 1_000.0 + 3600.0), None);
        let full = Replication { migration: Migration::FullLoad, ..r };
        assert_eq!(resume_warning(&full, 1_000.0 + 3.0 * 86_400.0), None);
    }

    #[test]
    fn state_filters() {
        let t = TableStat { table_state: "Table error".into(), ..Default::default() };
        assert!(state_filter("error").unwrap()(&t));
        assert!(!state_filter("loading").unwrap()(&t));
        assert!(state_filter("nope").is_err());
    }
}

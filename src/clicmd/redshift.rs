//! `dcli redshift`: Redshift clusters, a cluster's nodes (CPU and disk, skew), metrics, the
//! queries running, queued and slowest with their locks, table health, events and snapshots,
//! a performance diagnosis for an AI agent; pause, resume, resize, manual snapshots and
//! cancelling a query.

use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use crate::aws::Aws;
use crate::aws::lambda::Range;
use crate::aws::redshift::{self, Cluster, Health, Query};
use crate::aws::redshift_data::{self, Identity};
use crate::cli::{aws_for, confirm_write, env_and, paint, print_json};
use crate::config::Config;
use crate::tui::chart::{Agg, columns, fmt_ms, sparkline, sparkline_to};
use crate::util::{ellipsize, fmt_ago, fmt_utc, now_secs, program};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Cluster identifier; without it, lists the clusters
    cluster: Option<String>,
    /// Metrics of the nodes and the cluster over --since
    #[arg(long, group = "view")]
    metrics: bool,
    /// Queries running and queued now, and sessions blocked by locks (SQL through the Data API)
    #[arg(long, group = "view")]
    queries: bool,
    /// The slowest queries and the longest queue waits of the last 24 hours (Data API)
    #[arg(long, group = "view")]
    slow: bool,
    /// The largest tables and the upkeep they need: VACUUM, ANALYZE, skew (Data API)
    #[arg(long, group = "view")]
    tables: bool,
    /// Events of the last 7 days
    #[arg(long, group = "view")]
    events: bool,
    /// The cluster's snapshots
    #[arg(long, group = "view")]
    snapshots: bool,
    /// Markdown performance diagnosis for an AI agent: configuration, nodes and skew, WLM
    /// queue, slowest queries, tables, events
    #[arg(long, group = "view")]
    report: bool,
    /// Cancel the query running in this session (its process id, from --queries)
    #[arg(long, value_name = "PID", group = "view")]
    cancel: Option<i64>,
    /// Pause the cluster: queries stop, the nodes are no longer billed (storage still is)
    #[arg(long, group = "view")]
    pause: bool,
    /// Resume a paused cluster
    #[arg(long, group = "view")]
    resume: bool,
    /// Resize to --node-type and/or --nodes; alone, lists the shapes elastic resize allows
    #[arg(long, group = "view")]
    resize: bool,
    /// Node type for --resize (default: the current one)
    #[arg(long, value_name = "TYPE")]
    node_type: Option<String>,
    /// Number of nodes for --resize (default: the current number)
    #[arg(long, value_name = "N")]
    nodes: Option<u32>,
    /// With --resize: a classic resize (hours, the cluster read-only meanwhile), for shapes
    /// elastic resize cannot reach
    #[arg(long)]
    classic: bool,
    /// Take a manual snapshot; without a name, one is made up from the cluster and the time
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "", group = "view")]
    snapshot: Option<String>,
    /// Window for --metrics and --report: 1h, 24h or 7d
    #[arg(long, default_value = "24h", value_name = "WINDOW")]
    since: String,
    /// Database the SQL runs in (default: the saved identity's, else the cluster's)
    #[arg(long)]
    database: Option<String>,
    /// Run the SQL as this database user, with temporary credentials
    #[arg(long, value_name = "USER", conflicts_with = "secret_arn")]
    db_user: Option<String>,
    /// Run the SQL with the user and password of this Secrets Manager secret
    #[arg(long, value_name = "ARN")]
    secret_arn: Option<String>,
    /// Keep --database / --db-user / --secret-arn as the cluster's identity (also used by the UI)
    #[arg(long)]
    save_identity: bool,
    /// Do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let (env, cluster) = env_and(config, args.env.clone(), args.cluster.clone())?;
    let aws = aws_for(config, &env)?;
    let range = Range::parse(&args.since).ok_or_else(|| anyhow!("--since: use 1h, 24h or 7d"))?;
    let any_view = args.metrics
        || args.queries
        || args.slow
        || args.tables
        || args.events
        || args.snapshots
        || args.report
        || args.cancel.is_some()
        || args.pause
        || args.resume
        || args.resize
        || args.snapshot.is_some();
    let Some(id) = cluster else {
        if any_view {
            bail!("which cluster? {} redshift [ENV] CLUSTER …", program());
        }
        return list(&aws, args.json);
    };
    let c = redshift::find_cluster(&aws, &id)?;
    let identity = identity(&env, &c, &args)?;
    if args.report {
        print!("{}", redshift::gather_report(&aws, &env, &aws.region, &c.id, identity.as_ref(), range, now_secs())?);
        return Ok(ExitCode::SUCCESS);
    }
    if args.metrics {
        return metrics(&aws, &c, range, args.json);
    }
    if args.queries || args.slow {
        return queries(&aws, &c, &need(identity, &c)?, args.slow, args.json);
    }
    if args.tables {
        return tables(&aws, &c, &need(identity, &c)?, args.json);
    }
    if args.events {
        return events(&aws, &c, args.json);
    }
    if args.snapshots {
        return snapshots(&aws, &c, args.json);
    }
    if let Some(pid) = args.cancel {
        let identity = need(identity, &c)?;
        let pid_ok = redshift::valid_pid(pid).ok_or_else(|| anyhow!("--cancel takes a process id (a positive number): {pid}"))?;
        if !confirm_write(&env, &format!("Cancel the query of session {pid_ok} on {} ({env})? Its transaction rolls back.", c.id), args.yes)? {
            bail!("not cancelled");
        }
        redshift::cancel_query(&aws, &c.id, &identity, pid)?;
        println!("{} cancelled the query of session {pid_ok}", paint("✓", "32"));
        return Ok(ExitCode::SUCCESS);
    }
    if args.pause || args.resume {
        return pause(&aws, &env, &c, args.resume, args.yes);
    }
    if args.resize {
        return resize(&aws, &env, &c, &args);
    }
    if let Some(name) = &args.snapshot {
        let name = if name.is_empty() { redshift::snapshot_name(&c.id, now_secs()) } else { name.clone() };
        if !redshift::valid_snapshot_name(&name) {
            bail!("snapshot names use lower-case letters, digits and single hyphens, and start with a letter: {name}");
        }
        if !confirm_write(&env, &format!("Take the manual snapshot {name} of {} ({env})? It is kept, and billed, until deleted.", c.id), args.yes)? {
            bail!("no snapshot taken");
        }
        redshift::create_snapshot(&aws, &c.id, &name)?;
        println!("{} taking the snapshot {name} (see {} redshift {env} {} --snapshots)", paint("✓", "32"), program(), c.id);
        return Ok(ExitCode::SUCCESS);
    }
    nodes(&aws, &env, &c, args.json)
}

/// The identity for SQL: the flags, over what is saved for the cluster. `--save-identity`
/// keeps the flags' one.
fn identity(env: &str, c: &Cluster, args: &Args) -> Result<Option<Identity>> {
    let saved = redshift_data::saved_identity(env, &c.id);
    let given = args.db_user.is_some() || args.secret_arn.is_some() || args.database.is_some();
    if !given {
        return Ok(saved);
    }
    let base = saved.unwrap_or_default();
    let (db_user, secret_arn) = match (&args.db_user, &args.secret_arn) {
        (Some(u), _) => (Some(u.clone()), None),
        (None, Some(a)) => (None, Some(a.clone())),
        (None, None) => (base.db_user.clone(), base.secret_arn.clone()),
    };
    if let Some(arn) = &secret_arn
        && !arn.starts_with("arn:")
    {
        bail!("--secret-arn takes the secret's ARN (arn:aws:secretsmanager:…)");
    }
    let database = args.database.clone().filter(|d| !d.trim().is_empty()).unwrap_or_else(|| if base.database.is_empty() { c.database() } else { base.database.clone() });
    let identity = Identity { database, db_user, secret_arn };
    if args.save_identity {
        if !identity.is_complete() {
            bail!("--save-identity needs --db-user or --secret-arn");
        }
        redshift_data::save_identity(env, &c.id, &identity)?;
        eprintln!("{}", paint(&format!("saved: {} runs SQL in {}", c.id, identity.describe()), "90"));
    }
    Ok(identity.is_complete().then_some(identity))
}

fn need(identity: Option<Identity>, c: &Cluster) -> Result<Identity> {
    if c.paused() {
        bail!("{} is paused: resume it first ({} redshift … {} --resume)", c.id, program(), c.id);
    }
    identity.ok_or_else(|| {
        anyhow!(
            "no database identity for {}: pass --db-user USER (temporary credentials) or --secret-arn ARN, with --database; \
             --save-identity keeps it",
            c.id
        )
    })
}

fn health_code(h: Health) -> &'static str {
    match h {
        Health::Ok => "32",
        Health::Changing => "94",
        Health::Paused => "90",
        Health::Strained => "33",
        Health::Failing => "31",
    }
}

fn list(aws: &Aws, json: bool) -> Result<ExitCode> {
    let clusters = redshift::describe_clusters(aws, None)?;
    if json {
        print_json(&serde_json::to_value(&clusters)?);
        return Ok(ExitCode::SUCCESS);
    }
    if clusters.is_empty() {
        println!("No provisioned Redshift clusters in {}.", aws.region);
        return Ok(ExitCode::SUCCESS);
    }
    let refs: Vec<&Cluster> = clusters.iter().collect();
    let pulse = redshift::pulse(aws, &refs, now_secs()).ok();
    let width = clusters.iter().map(|c| c.id.chars().count()).max().unwrap_or(0).min(40);
    for c in &clusters {
        let last = |id: &str| pulse.as_ref().and_then(|p| p.last(&c.id, id));
        let health = if c.status == "available" { redshift::use_health(last("cpu_all"), last("disk_all")) } else { redshift::status_health(&c.status) };
        let mut stats = Vec::new();
        if let Some(v) = last("cpu_all") {
            stats.push(format!("CPU {v:.0}%"));
        }
        if let Some(v) = last("disk_all") {
            stats.push(format!("disk {v:.0}%"));
        }
        if let Some(v) = last("connections") {
            stats.push(format!("{v:.0} conn"));
        }
        if let Some(v) = last("queued").filter(|q| *q >= 1.0) {
            stats.push(format!("{v:.0} queued"));
        }
        let mut flags = Vec::new();
        if c.publicly_accessible {
            flags.push("⚠ public");
        }
        if !c.encrypted {
            flags.push("⚠ unencrypted");
        }
        println!(
            "{} {:<width$}  {:<20} {}  {}  {}",
            paint("●", health_code(health)),
            ellipsize(&c.id, width),
            c.shape(),
            paint(&format!("{:<9}", c.status), health_code(redshift::status_health(&c.status))),
            paint(&stats.join(" · "), "90"),
            paint(&flags.join(" "), "33")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn nodes(aws: &Aws, env: &str, c: &Cluster, json: bool) -> Result<ExitCode> {
    let pulse = redshift::pulse(aws, &[c], now_secs()).ok();
    if json {
        let nodes: Vec<serde_json::Value> = c
            .nodes
            .iter()
            .map(|n| {
                let key = redshift::node_key(&c.id, n);
                let last = |id: &str| pulse.as_ref().and_then(|p| p.last(&key, id));
                json!({"node": n, "cpu": last("cpu"), "disk": if n.leader() { None } else { last("disk") }})
            })
            .collect();
        let skew = pulse.as_ref().and_then(|p| redshift::skew(&redshift::node_values(c, p, "cpu", true), redshift::CPU_SKEW_GAP));
        print_json(&json!({"cluster": c, "nodes": nodes, "cpu_skew": skew}));
        return Ok(ExitCode::SUCCESS);
    }
    let status = redshift::status_health(&c.status);
    println!(
        "{}  {}  {}",
        paint(&c.id, "1"),
        paint(&format!("{} · version {} · {env} · {}", c.shape(), c.version.clone().unwrap_or_default(), aws.region), "90"),
        paint(&format!("● {}", c.status), health_code(status))
    );
    if let Some(e) = &c.endpoint {
        println!("{}", paint(&format!("{e}:{} · database {}", c.port.unwrap_or(5439), c.database()), "90"));
    }
    if let Some(note) = redshift::status_note(c) {
        println!("{}", paint(&note, "94"));
    }
    if c.paused() {
        println!("\nPaused: {} redshift {env} {} --resume", program(), c.id);
        return Ok(ExitCode::SUCCESS);
    }
    println!("\n{}", paint("Nodes · last hour", "1"));
    let p = pulse.as_ref();
    for n in &c.nodes {
        let key = redshift::node_key(&c.id, n);
        let cpu = p.and_then(|p| p.last(&key, "cpu"));
        let disk = if n.leader() { None } else { p.and_then(|p| p.last(&key, "disk")) };
        let spark = p.map(|p| sparkline_to(&columns(p.points(&key, "cpu"), p.window, 12, Agg::Mean), 100.0)).unwrap_or_default();
        let mut stats = format!("CPU {spark} {}", cpu.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into()));
        if let Some(d) = disk {
            stats.push_str(&format!("  disk {d:.0}%"));
        }
        println!(
            "  {} {:<10}  {}  {}",
            paint(if n.leader() { "◆" } else { "●" }, health_code(redshift::use_health(cpu, disk))),
            n.label(),
            stats,
            paint(n.private_ip.as_deref().unwrap_or(""), "90")
        );
    }
    if let Some(p) = p {
        for (id, gap, what) in [("cpu", redshift::CPU_SKEW_GAP, "CPU"), ("disk", redshift::DISK_SKEW_GAP, "disk")] {
            if let Some(s) = redshift::skew(&redshift::node_values(c, p, id, true), gap) {
                println!(
                    "\n{}",
                    paint(&format!("⚠ {what} skew: {} at {:.0}% while the other compute nodes average {:.0}% (look at the distribution keys)", s.node, s.value, s.others), "33")
                );
            }
        }
        let last = |id: &str| p.last(&c.id, id);
        let mut parts = Vec::new();
        if let Some(v) = last("connections") {
            parts.push(format!("{v:.0} connections"));
        }
        if let Some(v) = last("queued") {
            parts.push(format!("{v:.0} queued in WLM"));
        }
        if !parts.is_empty() {
            println!("\n{}", paint(&format!("cluster · {}", parts.join(" · ")), "90"));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A line per metric and node: a sparkline over the window and its numbers.
fn metrics(aws: &Aws, c: &Cluster, range: Range, json: bool) -> Result<ExitCode> {
    if c.paused() {
        bail!("{} is paused: it publishes no metrics", c.id);
    }
    let m = redshift::cluster_metrics(aws, c, range, now_secs())?;
    let sources: Vec<(String, String)> =
        c.nodes.iter().map(|n| (redshift::node_key(&c.id, n), n.label())).chain([(c.id.clone(), "cluster".to_string())]).collect();
    let defs = || redshift::NODE_METRICS.iter().chain(redshift::CLUSTER_METRICS.iter());
    if json {
        let mut out = serde_json::Map::new();
        for (key, label) in &sources {
            let series: serde_json::Map<String, serde_json::Value> = defs().filter(|d| m.has(key, d.id)).map(|d| (d.id.to_string(), json!(m.points(key, d.id)))).collect();
            out.insert(label.clone(), serde_json::Value::Object(series));
        }
        print_json(&json!({"cluster": c.id, "window": [m.window.0, m.window.1], "series": out}));
        return Ok(ExitCode::SUCCESS);
    }
    const WIDTH: usize = 40;
    println!("{} · last {}", paint(&c.id, "1"), range.label());
    for (key, label) in &sources {
        println!("\n  {}", paint(label, "1"));
        for def in defs().filter(|d| m.has(key, d.id)) {
            let agg = match def.stat {
                "Sum" => Agg::Sum,
                "Maximum" => Agg::Max,
                _ => Agg::Mean,
            };
            let spark = sparkline(&columns(m.points(key, def.id), m.window, WIDTH, agg));
            let numbers = if def.stat == "Sum" {
                format!("{} in the window", def.unit.fmt(m.sum(key, def.id)))
            } else {
                format!("avg {} · max {}", def.unit.fmt(m.avg(key, def.id).unwrap_or_default()), def.unit.fmt(m.max(key, def.id).unwrap_or_default()))
            };
            println!("    {:<30} {}  {}", def.label, paint(&spark, "36"), paint(&numbers, "90"));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn query_line(q: &Query, live: bool, now: f64) {
    let took = if live { q.started.map(|s| fmt_ms((now - s).max(0.0) * 1000.0)).unwrap_or_else(|| fmt_ms(q.elapsed_us / 1000.0)) } else { fmt_ms(q.elapsed_us / 1000.0) };
    println!(
        "  {:<7} {:<14} {:<9} {:>9} {:>9}  {}",
        q.pid.map(|p| p.to_string()).unwrap_or_else(|| "—".into()),
        ellipsize(&q.user, 14),
        q.status,
        took,
        paint(&format!("{:>9}", fmt_ms(q.queue_us / 1000.0)), if q.queue_us >= 1e6 { "33" } else { "90" }),
        ellipsize(&q.text.split_whitespace().collect::<Vec<_>>().join(" "), 100)
    );
}

fn queries(aws: &Aws, c: &Cluster, identity: &Identity, slow: bool, json: bool) -> Result<ExitCode> {
    let a = redshift::activity(aws, &c.id, identity);
    if json {
        let part = |r: &crate::aws::Result<Vec<Query>>| match r {
            Ok(v) => json!(v),
            Err(e) => json!({"error": e.short()}),
        };
        let locks = match &a.locks {
            Ok(v) => json!(v),
            Err(e) => json!({"error": e.short()}),
        };
        let out = if slow {
            json!({"cluster": c.id, "slowest": part(&a.slowest), "longest_waits": part(&a.waited)})
        } else {
            json!({"cluster": c.id, "now": part(&a.running), "locks": locks})
        };
        print_json(&out);
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", paint(&format!("{} · as {} · users who are not superusers see only their own queries", c.id, identity.describe()), "90"));
    let now = now_secs();
    let header = |live: bool| println!("{}", paint(&format!("  {:<7} {:<14} {:<9} {:>9} {:>9}  SQL", "pid", "user", "status", if live { "for" } else { "took" }, "queued"), "90"));
    let section = |title: &str, list: &crate::aws::Result<Vec<Query>>, live: bool| {
        println!("\n{}", paint(title, "1"));
        match list {
            Ok(list) if list.is_empty() => println!("  none"),
            Ok(list) => {
                header(live);
                for q in list {
                    query_line(q, live, now);
                }
            }
            Err(e) => println!("  {}", paint(&e.short(), "31")),
        }
    };
    if slow {
        section("Slowest, last 24 hours", &a.slowest, false);
        section("Longest waits in a queue, last 24 hours", &a.waited, false);
        return Ok(ExitCode::SUCCESS);
    }
    section("Running and queued now", &a.running, true);
    println!("\n{}", paint("Blocked by locks", "1"));
    match &a.locks {
        Ok(locks) if locks.is_empty() => println!("  none"),
        Ok(locks) => {
            for l in locks {
                let pid = |p: Option<i64>| p.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
                println!(
                    "  {} session {} ({}) has waited {} for {} on {}, held by session {} ({}) for {}",
                    paint("⚠", "33"),
                    pid(l.waiting_pid),
                    l.waiting_user,
                    fmt_ms(l.waiting_s * 1000.0),
                    l.wanted,
                    l.table,
                    pid(l.blocking_pid),
                    l.blocking_user,
                    fmt_ms(l.held_s * 1000.0)
                );
            }
        }
        Err(e) => println!("  {}", paint(&e.short(), "31")),
    }
    Ok(ExitCode::SUCCESS)
}

fn tables(aws: &Aws, c: &Cluster, identity: &Identity, json: bool) -> Result<ExitCode> {
    let list = redshift::tables(aws, &c.id, identity)?;
    if json {
        let rows: Vec<serde_json::Value> = list.iter().map(|t| json!({"table": t, "flags": t.flags()})).collect();
        print_json(&json!({"cluster": c.id, "tables": rows}));
        return Ok(ExitCode::SUCCESS);
    }
    let name_w = list.iter().map(|t| t.name().chars().count()).max().unwrap_or(10).clamp(10, 48);
    println!(
        "{}",
        paint(&format!("{:<name_w$} {:>9} {:>9} {:>9} {:>9} {:>6}  {:<14} flags", "table", "size", "rows", "unsorted", "stats off", "skew", "dist style"), "90")
    );
    let pct = |v: Option<f64>| v.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into());
    for t in &list {
        println!(
            "{:<name_w$} {:>9} {:>9} {:>9} {:>9} {:>6}  {:<14} {}",
            ellipsize(&t.name(), name_w),
            t.size(),
            redshift::fmt_rows(t.rows),
            pct(t.unsorted),
            pct(t.stats_off),
            t.skew_rows.map(|s| format!("{s:.1}")).unwrap_or_else(|| "—".into()),
            ellipsize(&t.diststyle, 14),
            paint(&t.flags().join(", "), "33")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn events(aws: &Aws, c: &Cluster, json: bool) -> Result<ExitCode> {
    let events: Vec<redshift::Event> = redshift::describe_events(aws, 24 * 7)?.into_iter().filter(|e| e.source == c.id).collect();
    if json {
        print_json(&json!({"events": events}));
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", paint("Events · last 7 days", "1"));
    if events.is_empty() {
        println!("  none");
    }
    for e in &events {
        println!(
            "  {} {:<9} {}  {}  {}",
            paint("●", health_code(e.severity())),
            fmt_ago(e.date),
            paint(&fmt_utc(e.date), "90"),
            paint(&e.categories.join(", "), "90"),
            e.message
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn snapshots(aws: &Aws, c: &Cluster, json: bool) -> Result<ExitCode> {
    let list = redshift::describe_snapshots(aws, Some(&c.id))?;
    if json {
        print_json(&json!({"snapshots": list}));
        return Ok(ExitCode::SUCCESS);
    }
    if list.is_empty() {
        println!("No snapshots of {}.", c.id);
    }
    for s in &list {
        let size = s.size_mb.map(|mb| crate::aws::rds::fmt_size(mb * 1024.0 * 1024.0)).unwrap_or_else(|| "?".into());
        println!(
            "{:<9} {:<50} {:>10}  {}  {}",
            s.kind,
            s.id,
            size,
            paint(&s.created.map(fmt_ago).unwrap_or_default(), "90"),
            if s.status == "available" { String::new() } else { paint(&s.status, "94") }
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn pause(aws: &Aws, env: &str, c: &Cluster, resume: bool, yes: bool) -> Result<ExitCode> {
    let (want, question) = if resume {
        ("paused", format!("Resume {} ({}, {env})? It takes a few minutes.", c.id, c.shape()))
    } else {
        ("available", format!("Pause {} ({}, {env})? Running queries stop and connections close; storage is still billed.", c.id, c.shape()))
    };
    if c.status != want {
        bail!("{} is {}: it can be {} when {want}", c.id, c.status, if resume { "resumed" } else { "paused" });
    }
    if !confirm_write(env, &question, yes)? {
        bail!("not changed");
    }
    if resume {
        redshift::resume(aws, &c.id)?;
    } else {
        redshift::pause(aws, &c.id)?;
    }
    println!("{} {} is {}", paint("✓", "32"), c.id, if resume { "resuming" } else { "pausing" });
    Ok(ExitCode::SUCCESS)
}

fn resize(aws: &Aws, env: &str, c: &Cluster, args: &Args) -> Result<ExitCode> {
    let options = redshift::resize_options(aws, &c.id)?;
    if args.node_type.is_none() && args.nodes.is_none() {
        if args.json {
            print_json(&json!({"cluster": c.id, "current": c.shape(), "elastic_resize": options}));
            return Ok(ExitCode::SUCCESS);
        }
        println!("{} · {} · elastic resize can go to:", paint(&c.id, "1"), c.shape());
        for o in &options {
            let current = if o.node_type == c.node_type && o.nodes == c.nodes_count { "  (now)" } else { "" };
            println!("  {:<24} {}{current}", o.shape(), paint(&o.disk_percent.map(|d| format!("disk ≈ {d:.0}%")).unwrap_or_default(), "90"));
        }
        println!("{}", paint(&format!("{} redshift {env} {} --resize --nodes N [--node-type TYPE]", program(), c.id), "90"));
        return Ok(ExitCode::SUCCESS);
    }
    if c.status != "available" {
        bail!("{} is {}: a resize starts from an available cluster", c.id, c.status);
    }
    let node_type = args.node_type.clone().unwrap_or_else(|| c.node_type.clone());
    let nodes = args.nodes.unwrap_or(c.nodes_count);
    if node_type == c.node_type && nodes == c.nodes_count {
        bail!("{} already is {}", c.id, c.shape());
    }
    let elastic = options.iter().find(|o| o.node_type == node_type && o.nodes == nodes);
    if elastic.is_none() && !args.classic {
        let shapes: Vec<String> = options.iter().map(|o| o.shape()).collect();
        bail!(
            "elastic resize cannot take {} to {node_type} × {nodes} (it can: {}). --classic runs a classic resize: hours, the cluster read-only meanwhile",
            c.id,
            if shapes.is_empty() { "nothing".into() } else { shapes.join(", ") }
        );
    }
    let how = if args.classic && elastic.is_none() {
        "Classic resize: it takes hours and the cluster is read-only meanwhile."
    } else {
        "Elastic resize: minutes, during which the cluster is unavailable for a short while; connections drop."
    };
    let disk = elastic.and_then(|o| o.disk_percent).map(|d| format!(" Estimated disk use after: {d:.0}%.")).unwrap_or_default();
    let question = format!("Resize {} ({env}) from {} to {node_type} × {nodes}? {how}{disk}", c.id, c.shape());
    if !confirm_write(env, &question, args.yes)? {
        bail!("not resized");
    }
    redshift::resize(aws, &c.id, &node_type, nodes, args.classic && elastic.is_none())?;
    println!("{} {} is resizing to {node_type} × {nodes}", paint("✓", "32"), c.id);
    Ok(ExitCode::SUCCESS)
}

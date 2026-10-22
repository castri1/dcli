//! `dcli aurora`: Aurora clusters, a cluster's topology (endpoints → nodes, each node's
//! CPU, connections and replica lag), its metrics, top SQL and wait events (Performance
//! Insights), events and pending maintenance, and a performance diagnosis for an AI agent.

use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use crate::aws::Aws;
use crate::aws::lambda::Range;
use crate::aws::pi;
use crate::aws::rds::{self, Cluster, Health, Instance, Inventory, Metrics};
use crate::cli::{aws_for, env_and, paint, print_json};
use crate::config::Config;
use crate::tui::chart::{Agg, columns, fmt_ms, sparkline, sparkline_to};
use crate::util::{ellipsize, fmt_ago, fmt_utc, now_secs, program};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Cluster identifier; without it, lists the clusters
    cluster: Option<String>,
    /// Every node's metrics over --range
    #[arg(long, group = "view")]
    metrics: bool,
    /// Top SQL and wait events (Performance Insights) of the writer, or of --instance
    #[arg(long, group = "view")]
    top_sql: bool,
    /// Events of the last 7 days and pending maintenance
    #[arg(long, group = "view")]
    events: bool,
    /// Markdown performance diagnosis for an AI agent: configuration, topology, metrics, top
    /// SQL, events
    #[arg(long, group = "view")]
    report: bool,
    /// The instance for --top-sql (default: the writer)
    #[arg(long, value_name = "INSTANCE")]
    instance: Option<String>,
    /// Window for --metrics, --top-sql and --report: 1h, 24h or 7d
    #[arg(long, default_value = "24h", value_name = "WINDOW")]
    range: String,
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let (env, cluster) = env_and(config, args.env.clone(), args.cluster.clone())?;
    let aws = aws_for(config, &env)?;
    let range = Range::parse(&args.range).ok_or_else(|| anyhow!("--range: use 1h, 24h or 7d"))?;
    let Some(id) = cluster else {
        if args.metrics || args.top_sql || args.events || args.report {
            bail!("which cluster? {} aurora [ENV] CLUSTER …", program());
        }
        return list(&aws, args.json);
    };
    if args.report {
        print!("{}", rds::gather_report(&aws, &env, &aws.region, &id, range, now_secs())?);
        return Ok(ExitCode::SUCCESS);
    }
    let inventory = rds::inventory(&aws, Some(&id))?;
    let Some(cluster) = inventory.cluster(&id) else { bail!("Aurora cluster not found: {id}") };
    let instances = inventory.instances_of(&id);
    if args.metrics {
        return metrics(&aws, cluster, &instances, range, args.json);
    }
    if args.top_sql {
        return top_sql(&aws, &instances, args.instance.as_deref(), range, args.json);
    }
    if args.events {
        return events(&aws, cluster, &instances, args.json);
    }
    topology(&aws, &env, &inventory, cluster, args.json)
}

fn health_code(h: Health) -> &'static str {
    match h {
        Health::Ok => "32",
        Health::Changing => "94",
        Health::Stopped => "90",
        Health::Strained => "33",
        Health::Failing => "31",
    }
}

/// The worst health of a cluster's nodes, from the last hour.
fn cluster_health(cluster: &Cluster, instances: &[&Instance], pulse: Option<&Metrics>) -> Health {
    let worst = instances
        .iter()
        .map(|i| rds::node_health(&i.status, pulse.and_then(|p| p.last(&i.id, "cpu")), pulse.and_then(|p| p.max(&i.id, "lag"))))
        .max()
        .unwrap_or(Health::Ok);
    worst.max(rds::status_health(&cluster.status))
}

fn list(aws: &Aws, json: bool) -> Result<ExitCode> {
    let inventory = rds::inventory(aws, None)?;
    if json {
        print_json(&serde_json::to_value(&inventory)?);
        return Ok(ExitCode::SUCCESS);
    }
    if inventory.clusters.is_empty() {
        println!("No Aurora clusters in {}.", aws.region);
        return Ok(ExitCode::SUCCESS);
    }
    let all: Vec<&Instance> = inventory.instances.iter().collect();
    let pulse = rds::pulse(aws, &all, &[], now_secs()).ok();
    let width = inventory.clusters.iter().map(|c| c.id.chars().count()).max().unwrap_or(0).min(40);
    for c in &inventory.clusters {
        let instances = inventory.instances_of(&c.id);
        let health = cluster_health(c, &instances, pulse.as_ref());
        let readers = instances.iter().filter(|i| !i.writer).count();
        let cpu = c.writer().and_then(|w| pulse.as_ref()?.last(w, "cpu")).map(|v| format!("CPU {v:.0}%")).unwrap_or_default();
        let global = c.global.as_ref().map(|(g, primary)| format!(" · global {g} ({})", if *primary { "primary" } else { "secondary" })).unwrap_or_default();
        println!(
            "{} {:<width$}  {}  {}",
            paint("●", health_code(health)),
            ellipsize(&c.id, width),
            c.engine_label(),
            paint(
                &format!("{} · {} · writer {} · {readers} reader{}  {cpu}{global}", c.status, c.capacity_label(), c.writer().unwrap_or("none"), if readers == 1 { "" } else { "s" }),
                "90"
            )
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn topology(aws: &Aws, env: &str, inventory: &Inventory, c: &Cluster, json: bool) -> Result<ExitCode> {
    let instances = inventory.instances_of(&c.id);
    let endpoints = inventory.endpoints_of(&c.id);
    let pulse = rds::pulse(aws, &instances, &[], now_secs()).ok();
    if json {
        let endpoints: Vec<serde_json::Value> = endpoints
            .iter()
            .map(|e| json!({"name": e.name, "kind": e.kind_label(), "address": e.address, "status": e.status, "routes": rds::routes(e, &instances)}))
            .collect();
        let nodes: Vec<serde_json::Value> = instances
            .iter()
            .map(|i| {
                let last = |id: &str| pulse.as_ref().and_then(|p| p.last(&i.id, id));
                json!({"instance": i, "cpu": last("cpu"), "connections": last("connections"), "replica_lag_ms": last("lag"), "capacity_acu": last("acu")})
            })
            .collect();
        print_json(&json!({"cluster": c, "endpoints": endpoints, "nodes": nodes}));
        return Ok(ExitCode::SUCCESS);
    }
    let health = cluster_health(c, &instances, pulse.as_ref());
    println!(
        "{}  {}  {}",
        paint(&c.id, "1"),
        paint(&format!("{} · {} · {env} · {}", c.engine_label(), c.capacity_label(), aws.region), "90"),
        paint(&format!("● {}", c.status), health_code(health))
    );
    if let Some((global, primary)) = &c.global {
        println!("{}", paint(&format!("global database {global} ({})", if *primary { "primary cluster" } else { "secondary cluster" }), "90"));
    }
    println!("\n{}", paint("Endpoints", "1"));
    let name_w = endpoints.iter().map(|e| e.name.chars().count()).max().unwrap_or(6);
    for e in &endpoints {
        let port = c.port.map(|p| format!(":{p}")).unwrap_or_default();
        println!(
            "  {:<name_w$}  {:<16}  {}{port}  {} {}",
            e.name,
            paint(e.kind_label(), "90"),
            e.address,
            paint("→", "36"),
            rds::routes(e, &instances).join(", ")
        );
    }
    println!("\n{}", paint("Nodes · last hour", "1"));
    let id_w = instances.iter().map(|i| i.id.chars().count()).max().unwrap_or(8);
    for (zone, nodes) in rds::by_zone(&instances) {
        println!("  {}", paint(&zone, "36"));
        for i in nodes {
            let p = pulse.as_ref();
            let cpu = p.and_then(|p| p.last(&i.id, "cpu"));
            let lag = p.and_then(|p| p.max(&i.id, "lag"));
            let h = rds::node_health(&i.status, cpu, lag);
            let spark = p.map(|p| sparkline_to(&columns(p.points(&i.id, "cpu"), p.window, 12, Agg::Mean), 100.0)).unwrap_or_default();
            let mut stats = format!("CPU {spark} {}", cpu.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into()));
            if let Some(n) = p.and_then(|p| p.last(&i.id, "connections")) {
                stats.push_str(&format!("  {n:.0} conn"));
            }
            if let Some(acu) = p.and_then(|p| p.last(&i.id, "acu")) {
                stats.push_str(&format!("  {acu:.1} ACU"));
            }
            if !i.writer {
                stats.push_str(&format!("  lag {}", lag.map(fmt_ms).unwrap_or_else(|| "—".into())));
            }
            println!(
                "    {} {:<id_w$}  {:<16}  {:<15}  {}  {}",
                paint(if i.writer { "✎" } else { "●" }, health_code(h)),
                i.id,
                i.role(),
                i.class,
                paint(&i.status, health_code(rds::status_health(&i.status))),
                stats
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A line per metric and node: a sparkline over the window and its numbers.
fn metrics(aws: &Aws, c: &Cluster, instances: &[&Instance], range: Range, json: bool) -> Result<ExitCode> {
    let m = rds::cluster_metrics(aws, &c.id, instances, range, now_secs())?;
    if json {
        let mut out = serde_json::Map::new();
        for source in instances.iter().map(|i| i.id.as_str()).chain([c.id.as_str()]) {
            let series: serde_json::Map<String, serde_json::Value> = rds::INSTANCE_METRICS
                .iter()
                .chain(rds::CLUSTER_METRICS.iter())
                .filter(|d| m.has(source, d.id))
                .map(|d| (d.id.to_string(), json!(m.points(source, d.id))))
                .collect();
            out.insert(source.to_string(), serde_json::Value::Object(series));
        }
        print_json(&json!({"cluster": c.id, "window": [m.window.0, m.window.1], "series": out}));
        return Ok(ExitCode::SUCCESS);
    }
    const WIDTH: usize = 40;
    println!("{} · last {}", paint(&c.id, "1"), range.label());
    let line = |source: &str, def: &rds::MetricDef| {
        if !m.has(source, def.id) {
            return;
        }
        let agg = if def.stat == "Sum" { Agg::Sum } else if def.stat == "Maximum" { Agg::Max } else { Agg::Mean };
        let spark = sparkline(&columns(m.points(source, def.id), m.window, WIDTH, agg));
        let numbers = if def.stat == "Sum" {
            format!("{} in the window", def.unit.fmt(m.sum(source, def.id)))
        } else if def.stat == "Minimum" {
            format!("lowest {} · latest {}", def.unit.fmt(m.min(source, def.id).unwrap_or_default()), def.unit.fmt(m.last(source, def.id).unwrap_or_default()))
        } else {
            format!("avg {} · max {}", def.unit.fmt(m.avg(source, def.id).unwrap_or_default()), def.unit.fmt(m.max(source, def.id).unwrap_or_default()))
        };
        println!("    {:<24} {}  {}", def.label, paint(&spark, "36"), paint(&numbers, "90"));
    };
    for i in instances {
        println!("\n  {} {}", paint(&i.id, "1"), paint(&format!("{} · {}", i.role(), i.class), "90"));
        for def in &rds::INSTANCE_METRICS {
            line(&i.id, def);
        }
    }
    println!("\n  {} {}", paint(&c.id, "1"), paint("cluster volume", "90"));
    for def in &rds::CLUSTER_METRICS {
        line(&c.id, def);
    }
    Ok(ExitCode::SUCCESS)
}

fn top_sql(aws: &Aws, instances: &[&Instance], instance: Option<&str>, range: Range, json: bool) -> Result<ExitCode> {
    let i = match instance {
        Some(id) => instances.iter().find(|i| i.id == id).ok_or_else(|| anyhow!("no instance {id} in this cluster"))?,
        None => instances.iter().find(|i| i.writer).or(instances.first()).ok_or_else(|| anyhow!("the cluster has no instances"))?,
    };
    if !i.pi_enabled {
        bail!("Performance Insights is off for {}. To turn it on: {}", i.id, pi::how_to_enable(&i.id, &aws.region));
    }
    let now = now_secs();
    let start = now - range.secs();
    let sql = pi::top_sql(aws, &i.resource_id, start, now, 10)?;
    let waits = pi::top_waits(aws, &i.resource_id, start, now, 10)?;
    if json {
        print_json(&json!({"instance": i.id, "window": [start, now], "sql": sql, "waits": waits}));
        return Ok(ExitCode::SUCCESS);
    }
    let vcpus = rds::class_specs(&i.class).map(|(v, _)| format!(" · {v} vCPUs")).unwrap_or_default();
    println!("{} · {} · last {}{vcpus}", paint(&i.id, "1"), i.role(), range.label());
    println!("\n  {}", paint("Wait events (average active sessions)", "1"));
    let total: f64 = waits.iter().map(|w| w.load).sum();
    for w in &waits {
        let bar = "█".repeat(((w.load / total.max(1e-9)) * 20.0).round() as usize);
        println!("    {:>6.2}  {:<20} {:<24} {}", w.load, paint(&bar, "36"), w.label, paint(w.kind.as_deref().unwrap_or(""), "90"));
    }
    println!("\n  {}", paint("Top SQL (average active sessions)", "1"));
    for (n, s) in sql.iter().enumerate() {
        let split: Vec<String> = s.partitions.iter().filter(|(_, v)| *v > 0.0).map(|(k, v)| format!("{k} {:.0}%", v / s.load.max(1e-9) * 100.0)).collect();
        println!("    {:>2}. {:>6.2}  {}", n + 1, s.load, paint(&split.join(" · "), "90"));
        for line in s.label.lines() {
            println!("        {line}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn events(aws: &Aws, c: &Cluster, instances: &[&Instance], json: bool) -> Result<ExitCode> {
    let mine = |source: &str| source == c.id || instances.iter().any(|i| i.id == source);
    let events: Vec<rds::Event> = rds::describe_events(aws, 24 * 7)?.into_iter().filter(|e| mine(&e.source)).collect();
    let pending: Vec<rds::Maintenance> = rds::pending_maintenance(aws).unwrap_or_default().into_iter().filter(|m| mine(m.target())).collect();
    if json {
        print_json(&json!({"events": events, "pending_maintenance": pending}));
        return Ok(ExitCode::SUCCESS);
    }
    if !pending.is_empty() {
        println!("{}", paint("Pending maintenance", "1"));
        for m in &pending {
            let when = m.auto_applied_after.or(m.forced_apply).or(m.current_apply).map(|t| format!(" · from {}", fmt_utc(t))).unwrap_or_default();
            println!("  {}  {}{}", m.target(), paint(&m.action, "33"), paint(&format!("{}{when}", m.description.as_deref().map(|d| format!(" · {d}")).unwrap_or_default()), "90"));
        }
        println!();
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
            e.source,
            e.message
        );
    }
    Ok(ExitCode::SUCCESS)
}

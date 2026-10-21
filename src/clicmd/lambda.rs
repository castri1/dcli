//! `dcli lambda`: a function's configuration, its metrics, a report for an AI agent, and
//! invoking it.

use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};

use crate::aws::lambda::{self, Range};
use crate::aws::Aws;
use crate::cli::{aws_for, confirm_write, env_and, paint, print_json, read_value};
use crate::config::Config;
use crate::tui::chart::{Agg, columns, fmt_count, fmt_ms, sparkline};
use crate::util::{now_secs, pretty_json, program};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Function name
    name: Option<String>,
    #[arg(long)]
    json: bool,
    /// Markdown report for an AI agent: configuration, last 24 h of metrics, problems in its logs
    #[arg(long, conflicts_with_all = ["metrics", "invoke"])]
    report: bool,
    /// Invocations, errors, duration, concurrency and throttles over --since
    #[arg(long, conflicts_with = "invoke")]
    metrics: bool,
    /// Window for --metrics: 1h, 24h or 7d
    #[arg(long, default_value = "24h", value_name = "WINDOW")]
    since: String,
    /// Invoke the function with this JSON payload ("-" reads stdin, with --yes); exit code 1 when it fails
    #[arg(long, value_name = "PAYLOAD", allow_hyphen_values = true)]
    invoke: Option<String>,
    /// Do not ask for confirmation before invoking
    #[arg(short, long)]
    yes: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let (env, Some(name)) = env_and(config, args.env, args.name)? else { bail!("which function? {} lambda [ENV] NAME", program()) };
    let aws = aws_for(config, &env)?;
    if args.report {
        print!("{}", lambda::gather_report(&aws, &env, &aws.region, &name, now_secs())?);
        return Ok(ExitCode::SUCCESS);
    }
    if args.metrics {
        let range = Range::parse(&args.since).ok_or_else(|| anyhow!("--since: use 1h, 24h or 7d"))?;
        return metrics(&aws, &name, range, args.json);
    }
    if let Some(payload) = args.invoke {
        return invoke(&aws, &env, &name, &payload, args.yes, args.json);
    }
    show(&aws, &name, args.json)
}

fn show(aws: &Aws, name: &str, json: bool) -> Result<ExitCode> {
    let f = lambda::get_function(aws, name)?;
    if json {
        print_json(&serde_json::to_value(&f)?);
        return Ok(ExitCode::SUCCESS);
    }
    let c = &f.configuration;
    let row = |k: &str, v: String| println!("  {:<14} {v}", paint(k, "90"));
    println!("{}", paint(&c.function_name, "1"));
    row("ARN", c.function_arn.clone());
    row("Runtime", c.runtime.clone().unwrap_or_else(|| "—".into()));
    row("Handler", c.handler.clone().unwrap_or_default());
    row("Memory", c.memory_size.map(|m| format!("{m} MB")).unwrap_or_default());
    row("Timeout", c.timeout.map(|t| format!("{t} s")).unwrap_or_default());
    row("Architecture", c.architectures.join(", "));
    row("State", c.state.clone().unwrap_or_default());
    row("Modified", c.last_modified.clone().unwrap_or_default());
    row("Role", c.role.clone().unwrap_or_default());
    if let Some(env) = &c.environment {
        row("Env vars", env.variables.keys().cloned().collect::<Vec<_>>().join(", "));
    }
    Ok(ExitCode::SUCCESS)
}

/// A line per metric: its numbers and a sparkline over the window.
fn metrics(aws: &Aws, name: &str, range: Range, json: bool) -> Result<ExitCode> {
    let m = lambda::metrics(aws, name, range, now_secs())?;
    if json {
        let series: serde_json::Map<String, serde_json::Value> =
            m.series.iter().map(|s| (s.id.clone(), serde_json::json!(s.points))).collect();
        print_json(&serde_json::json!({ "function": name, "window": [m.window.0, m.window.1], "series": series }));
        return Ok(ExitCode::SUCCESS);
    }
    const WIDTH: usize = 48;
    let line = |label: &str, id: &str, agg: Agg, numbers: String, color: &str| {
        let spark = sparkline(&columns(m.points(id), m.window, WIDTH, agg));
        println!("  {:<13} {}  {}", paint(label, "1"), paint(&spark, color), paint(&numbers, "90"));
    };
    println!("{} · last {}", paint(name, "1"), range.label());
    let errors = m.total("errors");
    let rate = m.error_rate().map(|r| format!(" ({:.2}%)", r * 100.0)).unwrap_or_default();
    line("Invocations", "invocations", Agg::Sum, format!("{} total", fmt_count(m.total("invocations"))), "36");
    line("Errors", "errors", Agg::Sum, format!("{}{rate}", fmt_count(errors)), if errors > 0.0 { "31" } else { "32" });
    let avg = m.mean_duration().map(fmt_ms).unwrap_or_else(|| "—".into());
    let worst = m.max("p95").map(fmt_ms).unwrap_or_else(|| "—".into());
    line("Duration p95", "p95", Agg::Max, format!("avg {avg} · worst p95 {worst}"), "94");
    line("Concurrency", "concurrency", Agg::Max, format!("max {}", fmt_count(m.max("concurrency").unwrap_or_default())), "32");
    line("Throttles", "throttles", Agg::Sum, format!("{} total", fmt_count(m.total("throttles"))), "33");
    Ok(ExitCode::SUCCESS)
}

fn invoke(aws: &Aws, env: &str, name: &str, payload: &str, yes: bool, json: bool) -> Result<ExitCode> {
    let payload = match read_value(payload)?.trim() {
        "" => "{}".to_string(),
        text => text.to_string(),
    };
    serde_json::from_str::<serde_json::Value>(&payload).map_err(|e| anyhow!("the payload is not valid JSON: {e}"))?;
    if !confirm_write(env, &format!("Invoke {name} in {env}? It runs for real."), yes)? {
        bail!("not invoked");
    }
    let inv = lambda::invoke(aws, name, &payload)?;
    if json {
        print_json(&serde_json::json!({
            "statusCode": inv.status,
            "functionError": inv.function_error,
            "executedVersion": inv.version,
            "payload": serde_json::from_str::<serde_json::Value>(&inv.payload).unwrap_or(serde_json::Value::String(inv.payload.clone())),
            "log": inv.log_tail,
        }));
    } else {
        let outcome = match &inv.function_error {
            Some(kind) => paint(&format!("✗ function error ({kind})"), "31"),
            None => paint("✓ succeeded", "32"),
        };
        println!("{outcome} · status {} · version {}", inv.status, inv.version.as_deref().unwrap_or("?"));
        println!("{}", pretty_json(&inv.payload).unwrap_or_else(|| inv.payload.clone()));
        if let Some(log) = &inv.log_tail {
            println!("{}", paint("--- log (the last 4 KB) ---", "90"));
            print!("{log}");
            if !log.ends_with('\n') {
                println!();
            }
        }
    }
    Ok(if inv.function_error.is_some() { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

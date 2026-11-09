//! `dcli lambda`: a function's configuration, its metrics, a report for an AI agent,
//! invoking it, and its saved test events.

use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};

use crate::aws::lambda::{self, Range};
use crate::aws::{self, Aws};
use crate::testevents::{self, Event};
use crate::cli::{aws_for, confirm_write, env_and, paint, print_json, read_value};
use crate::config::Config;
use crate::tui::chart::{Agg, columns, fmt_count, fmt_ms, sparkline};
use crate::util::{ellipsize, now_secs, pretty_json, program};

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
    /// The saved test events: this machine's and the ones shared in AWS (the Lambda console's)
    #[arg(long, conflicts_with_all = ["report", "metrics", "invoke"])]
    events: bool,
    /// Invoke with a saved test event (this machine's first; --shared takes the one in AWS)
    #[arg(long, value_name = "NAME", conflicts_with_all = ["report", "metrics", "invoke", "events"])]
    event: Option<String>,
    /// Save --payload as a test event, on this machine or, with --shared, in AWS
    #[arg(long, value_name = "NAME", requires = "payload", conflicts_with_all = ["report", "metrics", "invoke", "events", "event"])]
    save_event: Option<String>,
    /// The JSON payload for --save-event ("-" reads stdin)
    #[arg(long, value_name = "PAYLOAD", allow_hyphen_values = true, requires = "save_event")]
    payload: Option<String>,
    /// Delete a saved test event, from this machine or, with --shared, from AWS
    #[arg(long, value_name = "NAME", conflicts_with_all = ["report", "metrics", "invoke", "events", "event", "save_event"])]
    delete_event: Option<String>,
    /// With --event, --save-event and --delete-event: the events shared in AWS, which the Lambda console lists too
    #[arg(long)]
    shared: bool,
    /// Do not ask for confirmation before invoking or changing a shared event
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
    if args.events {
        return events(&aws, &name, args.json);
    }
    if let Some(event) = args.event {
        let payload = saved_event(&aws, &name, &event, args.shared)?.compact();
        return invoke(&aws, &env, &name, &payload, args.yes, args.json);
    }
    if let (Some(event), Some(payload)) = (&args.save_event, &args.payload) {
        return save_event(&aws, &env, &name, event, payload, args.shared, args.yes);
    }
    if let Some(event) = args.delete_event {
        return delete_event(&aws, &env, &name, &event, args.shared, args.yes);
    }
    if args.shared {
        bail!("--shared goes with --event, --save-event or --delete-event");
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

/// The saved events: this machine's, then the shared ones. Shared events that cannot be read
/// (no permission for EventBridge Schemas) are told apart from having none.
fn events(aws: &Aws, name: &str, json: bool) -> Result<ExitCode> {
    let local = testevents::local(name);
    let shared = testevents::shared(aws, name);
    if json {
        let list = |events: &[Event]| -> Vec<serde_json::Value> {
            events.iter().map(|e| serde_json::json!({ "name": e.name, "payload": e.payload })).collect()
        };
        print_json(&serde_json::json!({
            "function": name,
            "local": list(&local),
            "shared": shared.as_ref().map(|s| list(s)).unwrap_or_default(),
            "sharedError": shared.as_ref().err().map(aws::AwsError::short),
        }));
        return Ok(ExitCode::SUCCESS);
    }
    println!("{} · test events", paint(name, "1"));
    let width = local.iter().chain(shared.iter().flatten()).map(|e| e.name.chars().count()).max().unwrap_or(0);
    let print = |events: &[Event]| {
        for e in events {
            println!("  {:<width$}  {}  {}", e.name, paint(&format!("{:<13}", e.place.label()), "90"), ellipsize(&e.compact(), 100));
        }
    };
    print(&local);
    match &shared {
        Ok(shared) => {
            print(shared);
            if local.is_empty() && shared.is_empty() {
                println!("  {}", paint(&format!("none yet · {} lambda {name} --save-event NAME --payload '{{…}}'", program()), "90"));
            }
        }
        Err(e) => println!("  {}", paint(&format!("the shared ones could not be read: {}", e.short()), "33")),
    }
    Ok(ExitCode::SUCCESS)
}

/// The saved event `event`: this machine's unless `shared`, else the one in AWS.
fn saved_event(aws: &Aws, name: &str, event: &str, shared: bool) -> Result<Event> {
    if !shared && let Some(e) = testevents::local(name).into_iter().find(|e| e.name == event) {
        return Ok(e);
    }
    testevents::shared(aws, name)?
        .into_iter()
        .find(|e| e.name == event)
        .ok_or_else(|| anyhow!("{name} has no saved test event named {event} ({} lambda {name} --events lists them)", program()))
}

fn save_event(aws: &Aws, env: &str, name: &str, event: &str, payload: &str, shared: bool, yes: bool) -> Result<ExitCode> {
    testevents::check_name(event).map_err(|e| anyhow!(e))?;
    let payload: serde_json::Value =
        serde_json::from_str(read_value(payload)?.trim()).map_err(|e| anyhow!("the payload is not valid JSON: {e}"))?;
    if !shared {
        testevents::save_local(name, event, &payload)?;
        println!("Saved the test event {event} of {name} on this machine");
        return Ok(ExitCode::SUCCESS);
    }
    if !confirm_write(env, &format!("Save the shared test event {event} of {name} in {env}? Everyone with the account sees it."), yes)? {
        bail!("not saved");
    }
    aws::schemas::save(aws, name, event, &payload)?;
    println!("Saved the shared test event {event} of {name} · the Lambda console lists it too");
    Ok(ExitCode::SUCCESS)
}

fn delete_event(aws: &Aws, env: &str, name: &str, event: &str, shared: bool, yes: bool) -> Result<ExitCode> {
    let found = if shared {
        if !confirm_write(env, &format!("Delete the shared test event {event} of {name} in {env}? Nobody sees it any more."), yes)? {
            bail!("not deleted");
        }
        aws::schemas::delete(aws, name, event)?
    } else {
        testevents::delete_local(name, event)?
    };
    if !found {
        bail!("{name} has no {} test event named {event}", if shared { "shared" } else { "local" });
    }
    println!("Deleted the {} test event {event} of {name}", if shared { "shared" } else { "local" });
    Ok(ExitCode::SUCCESS)
}

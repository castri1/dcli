//! `dcli sfn`: state machines, their executions, the steps of one, and failure reports.

use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use crate::aws::Aws;
use crate::aws::sfn::{self, StepStatus};
use crate::cli::{aws_for, env_and, paint, print_json};
use crate::config::Config;
use crate::util::{ellipsize, fmt_ago, fmt_duration, now_secs, program, sanitize};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// State machine name or ARN; without it, lists the state machines
    machine: Option<String>,
    /// Only executions with this status: running, failed, succeeded, timed-out, aborted
    #[arg(long)]
    status: Option<String>,
    /// How many executions to list
    #[arg(short = 'n', long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    json: bool,
    /// Markdown report of the latest failed execution (or of --execution), for an AI agent
    #[arg(long)]
    report: bool,
    /// An execution: its ARN, or its name with the machine. Prints its steps (or, with
    /// --report, its report)
    #[arg(long, value_name = "NAME|ARN")]
    execution: Option<String>,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let (env_name, machine) = env_and(config, args.env, args.machine)?;
    let aws = aws_for(config, &env_name)?;
    if let Some(execution_arn) = args.execution.as_deref().filter(|e| e.starts_with("arn:")) {
        return execution(&aws, &env_name, execution_arn, args.report, args.json);
    }
    let Some(machine) = machine else {
        if args.execution.is_some() {
            bail!("name the state machine too, or pass the execution ARN");
        }
        return list_machines(&aws, args.json);
    };
    let (machine_arn, kind) = resolve(&aws, &machine)?;
    if let Some(name) = &args.execution {
        let execution_arn = format!("{}:{name}", machine_arn.replacen(":stateMachine:", ":execution:", 1));
        return execution(&aws, &env_name, &execution_arn, args.report, args.json);
    }
    if kind == "EXPRESS" {
        let logs = sfn::describe_state_machine(&aws, &machine_arn)?.log_group();
        println!("{machine} is an Express state machine: AWS keeps no list of its executions, only logs.");
        match logs {
            Some(group) => println!("Read them with: {} tail {env_name} {group} --since 1h", program()),
            None => println!("It does not log to CloudWatch (turn logging on to see its executions)."),
        }
        return Ok(ExitCode::SUCCESS);
    }
    if args.report {
        let latest = sfn::list_executions(&aws, &machine_arn, Some("FAILED"), 1)?;
        let Some(failed) = latest.first() else {
            println!("No failed executions of {machine}.");
            return Ok(ExitCode::SUCCESS);
        };
        return execution(&aws, &env_name, &failed.execution_arn, true, false);
    }
    let status = args.status.as_deref().map(status_filter).transpose()?;
    let list = sfn::list_executions(&aws, &machine_arn, status.as_deref(), args.limit.clamp(1, 1000))?;
    if args.json {
        print_json(&serde_json::to_value(&list)?);
        return Ok(ExitCode::SUCCESS);
    }
    if list.is_empty() {
        let which = args.status.as_deref().map(|s| format!("{s} ")).unwrap_or_default();
        println!("No {which}executions of {machine}.");
        return Ok(ExitCode::SUCCESS);
    }
    let now = now_secs();
    for e in &list {
        let duration = e.start_date.map(|s| fmt_duration(e.stop_date.unwrap_or(now) - s)).unwrap_or_default();
        println!(
            "{} {:<40} {:<10} {:>8}",
            status_label(&e.status, 16),
            ellipsize(&e.name, 40),
            e.start_date.map(fmt_ago).unwrap_or_default(),
            duration
        );
    }
    println!(
        "{}",
        paint(&format!("Steps of one: {} sfn {env_name} {machine} --execution <name> · a report: add --report", program()), "90")
    );
    Ok(ExitCode::SUCCESS)
}

fn list_machines(aws: &Aws, json: bool) -> Result<ExitCode> {
    let mut machines = sfn::all_state_machines(aws)?;
    machines.sort_by(|a, b| a.name.cmp(&b.name));
    if json {
        print_json(&serde_json::to_value(&machines)?);
        return Ok(ExitCode::SUCCESS);
    }
    let width = machines.iter().map(|m| m.name.chars().count()).max().unwrap_or(4);
    for m in &machines {
        let created = m.creation_date.map(|t| format!("created {}", fmt_ago(t))).unwrap_or_default();
        println!("{:<width$}  {:<8}  {}", m.name, m.kind, paint(&created, "90"));
    }
    if machines.is_empty() {
        println!("No state machines.");
    }
    Ok(ExitCode::SUCCESS)
}

/// A state machine's ARN and type (`STANDARD`, `EXPRESS`) from its name or ARN.
fn resolve(aws: &Aws, machine: &str) -> Result<(String, String)> {
    if machine.starts_with("arn:") {
        let d = sfn::describe_state_machine(aws, machine)?;
        return Ok((d.state_machine_arn, d.kind));
    }
    let machines = sfn::all_state_machines(aws)?;
    machines.iter().find(|m| m.name == machine).map(|m| (m.state_machine_arn.clone(), m.kind.clone())).ok_or_else(|| {
        let names: Vec<&str> = machines.iter().map(|m| m.name.as_str()).collect();
        anyhow!("no state machine named '{machine}'. Available: {}", names.join(", "))
    })
}

/// `failed` / `timed-out` → `FAILED` / `TIMED_OUT`.
fn status_filter(s: &str) -> Result<String> {
    let value = s.trim().to_uppercase().replace(['-', ' '], "_");
    if !["RUNNING", "SUCCEEDED", "FAILED", "TIMED_OUT", "ABORTED", "PENDING_REDRIVE"].contains(&value.as_str()) {
        bail!("--status takes running, succeeded, failed, timed-out, aborted or pending-redrive");
    }
    Ok(value)
}

/// Pads before colouring so escape codes do not break the columns.
fn status_label(status: &str, width: usize) -> String {
    let (s, label) = sfn::status(status);
    let code = match status {
        "SUCCEEDED" => "32",
        "FAILED" | "TIMED_OUT" => "31",
        "RUNNING" => "94",
        "ABORTED" | "PENDING_REDRIVE" => "33",
        _ => "90",
    };
    paint(&format!("{:<width$}", format!("{} {label}", s.symbol())), code)
}

fn execution(aws: &Aws, env_name: &str, arn: &str, report: bool, json: bool) -> Result<ExitCode> {
    let detail = sfn::describe_execution(aws, arn)?;
    let events = sfn::execution_history(aws, arn)?;
    if report {
        let machine = sfn::describe_state_machine(aws, &detail.state_machine_arn)?;
        print!("{}", sanitize(&sfn::report(env_name, &aws.region, &machine, &detail, &events)));
        return Ok(ExitCode::SUCCESS);
    }
    let steps = sfn::steps(&events);
    let failure = sfn::failure(&steps, &events, Some(&detail));
    let now = now_secs();
    if json {
        let steps: Vec<_> = steps
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "type": s.kind,
                    "status": format!("{:?}", s.status).to_lowercase(),
                    "start": s.start,
                    "end": s.end,
                    "lambda": s.lambda,
                    "resource": s.resource,
                    "retries": s.retries,
                    "error": s.error,
                    "cause": s.cause,
                })
            })
            .collect();
        print_json(&json!({
            "execution": detail,
            "steps": steps,
            "failure": failure.map(|f| json!({ "state": f.state, "error": f.error, "cause": f.cause })),
        }));
        return Ok(ExitCode::SUCCESS);
    }
    let duration = detail.duration(now).map(fmt_duration).unwrap_or_default();
    let started = detail.start_date.map(fmt_ago).unwrap_or_default();
    println!("{}  {}  {}", paint(&detail.name, "1"), status_label(&detail.status, 0), paint(&format!("started {started} · {duration}"), "90"));
    if let Some(f) = &failure {
        let at = f.state.as_deref().map(|s| format!("Failed at {s}: ")).unwrap_or_default();
        println!("  {}", paint(&format!("{at}{}", f.error.as_deref().unwrap_or("")), "31"));
        if let Some(c) = &f.cause {
            for line in sfn::pretty_cause(c).lines().take(12) {
                println!("    {}", paint(line, "31"));
            }
        }
    }
    println!();
    let width = steps.iter().map(|s| s.name.chars().count()).max().unwrap_or(4);
    for s in &steps {
        let (symbol, code) = match s.status {
            StepStatus::Running => ("↻", "94"),
            StepStatus::Succeeded => ("✓", "32"),
            StepStatus::Failed => ("✗", "31"),
            StepStatus::Caught => ("↷", "33"),
        };
        let what = s.lambda.as_deref().map(|l| format!("λ {l}")).or_else(|| s.resource.clone()).unwrap_or_default();
        let retries = if s.retries > 0 { format!(" ↻{}", s.retries) } else { String::new() };
        println!(
            "  {} {:<width$}  {:<9} {:>8}  {}{}",
            paint(symbol, code),
            s.name,
            s.kind,
            s.duration(now).map(fmt_duration).unwrap_or_default(),
            paint(&what, "90"),
            paint(&retries, "33")
        );
    }
    if let Some(group) = steps.iter().rev().find(|s| s.status == StepStatus::Failed).and_then(|s| s.log_group()) {
        println!("\n{}", paint(&format!("Logs: {} tail {env_name} {group} --since 1h", program()), "90"));
    }
    Ok(ExitCode::SUCCESS)
}

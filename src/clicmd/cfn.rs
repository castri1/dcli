//! `dcli cfn`: CloudFormation stacks, a stack's events, resources, outputs, template,
//! drift and change sets, and a Markdown report of why its latest operation failed.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use crate::aws::Aws;
use crate::aws::cloudformation::{self as cfn, Health, Stack};
use crate::cli::{aws_for, env_and, paint, print_json};
use crate::config::Config;
use crate::util::{ellipsize, fmt_ago, fmt_utc, program, sanitize};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Stack name or ID; without it, lists the stacks
    stack: Option<String>,
    /// The stack's events, newest first
    #[arg(long, group = "view")]
    events: bool,
    /// The stack's resources
    #[arg(long, group = "view")]
    resources: bool,
    /// The stack's outputs (`--json` for scripts)
    #[arg(long, group = "view")]
    outputs: bool,
    /// The stack's template, as written
    #[arg(long, group = "view")]
    template: bool,
    /// The stack's change sets, or with --change-set the changes of one
    #[arg(long, group = "view")]
    changes: bool,
    /// A change set: what it would change (with --report, as Markdown to review)
    #[arg(long, value_name = "NAME")]
    change_set: Option<String>,
    /// Detect drift (resources changed outside CloudFormation) and show it
    #[arg(long, group = "view")]
    drift: bool,
    /// Markdown report of why the latest operation failed (or of the drift / change set),
    /// for an AI agent
    #[arg(long)]
    report: bool,
    /// Events to show at most
    #[arg(short = 'n', long, default_value_t = 30)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let (env_name, stack) = env_and(config, args.env.clone(), args.stack.clone())?;
    let aws = aws_for(config, &env_name)?;
    let Some(name) = stack else {
        if args.report || args.events || args.resources || args.outputs || args.template || args.changes || args.drift {
            bail!("which stack? {} cfn [ENV] STACK …", program());
        }
        return list(&aws, args.json);
    };
    let stack = cfn::describe_stacks(&aws, Some(&name))?.into_iter().next().ok_or_else(|| anyhow!("no stack named '{name}'"))?;
    if args.change_set.is_some() || args.changes {
        return changes(&aws, &env_name, &stack, args.change_set.as_deref(), args.report, args.json);
    }
    if args.drift {
        return drift(&aws, &env_name, &stack, args.report, args.json);
    }
    if args.report {
        return match cfn::failure(&aws, &env_name, &aws.region, &stack)? {
            Some(f) => {
                print!("{}", sanitize(&cfn::failure_report(&f)));
                Ok(ExitCode::SUCCESS)
            }
            None => {
                println!("Nothing failed in the latest operation of {} ({}).", stack.name, stack.status);
                Ok(ExitCode::SUCCESS)
            }
        };
    }
    if args.events {
        return events(&aws, &stack, args.limit, args.json);
    }
    if args.resources {
        let resources = cfn::list_resources(&aws, &stack.id)?;
        if args.json {
            print_json(&serde_json::to_value(&resources)?);
            return Ok(ExitCode::SUCCESS);
        }
        let width = resources.iter().map(|r| r.logical_id.chars().count()).max().unwrap_or(4).min(40);
        for r in &resources {
            let drift = r.drift.as_deref().filter(|d| !matches!(*d, "IN_SYNC" | "NOT_CHECKED")).unwrap_or("");
            println!(
                "{} {:<width$}  {:<32}  {}  {}",
                status_label(&r.status, 20),
                ellipsize(&r.logical_id, width),
                ellipsize(r.resource_type.trim_start_matches("AWS::"), 32),
                r.physical_id.as_deref().unwrap_or("—"),
                paint(drift, "33")
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    if args.outputs {
        if args.json {
            print_json(&serde_json::to_value(&stack.outputs)?);
            return Ok(ExitCode::SUCCESS);
        }
        for o in &stack.outputs {
            println!("{}  {}", paint(&o.key, "1"), o.value);
        }
        if stack.outputs.is_empty() {
            println!("{} has no outputs.", stack.name);
        }
        return Ok(ExitCode::SUCCESS);
    }
    if args.template {
        print!("{}", sanitize(&cfn::template(&aws, &stack.id)?));
        return Ok(ExitCode::SUCCESS);
    }
    show(&aws, &env_name, &stack, args.json)
}

/// The stacks, nested ones under their parent.
fn list(aws: &Aws, json: bool) -> Result<ExitCode> {
    let stacks = cfn::describe_stacks(aws, None)?;
    if json {
        print_json(&serde_json::to_value(&stacks)?);
        return Ok(ExitCode::SUCCESS);
    }
    if stacks.is_empty() {
        println!("No stacks.");
        return Ok(ExitCode::SUCCESS);
    }
    let tree = cfn::tree(&stacks);
    let width = tree.iter().map(|(i, depth)| depth * 2 + stacks[*i].name.chars().count()).max().unwrap_or(4).min(60);
    for (i, depth) in tree {
        let s = &stacks[i];
        let name = format!("{}{}", "  ".repeat(depth), s.name);
        let when = s.updated.or(s.created).map(fmt_ago).unwrap_or_default();
        let drift = if s.drift.as_deref() == Some("DRIFTED") { paint("drifted", "33") } else { String::new() };
        println!("{} {:<width$}  {:>8}  {drift}", status_label(&s.status, 26), ellipsize(&name, width), when);
    }
    Ok(ExitCode::SUCCESS)
}

fn show(aws: &Aws, env_name: &str, s: &Stack, json: bool) -> Result<ExitCode> {
    if json {
        print_json(&serde_json::to_value(s)?);
        return Ok(ExitCode::SUCCESS);
    }
    let row = |k: &str, v: String| println!("  {:<12} {v}", paint(k, "90"));
    println!("{}  {}", paint(&s.name, "1"), status_label(&s.status, 0));
    if let Some(r) = &s.status_reason {
        row("Reason", r.clone());
    }
    if let Some(d) = &s.description {
        row("Description", d.clone());
    }
    if let Some(t) = s.created {
        row("Created", format!("{} · {}", fmt_ago(t), fmt_utc(t)));
    }
    if let Some(t) = s.updated {
        row("Updated", format!("{} · {}", fmt_ago(t), fmt_utc(t)));
    }
    let checked = s.drift_checked.map(|t| format!(" (checked {})", fmt_ago(t))).unwrap_or_default();
    row("Drift", format!("{}{checked}", s.drift.as_deref().unwrap_or("NOT_CHECKED")));
    if let Some(p) = &s.parent_id {
        row("Nested in", cfn::stack_name_from_id(p).to_string());
    }
    if s.failed()
        && let Some(t) = cfn::trail(aws, s)?
    {
        let c = &t.cause;
        let nested = if t.chain.len() > 1 { format!(" in nested stack {}", c.stack_name) } else { String::new() };
        let reason = c.reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
        row("Root cause", paint(&format!("{} {}{nested}{reason}", c.logical_id, c.status), "31"));
    }
    if !s.parameters.is_empty() {
        println!("  {}", paint("Parameters", "90"));
        for (k, v) in &s.parameters {
            println!("    {k} = {v}");
        }
    }
    if !s.outputs.is_empty() {
        println!("  {}", paint("Outputs", "90"));
        for o in &s.outputs {
            println!("    {} = {}", o.key, o.value);
        }
    }
    let more = if s.failed() { " · why it failed: --report" } else { "" };
    println!(
        "{}",
        paint(&format!("More: {} cfn {env_name} {} --events | --resources | --template | --drift | --changes{more}", program(), s.name), "90")
    );
    Ok(ExitCode::SUCCESS)
}

fn events(aws: &Aws, s: &Stack, limit: usize, json: bool) -> Result<ExitCode> {
    let mut events = cfn::stack_events(aws, &s.id, 0.0, limit.div_ceil(100).max(1))?;
    events.truncate(limit.max(1));
    if json {
        print_json(&serde_json::to_value(&events)?);
        return Ok(ExitCode::SUCCESS);
    }
    let width = events.iter().map(|e| e.logical_id.chars().count()).max().unwrap_or(4).min(40);
    for e in &events {
        let when = e.timestamp.map(|t| fmt_utc(t).replace(" UTC", "")).unwrap_or_default();
        println!("{when}  {} {:<width$}  {}", status_label(&e.status, 34), ellipsize(&e.logical_id, width), paint(&e.resource_type, "90"));
        if let Some(r) = &e.reason {
            let code = if e.failed() { "31" } else { "90" };
            println!("      {}", paint(r, code));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Detects drift (polling until it finishes) and prints what changed.
fn drift(aws: &Aws, env_name: &str, s: &Stack, report: bool, json: bool) -> Result<ExitCode> {
    let id = cfn::detect_drift(aws, &s.id)?;
    eprintln!("Detecting drift of {}…", s.name);
    let mut detection = cfn::drift_detection(aws, &id)?;
    for _ in 0..150 {
        if detection.status != "DETECTION_IN_PROGRESS" {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
        detection = cfn::drift_detection(aws, &id)?;
    }
    if detection.status == "DETECTION_FAILED" {
        bail!("drift detection failed: {}", detection.reason.unwrap_or_default());
    }
    let drifts = cfn::resource_drifts(aws, &s.id)?;
    if json {
        print_json(&serde_json::to_value(&drifts)?);
        return Ok(ExitCode::SUCCESS);
    }
    if report {
        let mut stack = s.clone();
        stack.drift_checked = Some(crate::util::now_secs());
        print!("{}", sanitize(&cfn::drift_report(env_name, &aws.region, &stack, &drifts)));
        return Ok(ExitCode::SUCCESS);
    }
    let drifted: Vec<&cfn::ResourceDrift> = drifts.iter().filter(|d| !matches!(d.status.as_str(), "IN_SYNC" | "NOT_CHECKED")).collect();
    if drifted.is_empty() {
        println!("{}: every checked resource matches the template ({} checked).", s.name, drifts.len());
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}: {} of {} resources drifted", s.name, drifted.len(), drifts.len());
    for d in drifted {
        println!("{} {}  {}", paint(&format!("{:<9}", d.status), "33"), d.logical_id, paint(&d.resource_type, "90"));
        for diff in &d.differences {
            println!(
                "    {} {}: {} → {}",
                diff.kind,
                diff.path,
                paint(diff.expected.as_deref().unwrap_or("—"), "90"),
                paint(diff.actual.as_deref().unwrap_or("—"), "33")
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn changes(aws: &Aws, env_name: &str, s: &Stack, name: Option<&str>, report: bool, json: bool) -> Result<ExitCode> {
    let sets = cfn::list_change_sets(aws, &s.id)?;
    let Some(name) = name else {
        if json {
            print_json(&serde_json::to_value(&sets)?);
            return Ok(ExitCode::SUCCESS);
        }
        for c in &sets {
            let when = c.created.map(fmt_ago).unwrap_or_default();
            println!("{:<36} {:<16} {:<18} {when}", c.name, c.status, c.execution_status);
        }
        if sets.is_empty() {
            println!("{} has no change sets.", s.name);
        } else {
            println!("{}", paint(&format!("Changes of one: {} cfn {env_name} {} --change-set <name>", program(), s.name), "90"));
        }
        return Ok(ExitCode::SUCCESS);
    };
    let set = sets.iter().find(|c| c.name == name || c.id == name).ok_or_else(|| anyhow!("{} has no change set '{name}'", s.name))?;
    let changes = cfn::describe_change_set(aws, &s.id, &set.id)?;
    if json {
        print_json(&serde_json::to_value(&changes)?);
    } else if report {
        print!("{}", sanitize(&cfn::change_set_report(env_name, &aws.region, s, set, &changes)));
    } else {
        for c in &changes {
            let replace = match c.replacement.as_deref() {
                Some("True") => paint("replaces it", "31"),
                Some("Conditional") => paint("may replace it", "33"),
                _ => String::new(),
            };
            println!("{:<7} {}  {}  {replace}", c.action, c.logical_id, paint(&c.resource_type, "90"));
            for d in &c.details {
                println!("          {}", paint(d, "90"));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The status, coloured by how the stack is doing, padded to `width` before colouring.
fn status_label(status: &str, width: usize) -> String {
    let code = match cfn::health(status) {
        Health::InProgress => "36",
        Health::RollingBack | Health::RolledBack => "33",
        Health::Failed => "31",
        Health::Ok => "32",
    };
    paint(&format!("{status:<width$}"), code)
}

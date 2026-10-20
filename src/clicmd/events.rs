//! `dcli events`: the rules of an event bus, and `dcli events send` for a test event.

use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::aws::events;
use crate::cli::{aws_for, confirm_write, current_env, env_and, paint, print_json, read_value};
use crate::config::Config;

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
    #[command(subcommand)]
    action: Option<Action>,
    /// Environment (default: the current one)
    env: Option<String>,
    /// Event bus (default: "default")
    bus: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Action {
    /// Send an event to a bus (asks for confirmation)
    Send {
        env: Option<String>,
        #[arg(long)]
        source: String,
        #[arg(long = "detail-type", value_name = "TYPE")]
        detail_type: String,
        /// The detail, a JSON object ("-" reads stdin, with --yes)
        #[arg(long, value_name = "JSON")]
        detail: String,
        #[arg(long, default_value = "default")]
        bus: String,
        /// Do not ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    match args.action {
        Some(Action::Send { env, source, detail_type, detail, bus, yes }) => {
            let env = match env {
                Some(e) => e,
                None => current_env(config)?,
            };
            send(config, &env, &bus, &source, &detail_type, &read_value(&detail)?, yes)
        }
        None => {
            let (env, bus) = env_and(config, args.env, args.bus)?;
            rules(config, &env, bus.as_deref().unwrap_or("default"), args.json)
        }
    }
}

fn rules(config: &Config, env: &str, bus: &str, json: bool) -> Result<ExitCode> {
    let aws = aws_for(config, env)?;
    let mut rules = events::list_rules(&aws, bus)?;
    rules.sort_by(|a, b| a.name.cmp(&b.name));
    // Targets of every rule, in parallel.
    let targets: Vec<Vec<events::Target>> = std::thread::scope(|scope| {
        let handles: Vec<_> = rules.iter().map(|r| scope.spawn(|| events::list_targets(&aws, bus, &r.name))).collect();
        handles.into_iter().map(|h| h.join().ok().and_then(Result::ok).unwrap_or_default()).collect()
    });

    if json {
        let rows: Vec<Value> = rules
            .iter()
            .zip(&targets)
            .map(|(r, t)| {
                json!({
                    "name": r.name,
                    "state": r.state,
                    "description": r.description,
                    "eventPattern": r.event_pattern.as_deref().and_then(|p| serde_json::from_str::<Value>(p).ok()),
                    "schedule": r.schedule_expression,
                    "targets": t.iter().map(|t| json!({ "id": t.id, "arn": t.arn, "input": t.input_summary() })).collect::<Vec<_>>(),
                })
            })
            .collect();
        print_json(&json!({ "bus": bus, "rules": rows }));
        return Ok(ExitCode::SUCCESS);
    }

    println!("{}", paint(&format!("{bus} · {env} · {} rules", rules.len()), "90"));
    let width = rules.iter().map(|r| r.name.chars().count()).max().unwrap_or(4) + 2;
    let summary_width = rules.iter().map(|r| r.summary().chars().count()).max().unwrap_or(0).min(70) + 2;
    for (r, t) in rules.iter().zip(&targets) {
        let (state, code) = if r.enabled() { ("● on ", "32") } else { ("○ off", "90") };
        let targets = match t.len() {
            1 => format!("→ {} {}", t[0].kind(), t[0].name()),
            n => format!("→ {n} targets"),
        };
        // Pad before colouring so escape codes do not break the columns.
        let summary = format!("{:<summary_width$}", crate::util::ellipsize(&r.summary(), 70));
        println!("{}  {:<width$} {}{}", paint(state, code), r.name, paint(&summary, "90"), paint(&targets, "36"));
    }
    Ok(ExitCode::SUCCESS)
}

fn send(config: &Config, env: &str, bus: &str, source: &str, detail_type: &str, detail: &str, yes: bool) -> Result<ExitCode> {
    let parsed: Value = serde_json::from_str(detail).map_err(|e| anyhow::anyhow!("--detail is not valid JSON: {e}"))?;
    if !parsed.is_object() {
        bail!("--detail must be a JSON object");
    }
    let aws = aws_for(config, env)?;
    if !confirm_write(env, &format!("Send a {detail_type} event from {source} to bus {bus} in {env}?"), yes)? {
        println!("Cancelled.");
        return Ok(ExitCode::FAILURE);
    }
    let result = events::put_event(&aws, bus, source, detail_type, &parsed.to_string())?;
    match result.event_id {
        Some(id) => {
            println!("{} Sent event {id} to {bus}", paint("✓", "32"));
            Ok(ExitCode::SUCCESS)
        }
        None => bail!(
            "{}: {}",
            result.error_code.unwrap_or_else(|| "Failed".into()),
            result.error_message.unwrap_or_default()
        ),
    }
}

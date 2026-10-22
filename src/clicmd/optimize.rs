//! `dcli optimize`: what could cost less in an environment, in the units AWS bills (see
//! `savings`): unused or oversized resources, with the evidence behind each. `--json` for
//! scripts and agents, `--report` for a Markdown plan, `--lambda-memory` to also read the
//! memory functions use (Logs Insights bills the data it scans: dcli says how much first).

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use crate::cli::{aws_for, confirm, current_env, interactive, paint, print_json};
use crate::config::Config;
use crate::savings::lambda::{self as memory, MemoryRun};
use crate::savings::{self, DEFAULT_DAYS, Finding, Impact, find_service, short_name, size};
use crate::util::{rfc3339, sanitize};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Only these services: lambda, logs, sqs, dynamodb, s3, sfn, secrets, alarms, apigw, aurora
    #[arg(long, value_delimiter = ',', value_name = "SERVICES")]
    service: Vec<String>,
    /// Days of usage to look at
    #[arg(long, default_value_t = DEFAULT_DAYS, value_parser = clap::value_parser!(u32).range(1..=455))]
    days: u32,
    /// Also read the memory Lambda functions use, with Logs Insights (billed per GB scanned)
    #[arg(long)]
    lambda_memory: bool,
    /// Do not ask before the memory analysis
    #[arg(short, long)]
    yes: bool,
    /// Markdown plan for an AI agent
    #[arg(long, conflicts_with = "json")]
    report: bool,
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let env_name = match &args.env {
        Some(env) => env.clone(),
        None => current_env(config)?,
    };
    let services = args
        .service
        .iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            find_service(s).ok_or_else(|| {
                anyhow!("unknown service '{}': use lambda, logs, sqs, dynamodb, s3, sfn, secrets, alarms, apigw or aurora", sanitize(s))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if args.lambda_memory && !args.yes && !interactive() {
        bail!("pass --yes to run the memory analysis without asking (Logs Insights bills the data it scans)");
    }
    let aws = aws_for(config, &env_name)?;
    let progress = std::io::stderr().is_terminal() && !args.json;
    if progress {
        eprintln!("{}", paint(&format!("Looking at the last {} of {env_name} ({})…", savings::days_label(args.days), aws.region), "90"));
    }
    let mut outcome = savings::run(&aws, args.days, &services, &|service, result| {
        if progress {
            match result {
                Ok(found) => eprintln!("  {} {service} · {}", paint("✓", "32"), found.len()),
                Err(e) => eprintln!("  {} {service} · {}", paint("✗", "31"), e.short()),
            }
        }
    });
    let mut memory_run: Option<MemoryRun> = None;
    if args.lambda_memory {
        let estimate = memory::memory_estimate(&aws, None, args.days)?;
        if estimate.functions.is_empty() {
            eprintln!("No Lambda function to analyse.");
        } else {
            eprintln!("Lambda memory: Logs Insights would scan {} (Insights bills the data it scans).", estimate.describe());
            if !estimate.shared.is_empty() {
                eprintln!("{}", paint(&format!("  {} functions share a log group and are left out", estimate.shared.len()), "90"));
            }
            if args.yes || confirm("Run it?") {
                let run = memory::memory_analysis(&aws, &estimate, &|done, total| {
                    if progress && done < total {
                        eprintln!("  {}", paint(&format!("reading log groups {}–{} of {total}…", done + 1, (done + 50).min(total)), "90"));
                    }
                })?;
                eprintln!("Lambda memory: {} functions read · {} scanned.", run.uses.len(), size(run.bytes_scanned));
                outcome.findings.extend(run.uses.iter().filter_map(memory::memory_finding).map(|f| Finding { service: "Lambda", ..f }));
                savings::sort(&mut outcome.findings);
                memory_run = Some(run);
            }
        }
    }

    if args.json {
        print_json(&json!({
            "env": env_name,
            "region": aws.region,
            "days": outcome.days,
            "at": rfc3339(outcome.at as i64),
            "findings": outcome.findings,
            "errors": outcome.errors.iter().map(|(s, e)| json!({ "service": s, "error": e.short() })).collect::<Vec<_>>(),
            "lambdaMemory": memory_run.as_ref().map(|r| json!({ "uses": r.uses, "bytesScanned": r.bytes_scanned, "withoutInvocations": r.silent })),
        }));
        return Ok(ExitCode::SUCCESS);
    }
    if args.report {
        print!("{}", savings::report(&env_name, &aws.region, &outcome));
        return Ok(ExitCode::SUCCESS);
    }
    print_table(&outcome.findings);
    for (service, e) in &outcome.errors {
        eprintln!("{}", paint(&format!("! {service} not checked: {}", e.short()), "33"));
    }
    let count = |impact| outcome.findings.iter().filter(|f| f.impact == impact).count();
    println!(
        "{}",
        paint(
            &format!(
                "{} findings ({} high, {} medium, {} low) · last {} · no prices: reductions are in the units AWS bills · --report for the evidence",
                outcome.findings.len(),
                count(Impact::High),
                count(Impact::Medium),
                count(Impact::Low),
                savings::days_label(outcome.days)
            ),
            "90"
        )
    );
    if !args.lambda_memory {
        println!("{}", paint("Lambda memory not read: add --lambda-memory (Logs Insights bills the data it scans)", "90"));
    }
    Ok(ExitCode::SUCCESS)
}

fn print_table(findings: &[Finding]) {
    if findings.is_empty() {
        println!("Nothing found to change.");
        return;
    }
    let resource_w = findings.iter().map(|f| f.resource.chars().count()).max().unwrap_or(8).clamp(8, 40);
    for f in findings {
        let color = match f.impact {
            Impact::High => "31;1",
            Impact::Medium => "33",
            Impact::Low => "36",
        };
        let resource = crate::util::ellipsize(&sanitize(&f.resource), resource_w);
        println!(
            "{} {:<8} {}  {}",
            paint(f.impact.dots(), color),
            short_name(f.service),
            paint(&format!("{resource:<resource_w$}"), "1"),
            sanitize(&f.summary)
        );
        println!("{:>w$}{} {}", "", paint(&format!("→ {}", sanitize(&f.recommendation)), "90"), paint(&format!("· {}", f.reduction), "36"), w = resource_w + 15);
    }
}

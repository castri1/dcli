//! `dcli optimize`: what could cost less in an environment, in the units AWS bills (see
//! `savings`): unused or oversized resources, with the evidence behind each, and in dollars
//! at the account's own rates when its costs were read (`dcli costs`). `--json` for scripts
//! and agents, `--report` for a Markdown plan, `--lambda-memory` to also read the memory
//! functions use (Logs Insights bills the data it scans: dcli says how much first),
//! `--by-cost` for the bill's usage types split over their resources, `--ledger` and
//! `--verify` for the changes followed and what the bill did after them.

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use crate::aws::Aws;
use crate::cli::{aws_for, confirm, current_env, interactive, paint, print_json};
use crate::config::Config;
use crate::costs::{self, attribution, ledger};
use crate::savings::lambda::{self as memory, MemoryRun};
use crate::savings::price::{self, money};
use crate::savings::{self, DEFAULT_DAYS, Finding, Impact, find_service, short_name, size};
use crate::util::{ellipsize, fmt_ago, fmt_utc, now_secs, rfc3339, sanitize};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Only these services: lambda, logs, sqs, dynamodb, s3, sfn, secrets, alarms, metrics, apigw, aurora, redshift, dms, ec2, xray, bedrock
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
    /// The bill's most expensive usage types split over the resources behind them (needs the
    /// costs read: `dcli costs`); reads CloudWatch only
    #[arg(long, conflicts_with_all = ["ledger", "verify", "report"])]
    by_cost: bool,
    /// The changes followed (applied, marked done) and what the bill did after them
    #[arg(long, conflicts_with_all = ["verify", "report"])]
    ledger: bool,
    /// Read the bill for the changes followed that are a week old and not verified yet: one
    /// Cost Explorer request each (asks first unless --yes)
    #[arg(long, conflicts_with = "report")]
    verify: bool,
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
                anyhow!("unknown service '{}': use lambda, logs, sqs, dynamodb, s3, sfn, secrets, alarms, metrics, apigw, aurora, redshift, dms, ec2, xray or bedrock", sanitize(s))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if args.lambda_memory && !args.yes && !interactive() {
        bail!("pass --yes to run the memory analysis without asking (Logs Insights bills the data it scans)");
    }
    let aws = aws_for(config, &env_name)?;
    if args.ledger {
        return print_ledger(&env_name, args.json);
    }
    if args.verify {
        return verify(&aws, &env_name, args.yes, args.json);
    }
    if args.by_cost {
        return by_cost(&aws, &env_name, args.json);
    }
    let progress = std::io::stderr().is_terminal() && !args.json;
    if progress {
        eprintln!("{}", paint(&format!("Looking at the last {} of {env_name} ({})…", savings::days_label(args.days), aws.region), "90"));
    }
    let mut outcome = savings::run(&aws, &env_name, args.days, &services, &|service, result| {
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
                let run = memory::memory_analysis(&aws, &env_name, &estimate, &|done, total| {
                    if progress && done < total {
                        eprintln!("  {}", paint(&format!("reading log groups {}–{} of {total}…", done + 1, (done + 50).min(total)), "90"));
                    }
                })?;
                eprintln!("Lambda memory: {} functions read · {} scanned.", run.uses.len(), size(run.bytes_scanned));
                // The scan brought the findings of kept analyses: this one replaces its functions'.
                outcome.findings.retain(|f| !(f.kind == "memory" && run.uses.iter().any(|u| u.function == f.target)));
                outcome.findings.extend(run.uses.iter().filter_map(memory::memory_finding).map(|f| Finding { service: "Lambda", ..f }));
                outcome.priced_at = savings::price_with_costs(&mut outcome.findings, &env_name, &aws.region);
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
            "costsReadAt": outcome.priced_at.map(|t| rfc3339(t as i64)),
            "total": price::total(&outcome.findings),
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
                "{} findings ({} high, {} medium, {} low) · last {} · --report for the evidence",
                outcome.findings.len(),
                count(Impact::High),
                count(Impact::Medium),
                count(Impact::Low),
                savings::days_label(outcome.days)
            ),
            "90"
        )
    );
    match (outcome.priced_at, price::total(&outcome.findings)) {
        (Some(read), Some(t)) if t.priced > 0 => println!(
            "{}",
            paint(
                &format!(
                    "≈ {} a month with your bill's rates, {} of it right away · {} priced · costs read {} (each resource's usage counted once)",
                    money(t.monthly, &t.currency),
                    money(t.now, &t.currency),
                    t.priced,
                    fmt_ago(read)
                ),
                "32"
            )
        ),
        (Some(read), _) => println!("{}", paint(&format!("No finding on this bill's usage types (costs read {})", fmt_ago(read)), "90")),
        (None, _) => println!("{}", paint("No dollars: reductions are in the units AWS bills · `dcli costs` reads the bill to price them", "90")),
    }
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
        let saves = f.price.as_ref().map(|p| paint(&format!(" · {}", p.describe()), "32")).unwrap_or_default();
        println!(
            "{:>w$}{} {}{saves}",
            "",
            paint(&format!("→ {}", sanitize(&f.recommendation)), "90"),
            paint(&format!("· {}", f.reduction), "36"),
            w = resource_w + 15
        );
    }
}

/// The bill's most expensive usage types split over their resources.
fn by_cost(aws: &Aws, env: &str, json: bool) -> Result<ExitCode> {
    let Some(costs) = costs::cached(env) else {
        bail!("no costs read for {env} yet: `dcli costs {env}` reads the bill first");
    };
    if !json {
        eprintln!("{}", paint(&format!("Splitting the costs read {} over the resources of {env} ({})…", fmt_ago(costs.read_at), aws.region), "90"));
    }
    let splits = attribution::split(aws, &costs, &|_, _| {});
    if json {
        print_json(&json!({ "env": env, "region": aws.region, "costsReadAt": rfc3339(costs.read_at as i64), "splits": splits }));
        return Ok(ExitCode::SUCCESS);
    }
    if splits.is_empty() {
        println!("None of the bill's usage types in {} is one dcli can split.", aws.region);
    }
    for s in &splits {
        println!(
            "{} {}  {}",
            paint(&sanitize(&s.usage), "1"),
            paint(&format!("{} in 30 days", money(s.cost, &s.currency)), "32"),
            paint(&format!("{} · by {}", s.service, s.by), "90")
        );
        if let Some(e) = &s.error {
            println!("    {}", paint(&format!("not split: {e}"), "31"));
        } else if s.shares.is_empty() {
            println!("    {}", paint("no resource here uses it now", "90"));
        }
        for share in s.shares.iter().take(10) {
            println!("    {:<40} {:>4.0} %  ≈ {}", ellipsize(&sanitize(&share.resource), 40), share.share * 100.0, money(share.cost, &s.currency));
        }
        if s.shares.len() > 10 {
            println!("    {}", paint(&format!("… {} more", s.shares.len() - 10), "90"));
        }
    }
    println!("{}", paint("Estimated by usage: each usage type's total shared out by what each resource used, not lines of the bill.", "90"));
    Ok(ExitCode::SUCCESS)
}

fn print_ledger(env: &str, json: bool) -> Result<ExitCode> {
    let entries = ledger::of_env(env);
    let now = now_secs();
    if json {
        let list: Vec<_> = entries
            .iter()
            .map(|e| json!({ "entry": e, "status": ledger::status(e, now).label(), "savedMonthly": e.verified.as_ref().map(|v| v.saved_monthly()) }))
            .collect();
        print_json(&json!({ "env": env, "changes": list }));
        return Ok(ExitCode::SUCCESS);
    }
    if entries.is_empty() {
        println!("No change followed in {env} yet: apply a fix or mark a finding done in the Savings module (d).");
        return Ok(ExitCode::SUCCESS);
    }
    for e in &entries {
        let status = ledger::status(e, now);
        let result = match &e.verified {
            Some(v) => format!("{} · {}", status.label(), v.difference(&e.currency)),
            None => status.label(),
        };
        let expected = e.expected.map(|x| format!("expected ≈ {}/mo", money(x, &e.currency))).unwrap_or_else(|| "not priced".into());
        println!(
            "{} {}  {}  {}",
            paint(fmt_utc(e.at).split(' ').next().unwrap_or_default(), "90"),
            sanitize(&e.what),
            paint(&expected, "36"),
            paint(&result, if matches!(status, ledger::Status::AsExpected) { "32" } else { "33" })
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn verify(aws: &Aws, env: &str, yes: bool, json: bool) -> Result<ExitCode> {
    let now = now_secs();
    let ready: Vec<ledger::Entry> = ledger::of_env(env).into_iter().filter(|e| ledger::status(e, now) == ledger::Status::Ready).collect();
    if ready.is_empty() {
        if !json {
            println!("Nothing to verify in {env}: changes need a week of bill after them (see --ledger).");
        }
        return if json { print_ledger(env, true) } else { Ok(ExitCode::SUCCESS) };
    }
    let (n, s) = (ready.len(), if ready.len() == 1 { "" } else { "s" });
    if !yes {
        if !interactive() {
            bail!(
                "pass --yes to read the bill for {n} change{s} ({n} Cost Explorer request{s}, up to {} each if paged; each billed)",
                ledger::VERIFY_PAGES
            );
        }
        let question = format!(
            "Read the bill for {n} change{s}? {n} Cost Explorer request{s}, up to {} each if the answer comes in pages (AWS bills each one; {} this month so far)",
            ledger::VERIFY_PAGES,
            costs::settings::requests_this_month()
        );
        if !confirm(&question) {
            bail!("not read");
        }
    }
    for mut e in ready {
        match ledger::verify(aws, &e, now) {
            Ok(v) => {
                ledger::verified(&mut e, v);
                ledger::put(e.clone())?;
            }
            Err(err) => eprintln!("{}", paint(&format!("! {}: {}", sanitize(&e.what), err.short()), "33")),
        }
    }
    print_ledger(env, json)
}

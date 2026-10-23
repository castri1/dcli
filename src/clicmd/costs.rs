//! `dcli costs`: what an environment's account costs, by service and usage type, commitments
//! and their coverage, from Cost Explorer (each request billed; results kept on disk).

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use crate::aws::profile::AwsFiles;
use crate::cli::{aws_for, confirm, env_and, interactive, paint, print_json};
use crate::config::Config;
use crate::costs::analysis::{self, Deploy, Step, change, money, rate};
use crate::costs::explorer::{self, date};
use crate::costs::settings::{self, Refresh};
use crate::costs::{self, ServiceCost, Snapshot, services};
use crate::savings::qty;
use crate::tui::chart::{Agg, columns, sparkline};
use crate::util::{ellipsize, fmt_ago, now_secs, program};

const DAY: f64 = 86_400.0;

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Read the costs again from Cost Explorer (about 7 requests, plus one per service with reserved instances; each billed by AWS)
    #[arg(long)]
    refresh: bool,
    /// The usage types of a service (Cost Explorer's name or the dcli module's) with their
    /// effective rates
    #[arg(long, value_name = "SERVICE", group = "view")]
    usage: Option<String>,
    /// A service's cost day by day, next to the days the environment deployed
    #[arg(long, value_name = "SERVICE", group = "view")]
    daily: Option<String>,
    /// Savings Plans and reservations: use, coverage, AWS's purchase recommendation
    #[arg(long, group = "view")]
    commitments: bool,
    /// Markdown report for an AI agent
    #[arg(long, group = "view")]
    report: bool,
    #[arg(long)]
    json: bool,
    /// Choose who reads the costs (--profile or --own-profile) and when (--mode)
    #[arg(long)]
    setup: bool,
    /// With --setup: the profile that reads every environment's costs (the management
    /// account's, with AWS Organizations)
    #[arg(long, value_name = "NAME", requires = "setup", conflicts_with = "own_profile")]
    profile: Option<String>,
    /// With --setup: each environment reads its own costs with its own profile
    #[arg(long, requires = "setup")]
    own_profile: bool,
    /// With --setup: manual (only with --refresh or r in the UI) or daily (read again when
    /// older than a day)
    #[arg(long, value_name = "MODE", requires = "setup")]
    mode: Option<String>,
    /// Do not ask before reading Cost Explorer
    #[arg(short, long)]
    yes: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    if args.setup {
        return setup(&args);
    }
    let (env, _) = env_and(config, args.env.clone(), None)?;
    let profile = config.env(&env)?.profile.clone();
    let aws = aws_for(config, &env)?;
    let now = now_secs();
    let mut snapshot = costs::cached(&env);
    // Once a day, and not again soon after an attempt (one that failed is not retried at
    // every run).
    let daily = !args.refresh && settings::daily_due(&env, snapshot.as_ref().map(|s| s.read_at), now);
    if daily {
        settings::daily_attempted(&env, now);
    }
    if args.refresh || daily {
        if args.refresh && !args.yes {
            if !interactive() {
                bail!("reading Cost Explorer makes billed requests: pass --yes to read without being asked");
            }
            let question = format!(
                "Read the costs of {env} with {}: {} to Cost Explorer, each billed by AWS ({} this month so far)?",
                explorer::reader_label(&settings::load()),
                explorer::requests_text(&env),
                settings::requests_this_month()
            );
            if !confirm(&question) {
                bail!("not read");
            }
        }
        let progress_shown = std::io::stderr().is_terminal() && !args.json;
        if progress_shown {
            eprintln!("{}", paint(&format!("Reading the costs of {env}{}…", if args.refresh { "" } else { " (once a day)" }), "90"));
        }
        let progress = |step: &str| {
            if progress_shown {
                eprintln!("{}", paint(&format!("  {step}"), "90"));
            }
        };
        match explorer::read(&env, &profile, &aws, now, &progress) {
            Ok(s) => {
                if progress_shown {
                    eprintln!(
                        "{}",
                        paint(&format!("Read: {} ({} requests this month)", analysis::read_note(&s), settings::requests_this_month()), "90")
                    );
                }
                snapshot = Some(s);
            }
            // The once-a-day reading failing leaves what was read before.
            Err(e) if daily && snapshot.is_some() => eprintln!("{}", paint(&format!("! could not read the costs again: {}", e.short()), "33")),
            Err(e) => return Err(e.into()),
        }
    }
    let Some(s) = snapshot else {
        bail!(
            "the costs of {env} have not been read yet: {} costs {env} --refresh ({} to Cost Explorer, each billed by AWS)",
            program(),
            explorer::requests_text(&env)
        );
    };
    let pipelines: Vec<String> = config.env(&env)?.pipelines.iter().map(|p| p.name.clone()).collect();
    let deploys = || analysis::deploys(&aws, &pipelines, now - 62.0 * DAY);
    if let Some(name) = &args.usage {
        return usage(&s, find(&s, name)?, args.json);
    }
    if let Some(name) = &args.daily {
        let (deploys, failed) = deploys();
        return daily_view(&s, find(&s, name)?, &deploys, &failed, args.json);
    }
    if args.commitments {
        return commitments(&s, args.json);
    }
    let (deploys, _) = deploys();
    let steps = analysis::steps(&s, &deploys, now);
    if args.report {
        print!("{}", analysis::report(&env, &s, &steps));
        return Ok(ExitCode::SUCCESS);
    }
    summary(config, &env, &s, &steps, args.json)
}

fn setup(args: &Args) -> Result<ExitCode> {
    if let Some(p) = &args.profile
        && !AwsFiles::load().profile_names().contains(p)
    {
        bail!("no profile named '{p}' in ~/.aws/config");
    }
    let refresh = match args.mode.as_deref().map(str::to_lowercase).as_deref() {
        None => None,
        Some("manual") => Some(Refresh::Manual),
        Some("daily") => Some(Refresh::Daily),
        Some(other) => bail!("--mode is manual or daily, not '{other}'"),
    };
    let s = settings::update(|s| {
        if let Some(p) = &args.profile {
            s.profile = Some(p.clone());
        } else if args.own_profile {
            s.profile = None;
        }
        if let Some(r) = refresh {
            s.refresh = r;
        }
    })?;
    println!(
        "Costs are read by {}, {}",
        explorer::reader_label(&s),
        if s.refresh == Refresh::Daily { "again when older than a day" } else { "only when asked (--refresh, or r in the UI)" }
    );
    Ok(ExitCode::SUCCESS)
}

/// The service `name` names: Cost Explorer's name or the dcli module's, else the only one
/// whose name contains it.
fn find<'a>(s: &'a Snapshot, name: &str) -> Result<&'a ServiceCost> {
    let lower = name.to_lowercase();
    if let Some(x) = s.services.iter().find(|x| x.service.to_lowercase() == lower || services::module(&x.service).is_some_and(|m| m.to_lowercase() == lower)) {
        return Ok(x);
    }
    let some: Vec<&ServiceCost> = s.services.iter().filter(|x| x.service.to_lowercase().contains(&lower)).collect();
    match some.as_slice() {
        [one] => Ok(one),
        [] => Err(anyhow!("no service named '{name}' in the costs read")),
        many => Err(anyhow!("'{name}' could be {}", many.iter().map(|x| x.service.as_str()).collect::<Vec<_>>().join(", "))),
    }
}

fn summary(config: &Config, env: &str, s: &Snapshot, steps: &[Step], json: bool) -> Result<ExitCode> {
    let c = s.currency.as_str();
    if json {
        print_json(&json!({ "environment": env, "costs": s, "costAfterDeploys": steps }));
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{}  {}",
        paint(env, "1"),
        paint(&format!("account {} · amortized · read {} by {}", s.account, fmt_ago(s.read_at), s.reader), "90")
    );
    let shared = shared_with(config, env, &s.account);
    if !shared.is_empty() {
        let verb = if shared.len() == 1 { "uses" } else { "use" };
        println!("{}", paint(&format!("{} {verb} this account too: these are the account's costs", shared.join(", ")), "90"));
    }
    // Complete days on both sides: today is still being counted (nothing to compare on the 1st).
    let vs = change(s.month_to_date_complete, s.last_month_to_date)
        .map(|ch| format!("   {ch} to yesterday vs the same days of last month ({})", money(s.last_month_to_date, c)))
        .unwrap_or_default();
    println!("This month to {}   {}{vs}", date(s.read_at), paint(&money(s.month_to_date, c), "1"));
    println!("Last month               {}", money(s.last_month, c));
    if let Some(f) = s.forecast {
        println!("Forecast, this month     {}", money(f, c));
    }
    println!();
    let name_w = s.services.iter().map(|x| x.service.chars().count()).max().unwrap_or(10).clamp(10, 42);
    println!("{}", paint(&format!("{:<name_w$} {:>11} {:>11} {:>7}  {:<30}  in dcli", "service", "this month", "last month", "change", "last 30 days"), "90"));
    let now = now_secs();
    for x in s.services.iter().filter(|x| x.month_to_date.abs() >= 0.005 || x.last_month.abs() >= 0.005) {
        let values = columns(&x.daily, (now - 30.0 * DAY, now), 30, Agg::Sum);
        let marked = steps.iter().any(|st| st.service == x.service);
        println!(
            "{:<name_w$} {:>11} {:>11} {:>7}  {}  {}{}",
            ellipsize(&x.service, name_w),
            money(x.month_to_date, c),
            money(x.last_month, c),
            change(x.month_to_date_complete, x.last_month_to_date).unwrap_or_default(),
            paint(&format!("{:<30}", sparkline(&values)), "36"),
            services::module(&x.service).unwrap_or(""),
            if marked { paint(" ▲", "33") } else { String::new() }
        );
    }
    if !steps.is_empty() {
        println!();
        for st in steps {
            println!("{} {}", paint(&st.service, "33"), paint(&st.sentence(c), "33"));
        }
        println!("{}", paint("A correlation, not proof: traffic or other changes can explain a rise.", "90"));
    }
    for m in &s.missing {
        println!("{}", paint(&format!("not read: {m}"), "90"));
    }
    println!(
        "{}",
        paint(
            &format!(
                "Cost Explorer: {} requests this month. --usage SERVICE, --daily SERVICE, --commitments, --refresh",
                settings::requests_this_month()
            ),
            "90"
        )
    );
    Ok(ExitCode::SUCCESS)
}

/// The other environments of the config in `account`, as far as dcli knows the accounts.
fn shared_with(config: &Config, env: &str, account: &str) -> Vec<String> {
    let accounts = settings::load().accounts;
    config
        .environments
        .iter()
        .filter(|(name, e)| *name != env && accounts.get(&crate::aws::credentials::effective_profile(&e.profile)).map(String::as_str) == Some(account))
        .map(|(name, _)| name.clone())
        .collect()
}

fn usage(s: &Snapshot, svc: &ServiceCost, json: bool) -> Result<ExitCode> {
    let c = s.currency.as_str();
    let list: Vec<_> = s.usage.iter().filter(|u| u.service == svc.service).collect();
    if json {
        print_json(&json!({ "service": svc.service, "usageTypes": list.iter().map(|u| json!({
            "usageType": u.usage_type, "cost": u.cost, "quantity": u.quantity, "unit": u.unit, "rate": u.rate(),
        })).collect::<Vec<_>>() }));
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}  {}", paint(&svc.service, "1"), paint("usage types, last 30 days · amortized", "90"));
    if list.is_empty() {
        println!("{}", paint("No usage types read for it.", "90"));
    }
    let w = list.iter().map(|u| u.usage_type.chars().count()).max().unwrap_or(10).clamp(10, 48);
    for u in list {
        println!(
            "{:<w$} {:>11}  {:>9} {:<18} {}",
            ellipsize(&u.usage_type, w),
            money(u.cost, c),
            qty(u.quantity),
            ellipsize(&u.unit, 18),
            u.rate().map(|r| paint(&format!("{} per {}", rate(r, c), u.unit), "90")).unwrap_or_default()
        );
    }
    println!("{}", paint("The rate is what one unit cost on average: the account's own price, discounts and commitments included.", "90"));
    Ok(ExitCode::SUCCESS)
}

fn daily_view(s: &Snapshot, svc: &ServiceCost, deploys: &[Deploy], failed: &[String], json: bool) -> Result<ExitCode> {
    let c = s.currency.as_str();
    let on = |day: f64| deploys.iter().filter(move |d| explorer::day_start(d.at) == day);
    if json {
        print_json(&json!({ "service": svc.service, "days": svc.daily.iter().map(|(t, cost)| json!({
            "day": date(*t), "cost": cost, "deploys": on(*t).collect::<Vec<_>>(),
        })).collect::<Vec<_>>() }));
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}  {}", paint(&svc.service, "1"), paint("cost per day · amortized · ▲ the environment deployed", "90"));
    let max = svc.daily.iter().map(|(_, v)| *v).fold(0.0f64, f64::max);
    for (t, cost) in &svc.daily {
        let bar = if max > 0.0 { "█".repeat(((cost / max) * 30.0).round() as usize) } else { String::new() };
        let ds: Vec<String> = on(*t)
            .map(|d| match (&d.commit, &d.message) {
                (Some(commit), Some(m)) => format!("{} ({commit} \"{}\")", d.pipeline, ellipsize(m, 40)),
                (Some(commit), None) => format!("{} ({commit})", d.pipeline),
                _ => d.pipeline.clone(),
            })
            .collect();
        let mark = if ds.is_empty() { String::new() } else { paint(&format!(" ▲ {}", ds.join(", ")), "33") };
        println!("{}  {:>11}  {}{mark}", date(*t), money(*cost, c), paint(&format!("{bar:<30}"), "36"));
    }
    if !failed.is_empty() {
        println!("{}", paint(&format!("deploys not read: {}", failed.join("; ")), "90"));
    }
    Ok(ExitCode::SUCCESS)
}

fn commitments(s: &Snapshot, json: bool) -> Result<ExitCode> {
    let c = s.currency.as_str();
    if json {
        print_json(&json!({ "commitments": s.commitments, "coverage": s.coverage, "recommendation": s.recommendation, "missing": s.missing }));
        return Ok(ExitCode::SUCCESS);
    }
    let now = now_secs();
    println!("{}", paint("Savings Plans and reservations", "1"));
    if s.commitments.is_empty() {
        println!("{}", paint("None seen (read with the environment's own profile, plans bought in the management account may not show).", "90"));
    }
    for k in &s.commitments {
        let ends = k.end.map(|e| {
            let days = ((e - now) / DAY).ceil().max(0.0);
            let text = format!("ends {} ({days:.0} days)", date(e));
            if days < 60.0 { paint(&format!("{text} ⚠"), "33") } else { text }
        });
        let unused = k.unused_cost.filter(|u| *u > 0.0).map(|u| paint(&format!(" · unused in 30 days {}", money(u, c)), "33"));
        println!(
            "  {} {}  {} an hour · {} · used {}{} · {}",
            paint(&k.kind, "1"),
            paint(&k.id, "90"),
            rate(k.hourly, c),
            ends.unwrap_or_else(|| "no end date".into()),
            k.utilization.map_or_else(|| "—".into(), |u| format!("{:.0} %", u * 100.0)),
            unused.unwrap_or_default(),
            k.scope
        );
        if !k.services.is_empty() {
            println!("    {}", paint(&format!("covers {}", k.services.join(", ")), "90"));
        }
    }
    println!("\n{}", paint("Coverage, last 30 days", "1"));
    for cv in &s.coverage {
        let filled = (cv.covered * 20.0).round() as usize;
        println!(
            "  {:<40} {}{} {:>3.0} % by {} · {} outside them",
            ellipsize(&cv.service, 40),
            paint(&"█".repeat(filled), "32"),
            paint(&"░".repeat(20 - filled.min(20)), "90"),
            cv.covered * 100.0,
            cv.by,
            money(cv.on_demand_cost, c)
        );
    }
    println!("\n{}", paint("AWS recommends", "1"));
    match &s.recommendation {
        Some(r) => {
            println!(
                "  A {} of {} an hour ({}, {}), from the last {} days: AWS estimates {} saved a month.",
                r.kind,
                rate(r.hourly_commitment, c),
                r.term.to_lowercase().replace('_', " "),
                r.payment.to_lowercase().replace('_', " "),
                r.lookback_days,
                money(r.estimated_monthly_savings, c)
            );
            println!(
                "  {}",
                paint(
                    &format!(
                        "Apply the Savings findings for these services first ({} optimize): committing before optimizing pays for usage you were about to remove.",
                        program()
                    ),
                    "33"
                )
            );
        }
        None => println!("{}", paint("  Nothing: AWS has no recommendation for this usage, or has not made it yet.", "90")),
    }
    for m in &s.missing {
        println!("{}", paint(&format!("not read: {m}"), "90"));
    }
    Ok(ExitCode::SUCCESS)
}

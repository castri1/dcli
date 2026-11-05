//! `dcli ecs`: ECS. `dcli ecs [ENV]` lists clusters; `dcli ecs [ENV] CLUSTER` its services;
//! `dcli ecs [ENV] CLUSTER SERVICE` (or just SERVICE when only one cluster has it) a service's
//! deployments and events, `--tasks` why its tasks stopped, `--report` a Markdown report for an
//! AI agent. `--force-deploy`, `--desired N` and `--rollback [REVISION]` change the service
//! (asking first) and `--wait` follows the change until it completes.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::aws::Aws;
use crate::aws::ecs::{self, Change, Cluster, EcsService, Progress, Task};
use crate::cli::{aws_for, confirm_write, current_env, paint, print_json};
use crate::config::{Config, is_production};
use crate::util::{fmt_ago, fmt_utc, now_secs, program, sanitize};

/// How long `--wait` follows a change.
const WAIT: Duration = Duration::from_secs(30 * 60);
/// How often `--wait` checks.
const EVERY: Duration = Duration::from_secs(5);

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one); then the cluster and the service
    #[arg(value_name = "ENV")]
    first: Option<String>,
    /// Cluster (or the service, when only one cluster has it)
    #[arg(value_name = "CLUSTER")]
    second: Option<String>,
    /// Service
    #[arg(value_name = "SERVICE")]
    third: Option<String>,
    #[arg(long)]
    json: bool,
    /// The service's running tasks and those stopped in the last hour, with why they stopped
    #[arg(long, conflicts_with_all = ["report", "force_deploy", "desired", "rollback"])]
    tasks: bool,
    /// Markdown report of the service for an AI agent: deployments, events, stopped tasks and
    /// their containers' last log lines
    #[arg(long, conflicts_with_all = ["force_deploy", "desired", "rollback"])]
    report: bool,
    /// Start a new deployment of the same task definition (new tasks pull the image again)
    #[arg(long, conflicts_with_all = ["desired", "rollback"])]
    force_deploy: bool,
    /// Set how many tasks the service runs
    #[arg(long, value_name = "N", conflicts_with = "rollback")]
    desired: Option<i64>,
    /// Deploy another task definition revision: the one before the current by default, or
    /// REVISION (a number, family:number or an ARN)
    #[arg(long, value_name = "REVISION", num_args = 0..=1, default_missing_value = "previous")]
    rollback: Option<String>,
    /// After a change, wait until the deployment completes (or the tasks run)
    #[arg(long)]
    wait: bool,
    /// With a change: do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
}

impl Args {
    fn change_given(&self) -> bool {
        self.force_deploy || self.desired.is_some() || self.rollback.is_some()
    }
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    if !args.change_given() && (args.wait || args.yes) {
        bail!("--wait and --yes go with --force-deploy, --desired or --rollback");
    }
    if let Some(n) = args.desired
        && n < 0
    {
        bail!("--desired takes 0 or more");
    }
    let mut parts: Vec<String> = [args.first.clone(), args.second.clone(), args.third.clone()].into_iter().flatten().collect();
    let env = if parts.first().is_some_and(|p| config.environments.contains_key(p)) { parts.remove(0) } else { current_env(config)? };
    if parts.len() > 2 {
        bail!("unexpected {}: {} ecs [ENV] [CLUSTER] [SERVICE]", parts[2], program());
    }
    let aws = aws_for(config, &env)?;
    let clusters = ecs::clusters(&aws)?;
    let (cluster, service) = match parts.as_slice() {
        [] => {
            if args.change_given() || args.tasks || args.report {
                bail!("name the service: {} ecs [ENV] [CLUSTER] SERVICE …", program());
            }
            return list_clusters(&clusters, args.json);
        }
        [one] if clusters.iter().any(|c| c.cluster_name == *one) => (one.clone(), None),
        [one] => (find_service(&aws, &clusters, one, &env)?, Some(one.clone())),
        [c, s] => {
            if !clusters.iter().any(|x| x.cluster_name == *c) {
                bail!("no ECS cluster {c} in {env}. Clusters: {}", names(&clusters));
            }
            (c.clone(), Some(s.clone()))
        }
        _ => unreachable!(),
    };
    let Some(name) = service else {
        if args.change_given() || args.tasks || args.report {
            bail!("name the service: {} ecs [ENV] {cluster} SERVICE …", program());
        }
        return list_services(&aws, &cluster, args.json);
    };
    let s = ecs::service(&aws, &cluster, &name)?.ok_or_else(|| anyhow!("no service {name} in cluster {cluster} ({env})"))?;
    if args.report {
        print!("{}", ecs::gather_report(&aws, &env, &aws.region, &cluster, &name, now_secs())?);
        return Ok(ExitCode::SUCCESS);
    }
    if args.tasks {
        return show_tasks(&aws, &s, args.json);
    }
    if args.change_given() {
        return change(&aws, &env, &s, &args);
    }
    show_service(&aws, &s, args.json)
}

fn names(clusters: &[Cluster]) -> String {
    if clusters.is_empty() { "none".into() } else { clusters.iter().map(|c| c.cluster_name.as_str()).collect::<Vec<_>>().join(", ") }
}

/// The cluster with service `name`, when exactly one has it.
fn find_service(aws: &Aws, clusters: &[Cluster], name: &str, env: &str) -> Result<String> {
    let mut found = Vec::new();
    for c in clusters {
        if ecs::service_names(aws, &c.cluster_name)?.iter().any(|s| s == name) {
            found.push(c.cluster_name.clone());
        }
    }
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("no ECS cluster or service {name} in {env}. Clusters: {}", names(clusters)),
        many => bail!("{name} is a service of several clusters ({}): name the cluster", many.join(", ")),
    }
}

// ---- reading ------------------------------------------------------------------------------

fn list_clusters(clusters: &[Cluster], json: bool) -> Result<ExitCode> {
    if json {
        print_json(&serde_json::to_value(clusters)?);
        return Ok(ExitCode::SUCCESS);
    }
    if clusters.is_empty() {
        println!("No ECS clusters in this region.");
        return Ok(ExitCode::SUCCESS);
    }
    let w = clusters.iter().map(|c| c.cluster_name.chars().count()).max().unwrap_or(4).clamp(4, 48) + 2;
    for c in clusters {
        let mut extra = String::new();
        if c.registered_container_instances_count > 0 {
            extra.push_str(&format!(" · {} EC2 instances", c.registered_container_instances_count));
        }
        if !c.capacity_providers.is_empty() {
            extra.push_str(&format!(" · {}", c.capacity_providers.join(", ")));
        }
        println!(
            "{}{:>3} services  {:>4} tasks{}",
            paint(&format!("{:<w$}", c.cluster_name), "1"),
            c.active_services_count,
            c.running_tasks_count,
            paint(&extra, "90")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn state_code(state: ecs::State) -> &'static str {
    match state {
        ecs::State::Failed => "31",
        ecs::State::Degraded => "33",
        ecs::State::Deploying => "36",
        ecs::State::Steady => "32",
        ecs::State::Idle => "90",
    }
}

fn service_json(s: &EcsService, now: f64) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or_default();
    v["state"] = json!(s.state(now).label());
    v
}

fn list_services(aws: &Aws, cluster: &str, json: bool) -> Result<ExitCode> {
    let services = ecs::services(aws, cluster)?;
    let now = now_secs();
    if json {
        print_json(&Value::Array(services.iter().map(|s| service_json(s, now)).collect()));
        return Ok(ExitCode::SUCCESS);
    }
    if services.is_empty() {
        println!("No services in {cluster}.");
        return Ok(ExitCode::SUCCESS);
    }
    let w = services.iter().map(|s| s.service_name.chars().count()).max().unwrap_or(4).clamp(4, 48) + 2;
    println!("{}", paint(&format!("  {:<w$}{:<9}{:<16}{:<26}STATE", "SERVICE", "TASKS", "LAUNCH", "TASK DEFINITION"), "90"));
    for s in &services {
        let state = s.state(now);
        println!(
            "{} {:<w$}{:<9}{:<16}{:<26}{}",
            paint(state.symbol(), state_code(state)),
            s.service_name,
            format!("{}/{}", s.running_count, s.desired_count),
            crate::util::ellipsize(&s.launch(), 15),
            crate::util::ellipsize(ecs::revision_of(&s.task_definition), 25),
            paint(state.label(), state_code(state))
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn show_service(aws: &Aws, s: &EcsService, json: bool) -> Result<ExitCode> {
    let now = now_secs();
    if json {
        print_json(&service_json(s, now));
        return Ok(ExitCode::SUCCESS);
    }
    let state = s.state(now);
    println!("{}  {}", paint(&s.service_name, "1"), paint(&format!("{} {}", state.symbol(), state.label()), state_code(state)));
    let row = |k: &str, v: String| println!("  {:<16} {v}", paint(k, "90"));
    row("Cluster", s.cluster_name().to_string());
    row("Tasks", format!("{} running of {} desired · {} pending", s.running_count, s.desired_count, s.pending_count));
    let image = ecs::task_definition(aws, &s.task_definition).map(|d| format!(" · {}", d.main_image())).unwrap_or_default();
    row("Task definition", format!("{}{image}", ecs::revision_of(&s.task_definition)));
    row("Launch", s.launch());
    row("Deploys with", s.controller().to_string());
    let breaker = match s.circuit_breaker() {
        Some(c) if c.rollback => "on, rolls back",
        Some(_) => "on, no rollback",
        None => "off",
    };
    row("Circuit breaker", breaker.to_string());
    println!();
    println!("{}", paint("Deployments", "1"));
    if s.deployments.is_empty() {
        println!("  none listed ({} deploys it)", s.controller());
    }
    for d in &s.deployments {
        let rollout = if d.rollout().is_empty() { "—" } else { d.rollout() };
        let code = match rollout {
            "FAILED" => "31",
            "IN_PROGRESS" => "36",
            "COMPLETED" => "32",
            _ => "90",
        };
        let mut line = format!(
            "  {:<8}{}  {:<22}{}/{} running",
            d.status.to_lowercase(),
            paint(&format!("{rollout:<12}"), code),
            ecs::revision_of(&d.task_definition),
            d.running_count,
            d.desired_count
        );
        if d.failed_tasks > 0 {
            line.push_str(&paint(&format!(" · {} failed", d.failed_tasks), "31"));
        }
        if let Some(t) = d.created_at {
            line.push_str(&paint(&format!(" · {}", fmt_ago(t.min(now))), "90"));
        }
        println!("{line}");
        if rollout == "FAILED"
            && let Some(r) = &d.rollout_state_reason
        {
            println!("    {}", paint(&sanitize(r), "31"));
        }
    }
    println!();
    println!("{}", paint("Events", "1"));
    for e in s.events.iter().take(10) {
        let when = e.created_at.map(|t| fmt_ago(t.min(now))).unwrap_or_default();
        println!("  {}  {}", paint(&format!("{when:>8}"), "90"), sanitize(&e.message));
    }
    if s.events.is_empty() {
        println!("  none");
    }
    Ok(ExitCode::SUCCESS)
}

fn task_json(t: &Task) -> Value {
    let mut v = serde_json::to_value(t).unwrap_or_default();
    v["id"] = json!(t.id());
    if t.stopped() {
        v["why"] = json!(t.why());
        v["likelyCause"] = json!(t.hint());
    }
    v
}

fn show_tasks(aws: &Aws, s: &EcsService, json: bool) -> Result<ExitCode> {
    let tasks = ecs::tasks(aws, s.cluster_name(), &s.service_name)?;
    if json {
        print_json(&Value::Array(tasks.iter().map(task_json).collect()));
        return Ok(ExitCode::SUCCESS);
    }
    let now = now_secs();
    let stopped = tasks.iter().filter(|t| t.stopped()).count();
    println!(
        "{}  {}",
        paint(&s.service_name, "1"),
        paint(&format!("{} running · {stopped} stopped in the last hour", tasks.len() - stopped), "90")
    );
    for t in &tasks {
        let when = t.when().map(|w| fmt_ago(w.min(now))).unwrap_or_default();
        let (symbol, code) = if !t.stopped() { ("●", "32") } else if t.failed_containers().any(|c| c.exit_code.is_some_and(|e| e != 0 && e != 143)) { ("✗", "31") } else { ("○", "90") };
        let status = if t.stopped() { "stopped".to_string() } else { t.last_status.to_lowercase() };
        println!(
            "{} {}  {:<22}{:<10}{}",
            paint(symbol, code),
            &t.id()[..t.id().len().min(12)],
            ecs::revision_of(&t.task_definition_arn),
            status,
            paint(&when, "90")
        );
        if t.stopped() {
            println!("    {}", paint(&t.why(), "33"));
            if let Some(h) = t.hint() {
                println!("    {}", paint(&format!("→ {h}"), "36"));
            }
            if let Some(at) = t.stopped_at {
                println!("    {}", paint(&format!("stopped {} · {}", fmt_utc(at), t.stop_code.as_deref().unwrap_or("—")), "90"));
            }
        }
    }
    if tasks.is_empty() {
        println!("No tasks running or stopped in the last hour.");
    }
    Ok(ExitCode::SUCCESS)
}

// ---- writing --------------------------------------------------------------------------------

fn say(json: bool, line: &str) {
    // With --json, stdout carries only the JSON.
    if json {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

/// The task definition `--rollback` names: `previous` (the newest active revision older than
/// the current one), a revision number of the service's family, `family:number` or an ARN.
fn rollback_target(aws: &Aws, s: &EcsService, wanted: &str) -> Result<String> {
    let current = ecs::revision_of(&s.task_definition).to_string();
    let (family, _) = current.rsplit_once(':').ok_or_else(|| anyhow!("{} has no task definition", s.service_name))?;
    let current_rev = ecs::revision_number(&s.task_definition).unwrap_or(i64::MAX);
    if wanted == "previous" {
        let defs = ecs::revisions(aws, family, 50)?;
        return defs
            .iter()
            .find(|d| d.revision < current_rev)
            .map(|d| d.task_definition_arn.clone())
            .ok_or_else(|| anyhow!("{family} has no active revision before {current}"));
    }
    let named = if let Ok(n) = wanted.parse::<i64>() { format!("{family}:{n}") } else { wanted.to_string() };
    let def = ecs::task_definition(aws, &named).map_err(|e| anyhow!("no task definition {named}: {}", e.short()))?;
    if def.family != family {
        bail!("{} is of family {}, not {family} (the service's)", ecs::revision_of(&def.task_definition_arn), def.family);
    }
    Ok(def.task_definition_arn)
}

fn change(aws: &Aws, env: &str, s: &EcsService, args: &Args) -> Result<ExitCode> {
    let cluster = s.cluster_name().to_string();
    let change = if args.force_deploy {
        Change::ForceDeploy
    } else if let Some(n) = args.desired {
        if n == s.desired_count {
            say(args.json, &format!("{} already wants {n} tasks; nothing to change.", s.service_name));
            return Ok(ExitCode::SUCCESS);
        }
        Change::Desired(n)
    } else {
        if let Some(why) = s.redeploy_refusal() {
            bail!("{why}");
        }
        let target = rollback_target(aws, s, args.rollback.as_deref().unwrap_or("previous"))?;
        if target == s.task_definition {
            say(args.json, &format!("{} runs {} already; --force-deploy starts a new deployment of it.", s.service_name, ecs::revision_of(&target)));
            return Ok(ExitCode::SUCCESS);
        }
        Change::TaskDefinition(target)
    };
    if change.deploys()
        && let Some(why) = s.redeploy_refusal()
    {
        bail!("{why}");
    }
    let place = format!("in {} · {} · {env}", s.service_name, cluster);
    let what = match &change {
        Change::ForceDeploy => format!("New deployment of {} ({})", s.service_name, ecs::revision_of(&s.task_definition)),
        Change::Desired(n) => format!("Desired tasks of {}: {} → {n}", s.service_name, s.desired_count),
        Change::TaskDefinition(td) => format!("Deploy {} on {} (now {})", ecs::revision_of(td), s.service_name, ecs::revision_of(&s.task_definition)),
    };
    say(args.json, &paint(&what, "1"));
    say(args.json, &paint(&place, if is_production(env) { "31" } else { "90" }));
    if change.deploys() {
        let breaker = match s.circuit_breaker() {
            Some(c) if c.rollback => "the circuit breaker rolls it back if its tasks keep failing",
            Some(_) => "the circuit breaker stops it if its tasks keep failing",
            None => "the circuit breaker is off: a failing deployment keeps trying",
        };
        say(args.json, &paint(&format!("{} running tasks are replaced; {breaker}.", s.running_count), "90"));
    }
    let question = match &change {
        Change::ForceDeploy => "Start the deployment?".to_string(),
        Change::Desired(n) => format!("Set {n} desired tasks?"),
        Change::TaskDefinition(td) => format!("Deploy {}?", ecs::revision_of(td)),
    };
    if !confirm_write(env, &question, args.yes)? {
        println!("Cancelled.");
        return Ok(ExitCode::FAILURE);
    }
    let updated = ecs::update_service(aws, &cluster, &s.service_name, &change).map_err(|e| {
        let crate::aws::AwsError::Api { message, .. } = &e else { return anyhow!(e) };
        anyhow!("ECS refused it: {}", sanitize(message))
    })?;
    let deployment = change.deploys().then(|| updated.primary().map(|d| d.id.clone())).flatten();
    let desired = match change {
        Change::Desired(n) => Some(n),
        _ => None,
    };
    let mut result = if change.deploys() { "started" } else { "changed" };
    let mut message = match &deployment {
        Some(id) => format!("Deployment {id} started"),
        None => format!("Desired count set to {}", desired.unwrap_or(updated.desired_count)),
    };
    let mut ok = true;
    if args.wait {
        let start = Instant::now();
        loop {
            std::thread::sleep(EVERY);
            let Some(now) = ecs::service(aws, &cluster, &s.service_name)? else { bail!("{} is gone", s.service_name) };
            match ecs::progress(&now, deployment.as_deref(), desired) {
                Progress::Done(text) => {
                    result = "completed";
                    message = text;
                    break;
                }
                Progress::Failed(text) => {
                    result = "failed";
                    message = text;
                    ok = false;
                    break;
                }
                Progress::Waiting(text) => {
                    eprintln!("{}", paint(&format!("  {text}…"), "90"));
                    if start.elapsed() > WAIT {
                        result = "waiting";
                        message = format!("still not done after {} minutes; stopped waiting", WAIT.as_secs() / 60);
                        break;
                    }
                }
            }
        }
    }
    if args.json {
        print_json(&json!({
            "action": match change { Change::ForceDeploy => "force-deploy", Change::Desired(_) => "desired", Change::TaskDefinition(_) => "rollback" },
            "cluster": cluster, "service": s.service_name, "change": change.describe(),
            "deployment": deployment, "result": result, "message": message,
        }));
    } else {
        let code = if !ok { "31" } else if result == "completed" || result == "changed" { "32" } else { "36" };
        println!("{}", paint(&message, code));
        if !args.wait && change.deploys() {
            println!("{}", paint(&format!("Follow it: {} ecs {env} {cluster} {} (or add --wait)", program(), s.service_name), "90"));
        }
        if !ok {
            println!("{}", paint(&format!("Why: {} ecs {env} {cluster} {} --report", program(), s.service_name), "90"));
        }
    }
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn parse(line: &str) -> std::result::Result<Args, clap::Error> {
        Cli::try_parse_from(std::iter::once("ecs").chain(line.split_whitespace())).map(|c| c.args)
    }

    #[test]
    fn parses_reads_and_changes() {
        let a = parse("prod acme-apps orders-api --tasks --json").unwrap();
        assert!(a.tasks && a.json && !a.change_given());
        let a = parse("prod orders-api --rollback").unwrap();
        assert_eq!(a.rollback.as_deref(), Some("previous"));
        let a = parse("prod orders-api --rollback 41 --wait -y").unwrap();
        assert_eq!(a.rollback.as_deref(), Some("41"));
        assert!(a.wait && a.yes);
        assert!(parse("prod orders-api --force-deploy --desired 2").is_err());
        assert!(parse("prod orders-api --tasks --force-deploy").is_err());
        assert_eq!(parse("orders-api --desired 0").unwrap().desired, Some(0));
    }
}

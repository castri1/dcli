//! `dcli eks`: EKS clusters. `dcli eks [ENV]` lists them with their Kubernetes version and
//! support; `dcli eks [ENV] CLUSTER` shows one (health, node groups, add-ons, upgrade insights,
//! access). `--pods` reads inside the cluster what is not well (deployments, pods), `--report`
//! writes Markdown for an AI agent, `--kubeconfig` writes a kubeconfig whose tokens come from
//! dcli (for kubectl and k9s), and `--token` prints one: what that kubeconfig runs.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::aws::eks::{self, Cluster, Detail, Support};
use crate::aws::{Aws, k8s, sts};
use crate::cli::{aws_for, env_and, paint, print_json};
use crate::config::{Loaded, Source};
use crate::util::{ellipsize, fmt_ago, now_secs, program, sanitize};

/// Failing pods the report reads events and logs of, and the log lines it keeps.
const REPORT_PODS: usize = 5;
const REPORT_LINES: usize = 40;

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Cluster name; without it, lists the clusters
    cluster: Option<String>,
    /// The deployments and pods that are not well, read inside the cluster
    #[arg(long, group = "view")]
    pods: bool,
    /// Markdown report of what needs attention, for an AI agent (reads inside the cluster too)
    #[arg(long, group = "view")]
    report: bool,
    /// Write a kubeconfig for the cluster (in ~/.deployment-cli/kube, never ~/.kube/config)
    /// whose tokens come from dcli, for kubectl and k9s
    #[arg(long, group = "view")]
    kubeconfig: bool,
    /// Print an ExecCredential with a token for the cluster: what that kubeconfig runs
    #[arg(long, group = "view")]
    token: bool,
    #[arg(long)]
    json: bool,
}

fn config_file(loaded: &Loaded) -> Option<&std::path::Path> {
    match &loaded.source {
        Source::File(p) => Some(p.as_path()),
        Source::AwsProfiles => None,
    }
}

pub fn run(loaded: &Loaded, args: Args) -> Result<ExitCode> {
    let config = &loaded.config;
    let (env_name, cluster) = env_and(config, args.env.clone(), args.cluster.clone())?;
    if args.token {
        // Run by kubectl and k9s: never asks anything, prints only the credential.
        let Some(cluster) = cluster else { bail!("which cluster? {} eks ENV CLUSTER --token", program()) };
        let env = config.env(&env_name)?;
        let aws = Aws::new(&env.profile, &env.region);
        let token = eks::token(&aws, &cluster).map_err(|e| {
            if e.is_login_required() {
                anyhow!("{} — sign in with: {} login {env_name}", e.short().replace(" — press L to sign in", ""), program())
            } else {
                anyhow!(e)
            }
        })?;
        println!("{}", eks::exec_credential(&token, now_secs()));
        return Ok(ExitCode::SUCCESS);
    }
    let aws = aws_for(config, &env_name)?;
    let Some(name) = cluster else {
        if args.pods || args.report || args.kubeconfig {
            bail!("which cluster? {} eks [ENV] CLUSTER …", program());
        }
        return list(&aws, args.json);
    };
    let versions = eks::cluster_versions(&aws).unwrap_or_default();
    let detail = eks::detail(&aws, &name, &versions).map_err(|e| match e {
        crate::aws::AwsError::Api { status: 404, .. } => {
            let known = eks::list_clusters(&aws).unwrap_or_default().join(", ");
            anyhow!("no EKS cluster named '{name}' in {env_name} ({}). Clusters: {known}", aws.region)
        }
        e => anyhow!(e),
    })?;
    if args.kubeconfig {
        let path = eks::write_kubeconfig(&detail.cluster, &env_name, config_file(loaded)).map_err(|e| anyhow!(e))?;
        if args.json {
            print_json(&json!({ "kubeconfig": path, "context": format!("{env_name}-{name}") }));
        } else {
            let shown = crate::util::display_path(&path);
            eprintln!("Wrote {shown} (tokens come from {}; ~/.kube/config is untouched). Use it with:", program());
            if cfg!(windows) {
                println!("$env:KUBECONFIG = \"{}\"", path.display());
            } else {
                println!("export KUBECONFIG={}", shell_quote(&path.to_string_lossy()));
            }
            eprintln!("then: kubectl get pods -A   or   k9s");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if args.pods || args.report {
        let inside = inside(aws.clone(), &detail.cluster, if args.report { REPORT_PODS } else { 0 });
        if args.report {
            let md = eks::report(&env_name, &aws.region, &detail, Some(inside.as_ref().map_err(String::clone)), now_secs());
            print!("{}", sanitize(&md));
            return Ok(ExitCode::SUCCESS);
        }
        return match inside {
            Ok(f) if args.json => {
                print_json(&workloads_json(&f.workloads));
                Ok(ExitCode::SUCCESS)
            }
            Ok(f) => {
                print_workloads(&f.workloads);
                Ok(ExitCode::SUCCESS)
            }
            Err(why) => {
                eprintln!("{}", paint(&format!("✗ {why}"), "31"));
                Ok(ExitCode::FAILURE)
            }
        };
    }
    if args.json {
        print_json(&detail_json(&detail));
    } else {
        print_detail(&detail);
    }
    Ok(ExitCode::SUCCESS)
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-~".contains(c)) { s.to_string() } else { format!("'{}'", s.replace('\'', "'\\''")) }
}

/// Inside the cluster: workloads and (for `pods` failing pods) their events and logs, or why
/// they could not be read.
fn inside(aws: Arc<Aws>, cluster: &Cluster, pods: usize) -> std::result::Result<k8s::Findings, String> {
    let explain = |e: crate::aws::AwsError| {
        let principal = sts::caller_principal(&aws).ok();
        k8s::explain(&e, cluster, principal.as_deref())
    };
    let client = k8s::Client::new(aws.clone(), cluster).map_err(explain)?;
    k8s::findings(&client, pods, REPORT_LINES).map_err(explain)
}

fn list(aws: &Aws, json: bool) -> Result<ExitCode> {
    let o = eks::overview(aws)?;
    let now = now_secs();
    if json {
        let rows: Vec<Value> = o
            .clusters
            .iter()
            .map(|c| {
                let s = Support::of(&c.version, &o.versions, now);
                json!({
                    "name": c.name, "status": c.status, "version": c.version, "support": s.short(now),
                    "extendedSupport": s.extended(), "endpoint": c.endpoint_access(), "authenticationMode": c.auth_mode(),
                    "autoMode": c.auto_mode(), "issues": c.health.issues.iter().map(eks::Issue::text).collect::<Vec<_>>(),
                })
            })
            .collect();
        print_json(&json!(rows));
        return Ok(ExitCode::SUCCESS);
    }
    if o.clusters.is_empty() {
        println!("No EKS clusters in {}.", aws.region);
        return Ok(ExitCode::SUCCESS);
    }
    let width = o.clusters.iter().map(|c| c.name.chars().count()).max().unwrap_or(4).clamp(4, 40);
    println!("{}", paint(&format!("  {:<width$}  {:<7} {:<9} {:<34} ENDPOINT", "NAME", "K8S", "STATUS", "SUPPORT"), "1"));
    for c in &o.clusters {
        let s = Support::of(&c.version, &o.versions, now);
        let (mark, code) = if c.status != "ACTIVE" || !c.health.issues.is_empty() {
            ("✗", "31")
        } else if s.attention(now) {
            ("!", "33")
        } else {
            ("✓", "32")
        };
        let support = format!("{:<34}", s.short(now));
        let support = if s.attention(now) { paint(&support, "33") } else { support };
        let endpoint = if c.resources_vpc_config.endpoint_public_access { "public" } else { "private" };
        println!("{} {:<width$}  {:<7} {:<9} {support} {endpoint}", paint(mark, code), ellipsize(&c.name, width), c.version, c.status);
    }
    Ok(ExitCode::SUCCESS)
}

fn section(title: &str) {
    println!("\n{}", paint(title, "1"));
}

fn print_detail(d: &Detail) {
    let c = &d.cluster;
    let now = now_secs();
    println!("{} · {} · Kubernetes {} ({})", paint(&c.name, "1"), c.status, c.version, c.platform_version);
    println!("  Support:   {}", d.support.describe(c, now));
    println!("  Endpoint:  {} · authentication {}", c.endpoint_access(), c.auth_mode());
    let logs = c.logs_enabled();
    println!("  Logs:      {}", if logs.is_empty() { "control plane logs off".to_string() } else { format!("{} → {}", logs.join(", "), c.log_group()) });
    if c.auto_mode() {
        println!("  Auto Mode: on ({})", c.compute_config.as_ref().map(|x| x.node_pools.join(", ")).unwrap_or_default());
    }
    if let Some(t) = c.created_at {
        println!("  Created:   {}", fmt_ago(t));
    }
    let problems = d.problems(now);
    if !problems.is_empty() {
        section("Needs attention");
        for p in &problems {
            println!("  {} {p}", paint("!", "33"));
        }
    }
    section("Node groups");
    match &d.nodegroups {
        Ok(list) if list.is_empty() => println!("  none{}", if c.auto_mode() { " (Auto Mode manages the nodes)" } else { "" }),
        Ok(list) => {
            for n in list {
                let s = &n.scaling_config;
                let mark = if n.ok() { paint("✓", "32") } else { paint("✗", "31") };
                println!(
                    "  {mark} {:<18} {:<9} {} nodes ({}–{})  {}  {}  k8s {}  {}",
                    n.nodegroup_name,
                    n.status,
                    s.desired_size,
                    s.min_size,
                    s.max_size,
                    n.instance_types.join(","),
                    n.capacity_type.as_deref().unwrap_or("ON_DEMAND"),
                    n.version.as_deref().unwrap_or("?"),
                    n.release_version.as_deref().unwrap_or(""),
                );
                for i in &n.health.issues {
                    println!("      {}", paint(&i.text(), "31"));
                }
            }
        }
        Err(e) => println!("  {}", paint(&e.short(), "31")),
    }
    section("Add-ons");
    match &d.addons {
        Ok(list) if list.is_empty() => println!("  none"),
        Ok(list) => {
            for a in list {
                let mark = if a.ok() { paint("✓", "32") } else { paint("✗", "31") };
                let newer = a.newer.as_deref().map(|v| paint(&format!("  → {v} available"), "33")).unwrap_or_default();
                println!("  {mark} {:<22} {:<22} {}{newer}", a.addon_name, a.addon_version, a.status);
                for i in &a.health.issues {
                    println!("      {}", paint(&i.text(), "31"));
                }
            }
        }
        Err(e) => println!("  {}", paint(&e.short(), "31")),
    }
    if let Ok(list) = &d.fargate
        && !list.is_empty()
    {
        section("Fargate profiles");
        for f in list {
            println!("  {:<20} {:<8} {}", f.fargate_profile_name, f.status, f.selects());
        }
    }
    section("Upgrade insights");
    match &d.insights {
        Ok(list) if list.is_empty() => println!("  none"),
        Ok(list) => {
            for i in list {
                let (mark, code) = match i.insight_status.status.as_str() {
                    "PASSING" => ("✓", "32"),
                    "ERROR" => ("✗", "31"),
                    _ => ("!", "33"),
                };
                println!("  {} {:<8} {}", paint(mark, code), i.insight_status.status, i.name);
                for dep in i.category_specific_summary.iter().flat_map(|s| &s.deprecation_details) {
                    let to = dep.replaced_with.as_deref().map(|r| format!(" → {r}")).unwrap_or_default();
                    println!("      {}{to}", dep.usage);
                    for s in &dep.client_stats {
                        println!("        called by {} ({} requests in 30 days)", s.user_agent, s.number_of_requests_last30_days);
                    }
                }
            }
        }
        Err(e) => println!("  {}", paint(&e.short(), "31")),
    }
    section("Access");
    match &d.access {
        Ok(None) => println!("  aws-auth ConfigMap only (authentication mode CONFIG_MAP)"),
        Ok(Some(list)) if list.is_empty() => println!("  no access entries"),
        Ok(Some(list)) => {
            for e in list {
                let policies: Vec<String> = e.policies.iter().map(eks::AccessPolicy::label).collect();
                let what = if policies.is_empty() { e.kubernetes_groups.join(", ") } else { policies.join(", ") };
                println!("  {}  {}  {what}", e.principal_arn, e.kind);
            }
        }
        Err(e) => println!("  {}", paint(&e.short(), "31")),
    }
    println!("\n{}", paint(&format!("Inside the cluster: {} eks {} --pods · kubectl/k9s: --kubeconfig", program(), c.name), "90"));
}

fn detail_json(d: &Detail) -> Value {
    let c = &d.cluster;
    let now = now_secs();
    let err = |e: &crate::aws::AwsError| json!({ "error": e.short() });
    json!({
        "name": c.name,
        "arn": c.arn,
        "status": c.status,
        "version": c.version,
        "platformVersion": c.platform_version,
        "support": d.support.describe(c, now),
        "extendedSupport": d.support.extended(),
        "supportType": c.support_type(),
        "endpoint": c.endpoint_access(),
        "authenticationMode": c.auth_mode(),
        "autoMode": c.auto_mode(),
        "logs": c.logs_enabled(),
        "problems": d.problems(now),
        "nodegroups": match &d.nodegroups {
            Ok(l) => json!(l.iter().map(|n| json!({
                "name": n.nodegroup_name, "status": n.status, "version": n.version, "releaseVersion": n.release_version,
                "instanceTypes": n.instance_types, "capacityType": n.capacity_type, "desired": n.scaling_config.desired_size,
                "min": n.scaling_config.min_size, "max": n.scaling_config.max_size,
                "issues": n.health.issues.iter().map(eks::Issue::text).collect::<Vec<_>>(),
            })).collect::<Vec<_>>()),
            Err(e) => err(e),
        },
        "addons": match &d.addons {
            Ok(l) => json!(l.iter().map(|a| json!({
                "name": a.addon_name, "version": a.addon_version, "status": a.status, "newer": a.newer,
                "issues": a.health.issues.iter().map(eks::Issue::text).collect::<Vec<_>>(),
            })).collect::<Vec<_>>()),
            Err(e) => err(e),
        },
        "fargateProfiles": match &d.fargate {
            Ok(l) => json!(l.iter().map(|f| json!({ "name": f.fargate_profile_name, "status": f.status, "selects": f.selects() })).collect::<Vec<_>>()),
            Err(e) => err(e),
        },
        "insights": match &d.insights {
            Ok(l) => json!(l.iter().map(|i| json!({
                "name": i.name, "status": i.insight_status.status, "reason": i.insight_status.reason,
                "deprecations": i.category_specific_summary.iter().flat_map(|s| &s.deprecation_details).map(|x| json!({
                    "usage": x.usage, "replacedWith": x.replaced_with, "removedIn": x.stop_serving_version,
                    "clients": x.client_stats.iter().map(|c| json!({ "userAgent": c.user_agent, "requests30Days": c.number_of_requests_last30_days })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>()),
            Err(e) => err(e),
        },
        "access": match &d.access {
            Ok(None) => json!("aws-auth ConfigMap only"),
            Ok(Some(l)) => json!(l.iter().map(|e| json!({
                "principal": e.principal_arn, "type": e.kind, "groups": e.kubernetes_groups,
                "policies": e.policies.iter().map(eks::AccessPolicy::label).collect::<Vec<_>>(),
            })).collect::<Vec<_>>()),
            Err(e) => err(e),
        },
    })
}

fn print_workloads(w: &k8s::Workloads) {
    let unhealthy = w.unhealthy();
    println!(
        "{} namespaces · {} deployments ({} not healthy) · {} pods ({} with a problem){}",
        w.namespaces,
        w.deployments.len(),
        unhealthy.len(),
        w.pods,
        w.failing.len(),
        if w.truncated { " · more than dcli lists" } else { "" }
    );
    if !unhealthy.is_empty() {
        section("Deployments not healthy");
        for d in &unhealthy {
            println!("  {} {:<32} {}/{}  {}", paint("✗", "31"), format!("{}/{}", d.namespace, d.name), d.ready, d.desired, d.problem().unwrap_or_default());
        }
    }
    if !w.failing.is_empty() {
        section("Pods with a problem");
        for (p, why) in &w.failing {
            println!(
                "  {} {:<44} {:<26} {:>3} restarts  {}",
                paint("✗", "31"),
                format!("{}/{}", p.namespace, p.name),
                why.reason,
                p.restarts(),
                ellipsize(&why.detail, 120)
            );
        }
    }
    if unhealthy.is_empty() && w.failing.is_empty() {
        println!("{}", paint("✓ Every deployment is ready and no pod has a problem", "32"));
    }
}

fn workloads_json(w: &k8s::Workloads) -> Value {
    json!({
        "namespaces": w.namespaces,
        "pods": w.pods,
        "truncated": w.truncated,
        "deployments": w.deployments.iter().map(|d| json!({
            "namespace": d.namespace, "name": d.name, "desired": d.desired, "ready": d.ready, "problem": d.problem(),
        })).collect::<Vec<_>>(),
        "failingPods": w.failing.iter().map(|(p, why)| json!({
            "namespace": p.namespace, "name": p.name, "phase": p.phase, "reason": why.reason, "detail": why.detail,
            "restarts": p.restarts(), "container": why.container, "node": p.node,
        })).collect::<Vec<_>>(),
    })
}

//! `dcli apigw`: API Gateway APIs, their routes and stages, metrics, testing a route,
//! exporting OpenAPI, moving stages between deployments, and a report for an AI agent.

use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::aws::Aws;
use crate::aws::apigateway::{self as apigw, Api, ApiDetail, Kind, Route, Stage, TestRequest};
use crate::aws::lambda::Range;
use crate::cli::{aws_for, confirm_write, env_and, paint, print_json, read_value};
use crate::config::{Config, is_production};
use crate::tui::chart::{Agg, columns, fmt_count, fmt_ms, sparkline};
use crate::util::{fmt_ago, now_secs, program};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// API name or id; without it, lists the APIs
    api: Option<String>,
    /// The routes: method, path, what they call, how they are protected
    #[arg(long, group = "view")]
    routes: bool,
    /// The stages: which deployment each runs, and their settings
    #[arg(long, group = "view")]
    stages: bool,
    /// Custom domain names and the API stages they map to
    #[arg(long, group = "view")]
    domains: bool,
    /// Requests, errors and latency of a stage (or of a route with --route) over --since
    #[arg(long, group = "view")]
    metrics: bool,
    /// The OpenAPI 3 definition of a stage (JSON; --yaml for YAML)
    #[arg(long, group = "view")]
    export: bool,
    /// Markdown report of a stage (or a route with --route) for an AI agent: settings, metrics, recent errors
    #[arg(long, group = "view")]
    report: bool,
    /// Call a route: 'METHOD /path' with its parameters filled in ('GET /orders/42'). REST
    /// APIs go through API Gateway's test invoke (the integration runs, the authorizer is
    /// skipped); HTTP APIs get a real request. Exit code 1 unless the status is 2xx or 3xx
    #[arg(long, group = "view", value_name = "'METHOD /PATH'")]
    test: Option<String>,
    /// Deploy the API as it is now to this stage
    #[arg(long, group = "view", value_name = "STAGE")]
    redeploy: Option<String>,
    /// Point this stage back at an earlier deployment (the one before, or --deployment)
    #[arg(long, group = "view", value_name = "STAGE")]
    rollback: Option<String>,
    /// Point --to at the deployment this stage runs
    #[arg(long, group = "view", value_name = "STAGE", requires = "to")]
    promote: Option<String>,
    /// The stage --promote changes
    #[arg(long, value_name = "STAGE")]
    to: Option<String>,
    /// The deployment --rollback goes to
    #[arg(long, value_name = "ID")]
    deployment: Option<String>,
    /// The stage (default: the API's only stage)
    #[arg(long, short = 's')]
    stage: Option<String>,
    /// A route for --metrics and --report: 'GET /orders/{id}'
    #[arg(long, value_name = "'METHOD /PATH'")]
    route: Option<String>,
    /// Window for --metrics: 1h, 24h or 7d
    #[arg(long, default_value = "24h", value_name = "WINDOW")]
    since: String,
    /// --export as YAML
    #[arg(long)]
    yaml: bool,
    /// --test: query string ('a=1&b=2')
    #[arg(long, value_name = "QUERY")]
    query: Option<String>,
    /// --test: a header, 'Name: value' (repeatable)
    #[arg(long = "header", short = 'H', value_name = "'NAME: VALUE'")]
    headers: Vec<String>,
    /// --test: the request body ("-" reads stdin, with --yes)
    #[arg(long, value_name = "BODY", allow_hyphen_values = true)]
    body: Option<String>,
    /// --redeploy: the new deployment's description
    #[arg(long, short = 'm', value_name = "TEXT")]
    description: Option<String>,
    /// Do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let (env, api) = env_and(config, args.env.clone(), args.api.clone())?;
    let aws = aws_for(config, &env)?;
    if args.domains {
        return domains(&aws, api.as_deref(), args.json);
    }
    let Some(id) = api else {
        if args.routes || args.stages || args.metrics || args.export || args.report || args.test.is_some() || args.redeploy.is_some() || args.rollback.is_some() || args.promote.is_some() {
            bail!("which API? {} apigw [ENV] API …", program());
        }
        return list(&aws, &env, args.json);
    };
    let api = apigw::find_api(&aws, &id)?;
    if args.export {
        let detail = apigw::detail(&aws, &api)?;
        let stage = pick_stage(&api, &detail, args.stage.as_deref())?;
        print!("{}", apigw::export(&aws, &api, &stage.name, args.yaml)?);
        return Ok(ExitCode::SUCCESS);
    }
    if args.report {
        let detail = apigw::detail(&aws, &api)?;
        let stage = pick_stage(&api, &detail, args.stage.as_deref())?;
        let route = args.route.as_deref().map(|r| find_route(&detail, r)).transpose()?.map(Route::key);
        print!("{}", apigw::gather_report(&aws, &env, &aws.region, &api, &stage.name, route.as_deref(), now_secs())?);
        return Ok(ExitCode::SUCCESS);
    }
    if args.metrics {
        let range = Range::parse(&args.since).ok_or_else(|| anyhow!("--since: use 1h, 24h or 7d"))?;
        return metrics(&aws, &api, &args, range);
    }
    if let Some(request) = &args.test {
        return test(&aws, &env, &api, request, &args);
    }
    if let Some(stage) = &args.redeploy {
        return redeploy(&aws, &env, &api, stage, &args);
    }
    if let Some(stage) = &args.rollback {
        return rollback(&aws, &env, &api, stage, &args);
    }
    if let (Some(from), Some(to)) = (&args.promote, &args.to) {
        return promote(&aws, &env, &api, from, to, args.yes);
    }
    let detail = apigw::detail(&aws, &api)?;
    if args.json {
        print_json(&api_json(&api, &detail, &aws.region));
        return Ok(ExitCode::SUCCESS);
    }
    if !args.stages {
        header(&api, &aws.region);
    }
    if !args.stages {
        routes(&api, &detail);
    }
    if !args.routes {
        if !args.stages {
            println!();
        }
        stages(&detail);
    }
    for w in &detail.warnings {
        println!("{}", paint(&format!("could not read {w}"), "33"));
    }
    if !args.routes && !args.stages {
        println!(
            "{}",
            paint(&format!("More: {0} apigw {1} --metrics -s STAGE · --test 'GET /path' -s STAGE · --report -s STAGE · --export -s STAGE", program(), api.name), "90")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn list(aws: &Aws, env: &str, json: bool) -> Result<ExitCode> {
    let apis = apigw::list_apis(aws)?;
    if json {
        let out: Vec<Value> = apis.iter().map(|a| api_summary(a, &aws.region)).collect();
        print_json(&Value::Array(out));
        return Ok(ExitCode::SUCCESS);
    }
    if apis.is_empty() {
        println!("No APIs in {env}.");
        return Ok(ExitCode::SUCCESS);
    }
    let width = apis.iter().map(|a| a.name.chars().count()).max().unwrap_or(4).max(3) + 2;
    println!("{}", paint(&format!("{:<width$}{:<6}{:<11}{:<12}{}", "api", "type", "endpoint", "id", "created"), "90"));
    for a in &apis {
        println!(
            "{:<width$}{:<6}{:<11}{:<12}{}",
            a.name,
            a.kind.short(),
            a.endpoint_type.to_lowercase(),
            a.id,
            paint(&a.created.map(fmt_ago).unwrap_or_default(), "90")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn api_summary(a: &Api, region: &str) -> Value {
    json!({
        "id": a.id,
        "name": a.name,
        "type": a.kind.label(),
        "endpointType": a.endpoint_type,
        "description": a.description,
        "created": a.created,
        "endpoint": a.endpoint(region),
        "defaultEndpointDisabled": a.default_endpoint_disabled,
    })
}

fn route_json(api: &Api, detail: &ApiDetail, r: &Route) -> Value {
    let i = r.integration.as_ref();
    json!({
        "key": r.key(),
        "method": r.method,
        "path": r.path,
        "authorization": r.auth.label(),
        "authorizer": detail.authorizer(r.authorizer_id.as_deref()).map(|a| a.name.clone()),
        "apiKeyRequired": r.api_key_required,
        "unprotected": apigw::unprotected(api.kind, r),
        "integration": i.map(|i| json!({ "type": i.kind, "subtype": i.subtype, "uri": i.uri, "target": i.target().label(), "timeoutMs": i.timeout_ms })),
    })
}

fn stage_json(s: &Stage, detail: &ApiDetail) -> Value {
    let d = detail.deployment(s.deployment_id.as_deref());
    json!({
        "name": s.name,
        "deploymentId": s.deployment_id,
        "deployed": d.and_then(|d| d.created),
        "deploymentDescription": d.and_then(|d| d.description.clone()),
        "autoDeploy": s.auto_deploy,
        // Values can be secrets: names only.
        "variables": s.variables.keys().collect::<Vec<_>>(),
        "cache": s.cache.as_ref().map(|(size, status)| json!({ "sizeGb": size, "status": status })),
        "throttle": { "rate": s.throttle_rate, "burst": s.throttle_burst },
        "detailedMetrics": s.detailed_metrics,
        "executionLogs": s.logging_level,
        "accessLogs": s.access_log,
        "canary": s.canary.as_ref().map(|(pct, dep)| json!({ "percent": pct, "deploymentId": dep })),
        "tracing": s.tracing,
        "webAcl": s.web_acl,
    })
}

fn api_json(api: &Api, detail: &ApiDetail, region: &str) -> Value {
    let mut v = api_summary(api, region);
    v["routes"] = Value::Array(detail.routes.iter().map(|r| route_json(api, detail, r)).collect());
    v["stages"] = Value::Array(detail.stages.iter().map(|s| stage_json(s, detail)).collect());
    v["deployments"] = Value::Array(
        detail
            .deployments
            .iter()
            .map(|d| json!({ "id": d.id, "created": d.created, "description": d.description, "status": d.status }))
            .collect(),
    );
    v
}

fn header(api: &Api, region: &str) {
    println!("{}  {}", paint(&api.name, "1"), paint(&format!("{} · {} · {}", api.kind.label(), api.endpoint_type.to_lowercase(), api.id), "90"));
    if let Some(d) = &api.description {
        println!("  {d}");
    }
    let endpoint = api.endpoint(region);
    let off = if api.default_endpoint_disabled { paint("  (disabled: only custom domains reach it)", "33") } else { String::new() };
    println!("  {} {endpoint}{off}", paint("endpoint", "90"));
    println!();
}

fn routes(api: &Api, detail: &ApiDetail) {
    if detail.routes.is_empty() {
        println!("No routes.");
        return;
    }
    // WebSocket routes have no method.
    let method_w = detail.routes.iter().map(|r| r.method.len()).max().unwrap_or(0);
    let method_w = if method_w == 0 { 0 } else { method_w.max(3) + 2 };
    let path_w = detail.routes.iter().map(|r| r.path.chars().count()).max().unwrap_or(4).min(40) + 2;
    let target_w = detail
        .routes
        .iter()
        .map(|r| r.integration.as_ref().map_or(4, |i| i.target().label().chars().count()))
        .max()
        .unwrap_or(4)
        .min(36)
        + 2;
    for r in &detail.routes {
        let target = r.integration.as_ref().map_or_else(|| "—".to_string(), |i| i.target().label());
        let target = crate::util::ellipsize(&target, target_w - 2);
        let mut auth = r.auth.label();
        if let Some(a) = detail.authorizer(r.authorizer_id.as_deref()) {
            auth = format!("{auth} ({})", a.name);
        }
        let auth = if apigw::unprotected(api.kind, r) { paint(&format!("{auth} ⚠"), "33") } else { auth };
        let key = if r.api_key_required { paint("  api key", "90") } else { String::new() };
        // Padded before painting: escape codes would count as width.
        let method = paint(&format!("{:<method_w$}", r.method), "1");
        println!("{method}{:<path_w$}{:<target_w$}{auth}{key}", r.path, target);
    }
}

fn stages(detail: &ApiDetail) {
    if detail.stages.is_empty() {
        println!("No stages: the API was never deployed.");
        return;
    }
    let width = detail.stages.iter().map(|s| s.name.chars().count()).max().unwrap_or(5) + 2;
    println!("{}", paint(&format!("{:<width$}{:<14}{:<12}{}", "stage", "deployment", "deployed", "description"), "90"));
    for (i, s) in detail.stages.iter().enumerate() {
        let d = detail.deployment(s.deployment_id.as_deref());
        let same = detail.stages[..i].iter().rev().find(|o| o.deployment_id.is_some() && o.deployment_id == s.deployment_id);
        let description = match same {
            Some(o) => paint(&format!("= {}", o.name), "90"),
            None => d.and_then(|d| d.description.clone()).unwrap_or_default(),
        };
        let behind = i
            .checked_sub(1)
            .and_then(|p| detail.behind(s, &detail.stages[p]).map(|n| (n, &detail.stages[p])))
            .filter(|(n, _)| *n > 0)
            .map(|(n, o)| paint(&format!("  {n} behind {}", o.name), "33"))
            .unwrap_or_default();
        let auto = if s.auto_deploy { paint("  auto-deploy", "90") } else { String::new() };
        println!(
            "{:<width$}{:<14}{:<12}{description}{behind}{auto}",
            s.name,
            s.deployment_id.as_deref().unwrap_or("—"),
            d.and_then(|d| d.created).map(fmt_ago).unwrap_or_default()
        );
        let mut settings = Vec::new();
        if let (Some(rate), Some(burst)) = (s.throttle_rate, s.throttle_burst) {
            settings.push(format!("throttle {rate}/s burst {burst}"));
        }
        if let Some((size, _)) = &s.cache {
            settings.push(format!("cache {size} GB"));
        }
        if let Some(level) = &s.logging_level {
            settings.push(format!("execution logs {}", level.to_lowercase()));
        }
        if let Some(g) = s.access_log_group() {
            settings.push(format!("access logs {g}"));
        }
        if s.detailed_metrics {
            settings.push("per-route metrics".into());
        }
        if let Some((pct, _)) = s.canary {
            settings.push(format!("canary {pct}%"));
        }
        if !s.variables.is_empty() {
            settings.push(format!("variables {}", s.variables.keys().cloned().collect::<Vec<_>>().join(", ")));
        }
        if !settings.is_empty() {
            println!("{:<width$}{}", "", paint(&settings.join(" · "), "90"));
        }
    }
}

fn domains(aws: &Aws, api: Option<&str>, json: bool) -> Result<ExitCode> {
    let all = apigw::domains(aws)?;
    let apis = apigw::list_apis(aws).unwrap_or_default();
    let name_of = |id: &str| apis.iter().find(|a| a.id == id).map_or(id.to_string(), |a| a.name.clone());
    let only = api.map(|a| apis.iter().find(|x| x.id == a || x.name == a).map_or(a.to_string(), |x| x.id.clone()));
    let wanted = |m: &apigw::Mapping| only.as_ref().is_none_or(|id| m.api_id == *id);
    if json {
        let out: Vec<Value> = all
            .iter()
            .map(|d| {
                json!({
                    "domain": d.name,
                    "endpointType": d.endpoint_type,
                    "securityPolicy": d.security_policy,
                    "status": d.status,
                    "target": d.target,
                    "mappings": d.mappings.iter().filter(|m| wanted(m)).map(|m| json!({ "path": m.key, "apiId": m.api_id, "api": name_of(&m.api_id), "stage": m.stage, "url": d.url(m) })).collect::<Vec<_>>(),
                })
            })
            .collect();
        print_json(&Value::Array(out));
        return Ok(ExitCode::SUCCESS);
    }
    if all.is_empty() {
        println!("No custom domain names.");
        return Ok(ExitCode::SUCCESS);
    }
    for d in &all {
        let mappings: Vec<&apigw::Mapping> = d.mappings.iter().filter(|m| wanted(m)).collect();
        if only.is_some() && mappings.is_empty() {
            continue;
        }
        let info = [d.endpoint_type.as_deref().map(str::to_lowercase), d.security_policy.clone(), d.status.as_deref().map(str::to_lowercase)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        println!("{}  {}", paint(&d.name, "1"), paint(&info, "90"));
        if let Some(e) = &d.error {
            println!("  {}", paint(&format!("mappings could not be read: {e}"), "33"));
        }
        for m in mappings {
            let path = if m.key.is_empty() { "/".to_string() } else { format!("/{}", m.key) };
            println!("  {path:<16} → {} · {}", name_of(&m.api_id), m.stage);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The stage named `wanted`, or the API's only one.
fn pick_stage<'a>(api: &Api, detail: &'a ApiDetail, wanted: Option<&str>) -> Result<&'a Stage> {
    let names = || detail.stages.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ");
    match wanted {
        Some(w) => detail.stage(w).ok_or_else(|| anyhow!("{} has no stage '{w}' (stages: {})", api.name, names())),
        None => match detail.stages.as_slice() {
            [only] => Ok(only),
            [] => bail!("{} has no stages: it was never deployed", api.name),
            _ => bail!("which stage? --stage {}", names().replace(", ", "|")),
        },
    }
}

/// `'GET /orders/{id}'` (a route key) or a concrete request (`GET /orders/42`).
fn find_route<'a>(detail: &'a ApiDetail, text: &str) -> Result<&'a Route> {
    if let Some(r) = detail.route(text.trim()) {
        return Ok(r);
    }
    let (method, path) = text.trim().split_once(' ').unwrap_or(("ANY", text.trim()));
    apigw::match_route(&detail.routes, method, path.trim()).ok_or_else(|| anyhow!("no route serves '{text}'"))
}

fn metrics(aws: &Aws, api: &Api, args: &Args, range: Range) -> Result<ExitCode> {
    let detail = apigw::detail(aws, api)?;
    let stage = pick_stage(api, &detail, args.stage.as_deref())?;
    let route = args.route.as_deref().map(|r| find_route(&detail, r)).transpose()?;
    if route.is_some() && !stage.detailed_metrics {
        eprintln!("{}", paint(&format!("{} has per-route (detailed) metrics off: CloudWatch may have none for this route", stage.name), "33"));
    }
    let m = apigw::metrics(aws, api, &stage.name, route, range, now_secs())?;
    if args.json {
        let series: serde_json::Map<String, Value> = m.series.iter().map(|s| (s.id.clone(), json!(s.points))).collect();
        print_json(&json!({ "api": api.name, "stage": stage.name, "route": route.map(Route::key), "window": [m.window.0, m.window.1], "series": series }));
        return Ok(ExitCode::SUCCESS);
    }
    const WIDTH: usize = 48;
    let line = |label: &str, id: &str, agg: Agg, numbers: String, color: &str| {
        let spark = sparkline(&columns(m.points(id), m.window, WIDTH, agg));
        println!("  {:<13} {}  {}", paint(label, "1"), paint(&spark, color), paint(&numbers, "90"));
    };
    let scope = route.map_or_else(|| stage.name.clone(), |r| format!("{} · {}", stage.name, r.key()));
    println!("{} · {scope} · last {}", paint(&api.name, "1"), range.label());
    if m.points("count").is_empty() {
        println!("  {}", paint(&format!("No requests in the last {}.", range.label()), "90"));
        return Ok(ExitCode::SUCCESS);
    }
    let pct = |id: &str| m.rate(id).map(|r| format!(" ({:.2}%)", r * 100.0)).unwrap_or_default();
    let requests = if api.kind == Kind::WebSocket { "Messages" } else { "Requests" };
    line(requests, "count", Agg::Sum, format!("{} total", fmt_count(m.total("count"))), "36");
    if api.kind == Kind::WebSocket {
        line("Connections", "connects", Agg::Sum, format!("{} total", fmt_count(m.total("connects"))), "36");
    }
    let errors = m.total("5xx");
    let ws = api.kind == Kind::WebSocket;
    line(if ws { "Integ. errors" } else { "5xx" }, "5xx", Agg::Sum, format!("{}{}", fmt_count(errors), pct("5xx")), if errors > 0.0 { "31" } else { "32" });
    line(if ws { "Client errors" } else { "4xx" }, "4xx", Agg::Sum, format!("{}{}", fmt_count(m.total("4xx")), pct("4xx")), "33");
    if api.kind != Kind::WebSocket {
        let avg = m.mean("latency").map(fmt_ms).unwrap_or_else(|| "—".into());
        let worst = m.max("p99").map(fmt_ms).unwrap_or_else(|| "—".into());
        line("Latency p99", "p99", Agg::Max, format!("avg {avg} · worst p99 {worst}"), "94");
    }
    let integration = m.mean("integration").map(fmt_ms).unwrap_or_else(|| "—".into());
    let overhead = m.overhead().map(|o| format!(" · API Gateway adds {}", fmt_ms(o))).unwrap_or_default();
    line("Integration", "integration_p99", Agg::Max, format!("avg {integration}{overhead}"), "94");
    if stage.cache.is_some() {
        let lookups = m.total("hits") + m.total("misses");
        let rate = if lookups > 0.0 { format!("{:.0}% hits", m.total("hits") / lookups * 100.0) } else { "no lookups".into() };
        line("Cache hits", "hits", Agg::Sum, rate, "32");
    }
    Ok(ExitCode::SUCCESS)
}

fn test(aws: &Aws, env: &str, api: &Api, request: &str, args: &Args) -> Result<ExitCode> {
    let (method, path) = request.trim().split_once(' ').ok_or_else(|| anyhow!("--test takes 'METHOD /path', like 'GET /orders/42'"))?;
    let (method, path) = (method.to_uppercase(), path.trim().to_string());
    if !path.starts_with('/') {
        bail!("the path starts with '/': --test '{method} /orders/42'");
    }
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p.to_string(), args.query.clone().unwrap_or_else(|| q.to_string())),
        None => (path, args.query.clone().unwrap_or_default()),
    };
    let missing = apigw::unfilled(&path);
    if !missing.is_empty() {
        bail!("fill in the path parameters ({}): --test '{method} /…/value'", missing.join(", "));
    }
    let headers = TestRequest::parse_headers(&args.headers.join("\n")).map_err(|e| anyhow!(e))?;
    let body = match &args.body {
        Some(b) => read_value(b)?,
        None => String::new(),
    };
    let detail = apigw::detail(aws, api)?;
    let stage = pick_stage(api, &detail, args.stage.as_deref())?;
    let req = TestRequest { path: path.clone(), query, headers, body };
    let result = match api.kind {
        Kind::WebSocket => bail!("WebSocket APIs are not tested here; connect with: {}", apigw::wscat(api, &stage.name, &aws.region, &detail)),
        Kind::Rest => {
            let route = apigw::match_route(&detail.routes, &method, &path).ok_or_else(|| anyhow!("no route of {} serves {method} {path}", api.name))?;
            let question = format!(
                "Test {} on {} {}? The integration runs for real (the authorizer is skipped).",
                route.key(),
                api.title(),
                stage.name
            );
            if !confirm(env, &stage.name, &question, args.yes)? {
                bail!("not sent");
            }
            apigw::test_invoke(aws, api, route, &req, &stage.variables)?
        }
        Kind::Http => {
            let url = apigw::test_url(api, &stage.name, &aws.region, &req)?;
            if !confirm(env, &stage.name, &format!("Send {method} {url} ({})? It is a real request.", api.title()), args.yes)? {
                bail!("not sent");
            }
            apigw::call(&method, &url, &req.headers, &req.body)?
        }
    };
    if args.json {
        print_json(&json!({
            "status": result.status,
            "latencyMs": result.latency_ms,
            "headers": result.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<String, Value>>(),
            "body": serde_json::from_str::<Value>(&result.body)
                .map(|mut v| {
                    // Decoded escapes would bring back what the body was sanitized of.
                    crate::util::sanitize_json(&mut v);
                    v
                })
                .unwrap_or(Value::String(result.body.clone())),
            "truncated": result.truncated,
            "log": result.log,
        }));
    } else {
        for line in apigw::result_lines(&result) {
            println!("{line}");
        }
    }
    Ok(if (200..400).contains(&result.status) { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

fn redeploy(aws: &Aws, env: &str, api: &Api, stage: &str, args: &Args) -> Result<ExitCode> {
    let detail = apigw::detail(aws, api)?;
    let s = pick_stage(api, &detail, Some(stage))?;
    if s.auto_deploy {
        bail!("{} deploys by itself on every change (auto-deploy): nothing to redeploy", s.name);
    }
    let now = detail.deployment(s.deployment_id.as_deref()).map(describe).unwrap_or_else(|| "no deployment".into());
    let question = format!("Deploy {} as it is now to {} (it runs {now})?", api.title(), s.name);
    if !confirm(env, &s.name, &question, args.yes)? {
        bail!("not deployed");
    }
    let d = apigw::create_deployment(aws, api, &s.name, args.description.as_deref().unwrap_or(""))?;
    println!("{} {} now runs deployment {}", paint("✓", "32"), s.name, d.id);
    Ok(ExitCode::SUCCESS)
}

fn rollback(aws: &Aws, env: &str, api: &Api, stage: &str, args: &Args) -> Result<ExitCode> {
    let detail = apigw::detail(aws, api)?;
    let s = pick_stage(api, &detail, Some(stage))?;
    if let Some(why) = apigw::stage_locked(s) {
        bail!("{why}");
    }
    let target = match &args.deployment {
        Some(id) => detail.deployments.iter().find(|d| d.id == *id).ok_or_else(|| anyhow!("{} has no deployment {id}", api.name))?,
        None => apigw::previous_deployment(&detail, s).ok_or_else(|| anyhow!("no earlier deployment than the one {} runs", s.name))?,
    };
    if Some(&target.id) == s.deployment_id.as_ref() {
        println!("{} already runs {}", s.name, target.id);
        return Ok(ExitCode::SUCCESS);
    }
    let now = detail.deployment(s.deployment_id.as_deref()).map(describe).unwrap_or_else(|| "no deployment".into());
    let question = format!("Point {} {} from {now} back to {}?", api.title(), s.name, describe(target));
    if !confirm(env, &s.name, &question, args.yes)? {
        bail!("not changed");
    }
    apigw::set_stage_deployment(aws, api, &s.name, &target.id)?;
    println!("{} {} now runs deployment {}", paint("✓", "32"), s.name, target.id);
    Ok(ExitCode::SUCCESS)
}

fn promote(aws: &Aws, env: &str, api: &Api, from: &str, to: &str, yes: bool) -> Result<ExitCode> {
    let detail = apigw::detail(aws, api)?;
    let source = pick_stage(api, &detail, Some(from))?;
    let target = pick_stage(api, &detail, Some(to))?;
    if let Some(why) = apigw::stage_locked(target) {
        bail!("{why}");
    }
    let Some(id) = source.deployment_id.clone() else { bail!("{} runs no deployment", source.name) };
    if target.deployment_id.as_deref() == Some(id.as_str()) {
        println!("{} already runs the deployment of {} ({id})", target.name, source.name);
        return Ok(ExitCode::SUCCESS);
    }
    let what = detail.deployment(Some(&id)).map(describe).unwrap_or_else(|| id.clone());
    let now = detail.deployment(target.deployment_id.as_deref()).map(describe).unwrap_or_else(|| "no deployment".into());
    let question = format!("Point {} {} from {now} to {what}, the deployment {} runs?", api.title(), target.name, source.name);
    if !confirm(env, &target.name, &question, yes)? {
        bail!("not changed");
    }
    apigw::set_stage_deployment(aws, api, &target.name, &id)?;
    println!("{} {} now runs deployment {id}", paint("✓", "32"), target.name);
    Ok(ExitCode::SUCCESS)
}

/// Asks before a write or a test call: typing the name of the environment, or of the stage
/// when only the stage looks like production (a `prod` stage in a dev account).
fn confirm(env: &str, stage: &str, question: &str, yes: bool) -> Result<bool> {
    let name = if !is_production(env) && is_production(stage) { stage } else { env };
    confirm_write(name, question, yes)
}

fn describe(d: &apigw::Deployment) -> String {
    let mut text = format!("deployment {}", d.id);
    if let Some(t) = d.created {
        text.push_str(&format!(" ({})", fmt_ago(t)));
    }
    if let Some(desc) = &d.description {
        text.push_str(&format!(" \"{desc}\""));
    }
    text
}

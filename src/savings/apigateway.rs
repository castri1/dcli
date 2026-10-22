//! API Gateway: REST APIs that use nothing only REST has (HTTP API candidates), caches that rarely hit.

use std::collections::{HashMap, HashSet};

use super::{Finding, Impact, Scan, qty};
use crate::aws::apigateway::{self as apigw, Api, ApiDetail, Integration};
use crate::aws::metrics::{self, Query};
use crate::aws::{self, Aws};
use crate::util::now_secs;

/// Requests a month that make an HTTP API candidate matter a lot, or somewhat: requests
/// are what REST APIs bill per, at a higher rate than HTTP APIs.
const HIGH_REQUESTS: f64 = 10_000_000.0;
const MEDIUM_REQUESTS: f64 = 1_000_000.0;
/// Below this hit rate a stage cache answers too few requests to be worth running.
const LOW_HIT_RATE: f64 = 0.2;
/// Cache sizes (GB) from which a cache running all month matters a lot, or somewhat: a
/// cache bills by the hour, more the larger it is, whether it is used or not.
const HIGH_CACHE_GB: f64 = 13.5;
const MEDIUM_CACHE_GB: f64 = 1.6;
/// APIs read in parallel.
const PARALLEL: usize = 6;

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let apis = apigw::list_rest_apis(scan.aws)?;
    if apis.is_empty() {
        return Ok(Vec::new());
    }
    let plans = apigw::usage_plan_stages(scan.aws).ok();
    let details = read_details(scan.aws, apis);
    let usage = usage(scan, &details)?;
    let mut out = Vec::new();
    for (api, detail) in &details {
        out.extend(http_candidate(scan, api, detail, plans.as_deref(), &usage));
        out.extend(caches(scan, api, detail, &usage));
    }
    Ok(out)
}

/// Each API with its routes and stages; APIs that cannot be read are left out.
fn read_details(aws: &Aws, apis: Vec<Api>) -> Vec<(Api, ApiDetail)> {
    let mut out = Vec::new();
    for chunk in apis.chunks(PARALLEL) {
        let read: Vec<Option<ApiDetail>> = std::thread::scope(|s| {
            let jobs: Vec<_> = chunk.iter().map(|api| s.spawn(move || apigw::detail(aws, api).ok())).collect();
            jobs.into_iter().map(|j| j.join().ok().flatten()).collect()
        });
        out.extend(chunk.iter().cloned().zip(read).filter_map(|(api, d)| Some((api, d?))));
    }
    out
}

/// What a stage served over the scan: requests, and cache hits and misses.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Usage {
    requests: f64,
    hits: f64,
    misses: f64,
}

/// Usage per (API id, stage), read in as few GetMetricData calls as possible.
fn usage(scan: &Scan, details: &[(Api, ApiDetail)]) -> aws::Result<HashMap<(String, String), Usage>> {
    let mut queries = Vec::new();
    let mut keys = Vec::new();
    for (api, detail) in details {
        for stage in &detail.stages {
            let i = keys.len();
            let dims = vec![("ApiName".to_string(), api.name.clone()), ("Stage".to_string(), stage.name.clone())];
            let mut add = |prefix: &str, metric: &str| {
                queries.push(Query {
                    id: format!("{prefix}{i}"),
                    namespace: "AWS/ApiGateway".into(),
                    name: metric.into(),
                    dimensions: dims.clone(),
                    stat: "Sum".into(),
                    period: 86_400,
                })
            };
            add("c", "Count");
            if stage.cache.is_some() {
                add("h", "CacheHitCount");
                add("m", "CacheMissCount");
            }
            keys.push((api.id.clone(), stage.name.clone()));
        }
    }
    let end = now_secs();
    let start = end - scan.days as f64 * 86_400.0;
    let mut out: HashMap<(String, String), Usage> = HashMap::new();
    for chunk in queries.chunks(450) {
        for s in metrics::get_metric_data(scan.aws, chunk, start, end)? {
            let (prefix, index) = s.id.split_at(1);
            let Some(key) = index.parse::<usize>().ok().and_then(|i| keys.get(i)) else { continue };
            let total: f64 = s.points.iter().map(|p| p.1).sum();
            let u = out.entry(key.clone()).or_default();
            match prefix {
                "c" => u.requests += total,
                "h" => u.hits += total,
                _ => u.misses += total,
            }
        }
    }
    Ok(out)
}

/// What keeps a REST API from moving to an HTTP API as it is. `plans`: the (plan, API id,
/// stage) of the usage plans, `None` when they could not be read.
fn blockers(api: &Api, detail: &ApiDetail, plans: Option<&[(String, String, String)]>) -> Vec<&'static str> {
    let mut out = Vec::new();
    match api.endpoint_type.as_str() {
        "PRIVATE" => out.push("a private endpoint"),
        "EDGE" => out.push("an edge-optimized endpoint (HTTP APIs are regional)"),
        _ => {}
    }
    if api.has_policy {
        out.push("a resource policy");
    }
    let in_plans = plans.is_some_and(|p| p.iter().any(|(_, id, _)| *id == api.id));
    if in_plans || detail.routes.iter().any(|r| r.api_key_required) {
        out.push("API keys or usage plans");
    }
    if detail.routes.iter().any(|r| r.request_validator_id.is_some()) {
        out.push("request validation");
    }
    let integrations: Vec<&Integration> = detail.routes.iter().filter_map(|r| r.integration.as_ref()).collect();
    if integrations.iter().any(|i| i.transforms()) {
        out.push("mapping templates (VTL)");
    }
    if integrations.iter().any(|i| i.kind == "MOCK") {
        out.push("mock integrations");
    }
    if integrations.iter().any(|i| i.kind == "AWS") {
        out.push("AWS service integrations");
    }
    if detail.authorizers.iter().any(|a| a.kind == "TOKEN") {
        out.push("TOKEN Lambda authorizers");
    }
    let stages = &detail.stages;
    if stages.iter().any(|s| s.cache.is_some()) {
        out.push("a stage cache");
    }
    if stages.iter().any(|s| s.web_acl.is_some()) {
        out.push("AWS WAF");
    }
    if stages.iter().any(|s| s.canary.is_some()) {
        out.push("canary deployments");
    }
    if stages.iter().any(|s| s.tracing) {
        out.push("X-Ray tracing");
    }
    if stages.iter().any(|s| s.client_certificate.is_some()) {
        out.push("client certificates");
    }
    out
}

fn requests_impact(monthly: f64) -> Impact {
    if monthly >= HIGH_REQUESTS {
        Impact::High
    } else if monthly >= MEDIUM_REQUESTS {
        Impact::Medium
    } else {
        Impact::Low
    }
}

fn http_candidate(
    scan: &Scan,
    api: &Api,
    detail: &ApiDetail,
    plans: Option<&[(String, String, String)]>,
    usage: &HashMap<(String, String), Usage>,
) -> Option<Finding> {
    if !blockers(api, detail, plans).is_empty() || detail.routes.is_empty() {
        return None;
    }
    let requests: f64 = detail.stages.iter().filter_map(|s| usage.get(&(api.id.clone(), s.name.clone()))).map(|u| u.requests).sum();
    // REST APIs bill per request: without requests there is nothing to save.
    if requests <= 0.0 {
        return None;
    }
    let monthly = requests * 30.0 / scan.days.max(1) as f64;
    let stages: Vec<&str> = detail.stages.iter().map(|s| s.name.as_str()).collect();
    let mut finding = Finding::new(
        "API Gateway",
        api.id.clone(),
        api.name.clone(),
        "REST API that uses nothing only REST APIs have",
        "Move it to an HTTP API (Lambda and HTTP integrations carry over; Cognito becomes a JWT authorizer)",
        format!("{} requests a month billed at the REST API rate", qty(monthly)),
        requests_impact(monthly),
    )
    .evidence(format!(
        "{} requests in the last {} days on stage{} {}",
        qty(requests),
        scan.days,
        if stages.len() == 1 { "" } else { "s" },
        stages.join(", ")
    ))
    .evidence(
        "Checked: regional endpoint, no resource policy, API keys, usage plans, request validators, mapping templates, \
         mock or AWS service integrations, TOKEN authorizers, cache, WAF, canary, X-Ray or client certificates",
    )
    .caveat("dcli has no prices: compare the REST and HTTP API request prices for your region")
    .caveat("Some behaviour differs (payload format, error responses, CORS, binary media): try the HTTP API in dev first")
    .caveat("Clients call a new endpoint unless a custom domain moves with the API");
    if plans.is_none() {
        finding = finding.caveat("Usage plans could not be read: check that none uses this API");
    }
    Some(finding)
}

fn cache_impact(size_gb: f64, wasted: bool) -> Impact {
    let impact = if size_gb >= HIGH_CACHE_GB {
        Impact::High
    } else if size_gb >= MEDIUM_CACHE_GB {
        Impact::Medium
    } else {
        Impact::Low
    };
    // A cache that serves nothing is all waste, whatever its size.
    if wasted { impact.max(Impact::Medium) } else { impact }
}

fn caches(scan: &Scan, api: &Api, detail: &ApiDetail, usage: &HashMap<(String, String), Usage>) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for stage in &detail.stages {
        let Some((size, status)) = &stage.cache else { continue };
        if !seen.insert(&stage.name) {
            continue;
        }
        let u = usage.get(&(api.id.clone(), stage.name.clone())).copied().unwrap_or_default();
        let gb: f64 = size.parse().unwrap_or(0.5);
        let lookups = u.hits + u.misses;
        let (summary, recommendation, wasted) = if stage.cached_methods.is_empty() {
            (format!("Stage {} runs a cache no method uses", stage.name), "Turn the stage cache off".to_string(), true)
        } else if u.requests <= 0.0 {
            (
                format!("Stage {} has a cache and no requests in {} days", stage.name, scan.days),
                "Turn the cache off, or delete the stage if nothing uses it".to_string(),
                true,
            )
        } else if lookups > 0.0 && u.hits / lookups < LOW_HIT_RATE {
            (
                format!("Stage {}'s cache answers {:.0}% of lookups", stage.name, u.hits / lookups * 100.0),
                "Turn the cache off, or cache what repeats (longer TTL, fewer cache keys)".to_string(),
                false,
            )
        } else {
            continue;
        };
        out.push(
            Finding::new(
                "API Gateway",
                api.id.clone(),
                format!("{} · {}", api.name, stage.name),
                summary,
                recommendation,
                format!("a {size} GB cache running all month (about 730 cache-hours)"),
                cache_impact(gb, wasted),
            )
            .evidence(format!(
                "Last {} days: {} requests, {} cache hits, {} misses · cache status {}",
                scan.days,
                qty(u.requests),
                qty(u.hits),
                qty(u.misses),
                if status.is_empty() { "unknown" } else { status }
            ))
            .evidence(format!(
                "Methods with caching on: {}",
                if stage.cached_methods.is_empty() { "none".to_string() } else { stage.cached_methods.join(", ") }
            ))
            .caveat("A cache can also protect the backend from load: check before turning it off"),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::aws::apigateway::{Auth, Authorizer, Kind, Route, Stage};

    fn api(endpoint: &str) -> Api {
        Api {
            id: "a1".into(),
            name: "public-api".into(),
            kind: Kind::Rest,
            description: None,
            endpoint_type: endpoint.into(),
            created: None,
            api_endpoint: None,
            default_endpoint_disabled: false,
            has_policy: false,
        }
    }

    fn route(integration: &str) -> Route {
        Route {
            id: "r".into(),
            method: "GET".into(),
            path: "/orders".into(),
            auth: Auth::Cognito,
            authorizer_id: Some("c".into()),
            api_key_required: false,
            operation_name: None,
            request_validator_id: None,
            request_parameters: Vec::new(),
            request_models: BTreeMap::new(),
            integration: Some(Integration { kind: integration.into(), uri: Some("arn:aws:lambda:us-east-1:1:function:f".into()), ..Default::default() }),
        }
    }

    fn detail(routes: Vec<Route>, stages: Vec<Stage>) -> ApiDetail {
        ApiDetail { routes, authorizers: Vec::new(), validators: Vec::new(), stages, deployments: Vec::new(), warnings: Vec::new() }
    }

    fn stage(name: &str) -> Stage {
        Stage { name: name.into(), ..Default::default() }
    }

    #[test]
    fn rest_apis_with_rest_only_features_are_not_candidates() {
        let plain = detail(vec![route("AWS_PROXY")], vec![stage("prod")]);
        assert!(blockers(&api("REGIONAL"), &plain, Some(&[])).is_empty());
        assert_eq!(blockers(&api("EDGE"), &plain, Some(&[])), ["an edge-optimized endpoint (HTTP APIs are regional)"]);
        let planned = [("gold".to_string(), "a1".to_string(), "prod".to_string())];
        assert_eq!(blockers(&api("REGIONAL"), &plain, Some(&planned)), ["API keys or usage plans"]);

        let mut vtl = route("AWS");
        vtl.integration.as_mut().unwrap().templates.insert("application/json".into(), "#set($x = 1)".into());
        let mut validated = route("AWS_PROXY");
        validated.request_validator_id = Some("v".into());
        let mut cached = stage("prod");
        cached.cache = Some(("0.5".into(), "AVAILABLE".into()));
        let mut busy = detail(vec![vtl, validated, route("MOCK")], vec![cached]);
        busy.authorizers.push(Authorizer {
            id: "t".into(),
            name: "legacy".into(),
            kind: "TOKEN".into(),
            identity_source: Vec::new(),
            providers: Vec::new(),
            function: None,
            issuer: None,
            audience: Vec::new(),
            ttl: None,
        });
        let found = blockers(&api("PRIVATE"), &busy, None);
        for expected in [
            "a private endpoint",
            "request validation",
            "mapping templates (VTL)",
            "mock integrations",
            "AWS service integrations",
            "TOKEN Lambda authorizers",
            "a stage cache",
        ] {
            assert!(found.contains(&expected), "{expected} in {found:?}");
        }
    }

    #[test]
    fn candidates_need_traffic_and_scale_with_it() {
        let aws = Aws::new("test", "us-east-1");
        let scan = Scan { aws: &aws, days: 14 };
        let d = detail(vec![route("AWS_PROXY")], vec![stage("dev"), stage("prod")]);
        let mut usage = HashMap::new();
        assert!(http_candidate(&scan, &api("REGIONAL"), &d, Some(&[]), &usage).is_none(), "no requests, nothing to save");
        usage.insert(("a1".to_string(), "prod".to_string()), Usage { requests: 7_000_000.0, ..Default::default() });
        let f = http_candidate(&scan, &api("REGIONAL"), &d, Some(&[]), &usage).unwrap();
        assert_eq!(f.impact, Impact::High);
        assert_eq!(f.reduction, "15 M requests a month billed at the REST API rate");
        assert!(!f.reduction.contains('$') && !f.summary.contains('%'));
        let f = http_candidate(&scan, &api("REGIONAL"), &d, None, &usage).unwrap();
        assert!(f.caveats.iter().any(|c| c.contains("Usage plans could not be read")));
    }

    #[test]
    fn caches_that_serve_little_are_found() {
        let aws = Aws::new("test", "us-east-1");
        let scan = Scan { aws: &aws, days: 14 };
        let mut prod = stage("prod");
        prod.cache = Some(("6.1".into(), "AVAILABLE".into()));
        prod.cached_methods = vec!["*/*".into()];
        let mut qa = stage("qa");
        qa.cache = Some(("0.5".into(), "AVAILABLE".into()));
        let d = detail(vec![route("AWS_PROXY")], vec![prod, qa]);
        let mut usage = HashMap::new();
        usage.insert(("a1".to_string(), "prod".to_string()), Usage { requests: 1000.0, hits: 50.0, misses: 950.0 });
        usage.insert(("a1".to_string(), "qa".to_string()), Usage { requests: 10.0, hits: 0.0, misses: 0.0 });
        let found = caches(&scan, &api("REGIONAL"), &d, &usage);
        assert_eq!(found.len(), 2);
        assert!(found[0].summary.contains("5% of lookups") && found[0].impact == Impact::Medium);
        assert!(found[1].summary.contains("no method uses") && found[1].impact == Impact::Medium);
        assert!(found.iter().all(|f| f.reduction.contains("GB cache running all month")));
        usage.insert(("a1".to_string(), "prod".to_string()), Usage { requests: 1000.0, hits: 800.0, misses: 200.0 });
        assert_eq!(caches(&scan, &api("REGIONAL"), &d, &usage).len(), 1, "a cache that hits is fine");
    }
}

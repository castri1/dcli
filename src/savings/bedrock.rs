//! Bedrock: models invoked with long prompts and no prompt caching, invocation logging that
//! writes every prompt and answer to CloudWatch Logs, and provisioned throughput nobody uses.
//!
//! Each model is billed as a service of its own (`Claude Sonnet 4 (Amazon Bedrock Edition)`)
//! or under `Amazon Bedrock`, with usage type names that vary: a finding finds the model's
//! service and input-token usage type on the bill by name ([`bill_for_model`]) and stays
//! unpriced, with its evidence, when it cannot.

use super::{Finding, GB, Impact, MONTH, Scan, billing, metric_data, per_month, qty, query, query_id, total};
use crate::aws::bedrock::{self, ProvisionedThroughput};
use crate::aws::{self, Aws};
use crate::costs::{self, Snapshot};

const MODULE: &str = "Bedrock";
const DAY: u32 = 86_400;
/// Input tokens a month without caching: medium and high impact from these.
const TOKENS_MEDIUM: f64 = 50_000_000.0;
const TOKENS_HIGH: f64 = 500_000_000.0;
/// Prompts shorter than this (tokens, on average) leave little for a cache: Claude caches
/// prefixes of 1,024 tokens or more.
const MIN_PROMPT: f64 = 1024.0;
/// GB of invocation logs a month: medium and high impact from these.
const LOG_GB_MEDIUM: f64 = 10.0;
const LOG_GB_HIGH: f64 = 100.0;

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let costs = costs::cached(scan.env);
    let models = bedrock::invoked_models(scan.aws)?;
    let mut out = caching_findings(&usage(scan.aws, &models, scan.days)?, costs.as_ref(), scan.days);
    if let Some(f) = logging(scan)? {
        out.push(f);
    }
    // Throughput provisioned during the window had no chance to be used over all of it.
    let since = crate::util::now_secs() - f64::from(scan.days) * 86_400.0;
    let provisioned: Vec<ProvisionedThroughput> = bedrock::provisioned(scan.aws)?
        .into_iter()
        .filter(|p| p.status == "InService")
        .filter(|p| p.creation_time.as_deref().and_then(crate::util::parse_rfc3339).is_none_or(|t| t < since))
        .collect();
    if !provisioned.is_empty() {
        let ids: Vec<String> = provisioned.iter().map(|p| p.provisioned_model_arn.clone()).collect();
        for (p, u) in provisioned.iter().zip(usage(scan.aws, &ids, scan.days)?) {
            if u.invocations <= 0.0 {
                out.push(idle_provisioned(p, costs.as_ref(), scan.days));
            }
        }
    }
    Ok(out)
}

/// What a model did over the window.
#[derive(Debug, Clone, Default, PartialEq)]
struct ModelUse {
    model: String,
    invocations: f64,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

fn usage(aws: &Aws, models: &[String], days: u32) -> aws::Result<Vec<ModelUse>> {
    const METRICS: [&str; 5] = ["Invocations", "InputTokenCount", "OutputTokenCount", "CacheReadInputTokenCount", "CacheWriteInputTokenCount"];
    let mut queries = Vec::new();
    for (i, m) in models.iter().enumerate() {
        for (j, metric) in METRICS.iter().enumerate() {
            queries.push(query(query_id(&format!("m{j}x"), i), "AWS/Bedrock", metric, &[("ModelId", m)], "Sum", DAY));
        }
    }
    let series = metric_data(aws, &queries, days)?;
    Ok(models
        .iter()
        .zip(series.chunks(METRICS.len()))
        .map(|(m, s)| ModelUse {
            model: m.clone(),
            invocations: total(&s[0]),
            input: total(&s[1]),
            output: total(&s[2]),
            cache_read: total(&s[3]),
            cache_write: total(&s[4]),
        })
        .collect())
}

/// Models Bedrock caches prompts for: Claude 3.5 Haiku, 3.7 Sonnet and the Claude 4 family,
/// and Amazon Nova.
fn caching_supported(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    ["claude-3-7", "claude-3-5-haiku", "claude-sonnet-4", "claude-opus-4", "claude-haiku-4", "nova-"].iter().any(|p| m.contains(p))
}

/// The words of a model id that name it: `us.anthropic.claude-sonnet-4-20250514-v1:0` →
/// `claude sonnet 4` (no region, vendor, date or version).
fn model_words(model: &str) -> Vec<String> {
    let lower = model.to_ascii_lowercase();
    let mut rest = lower.as_str();
    for prefix in ["us.", "eu.", "apac.", "global.", "us-gov."] {
        rest = rest.strip_prefix(prefix).unwrap_or(rest);
    }
    let rest = rest.split_once('.').map_or(rest, |(vendor, name)| if vendor.contains('-') { rest } else { name });
    rest.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .filter(|w| !(w.len() == 8 && w.bytes().all(|b| b.is_ascii_digit())))
        .filter(|w| !(w.starts_with('v') && w.len() <= 3 && w[1..].bytes().all(|b| b.is_ascii_digit())))
        .filter(|w| *w != "0")
        .map(str::to_string)
        .collect()
}

/// The words of a Cost Explorer service name, without `Amazon`, `Bedrock`, `Edition`.
fn service_words(service: &str) -> Vec<String> {
    service
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty() && !["amazon", "bedrock", "edition"].contains(w))
        .map(str::to_string)
        .collect()
}

/// Where the bill puts a model's input tokens: (service, usage type without region, monthly
/// quantity). Its own service (`Claude Sonnet 4 (Amazon Bedrock Edition)`: the same words as
/// the model's, or the fewest more), else `Amazon Bedrock` with a usage type that names it.
fn bill_for_model(s: &Snapshot, model: &str) -> Option<(String, String, f64)> {
    let words = model_words(model);
    if words.is_empty() {
        return None;
    }
    let mut services: Vec<&str> = Vec::new();
    for u in &s.usage {
        if u.service.contains("Bedrock") && !services.contains(&u.service.as_str()) {
            services.push(&u.service);
        }
    }
    let own = services
        .iter()
        .filter(|svc| **svc != "Amazon Bedrock")
        .filter_map(|svc| {
            let sw = service_words(svc);
            words.iter().all(|w| sw.contains(w)).then(|| (sw.len() - words.len(), *svc))
        })
        .min_by_key(|(extra, _)| *extra)
        .map(|(_, svc)| svc.to_string());
    let compact = words.concat();
    let input = |u: &crate::costs::UsageCost| {
        let name = billing::split_region(&u.usage_type).1.to_ascii_lowercase();
        name.contains("input") && !name.contains("cache")
    };
    let (service, names_model) = match own {
        Some(svc) => (svc, false),
        None => ("Amazon Bedrock".to_string(), true),
    };
    let mut by_name: Vec<(String, f64, f64)> = Vec::new();
    for u in s.usage.iter().filter(|u| u.service == service && input(u)) {
        let name = billing::split_region(&u.usage_type).1;
        if names_model && !name.to_ascii_lowercase().replace(['-', '_', '.'], "").contains(&compact) {
            continue;
        }
        match by_name.iter_mut().find(|(n, _, _)| n == name) {
            Some(slot) => {
                slot.1 += u.quantity;
                slot.2 += u.cost;
            }
            None => by_name.push((name.to_string(), u.quantity, u.cost)),
        }
    }
    by_name.sort_by(|a, b| b.2.total_cmp(&a.2));
    let (usage, quantity, _) = by_name.into_iter().next()?;
    Some((service, usage, quantity * 30.4375 / 30.0))
}

/// Whether `u` is a model with long prompts and nothing cached.
fn uncached(u: &ModelUse, days: u32) -> bool {
    caching_supported(&u.model)
        && u.invocations > 0.0
        && u.cache_read + u.cache_write <= 0.0
        && u.input * per_month(days) >= TOKENS_MEDIUM
        && u.input / u.invocations >= MIN_PROMPT
}

/// Where the bill puts a model's input (`service|usage`, and the service, usage type and monthly
/// quantity), or without it the model's name.
type BillKey = (String, Option<(String, String, f64)>);

/// One finding per billed model with long prompts and nothing cached. Model ids billed as one
/// (an inference profile, `us.anthropic.…`, and its foundation model) make one finding, so they
/// never claim the same tokens twice; each claims the share of the bill's input its own tokens
/// are (from CloudWatch), not all of it.
fn caching_findings(uses: &[ModelUse], costs: Option<&Snapshot>, days: u32) -> Vec<Finding> {
    let key = |u: &ModelUse| -> BillKey {
        match costs.and_then(|s| bill_for_model(s, &u.model)) {
            Some((service, usage, quantity)) => (format!("{service}|{usage}"), Some((service, usage, quantity))),
            None => (model_words(&u.model).join(" "), None),
        }
    };
    let keyed: Vec<(BillKey, &ModelUse)> = uses.iter().map(|u| (key(u), u)).collect();
    let mut out = Vec::new();
    let mut done: Vec<&str> = Vec::new();
    for ((k, billed), _) in &keyed {
        if done.contains(&k.as_str()) {
            continue;
        }
        done.push(k);
        let all: Vec<&ModelUse> = keyed.iter().filter(|((k2, _), _)| k2 == k).map(|(_, u)| *u).collect();
        let found: Vec<&ModelUse> = all.iter().copied().filter(|u| uncached(u, days)).collect();
        if found.is_empty() {
            continue;
        }
        // The share of the bill's input these models' tokens are.
        let input: f64 = found.iter().map(|u| u.input).sum();
        let every: f64 = all.iter().map(|u| u.input).sum();
        let share = if every > 0.0 { (input / every).clamp(0.0, 1.0) } else { 0.0 };
        out.push(caching(&found, billed.as_ref().map(|(svc, usage, q)| (svc.clone(), usage.clone(), q * share)), days));
    }
    out
}

/// A finding for `models` (billed as one), with their share of the bill's input when known.
fn caching(models: &[&ModelUse], billed: Option<(String, String, f64)>, days: u32) -> Finding {
    let invocations: f64 = models.iter().map(|u| u.invocations).sum();
    let input: f64 = models.iter().map(|u| u.input).sum();
    let output: f64 = models.iter().map(|u| u.output).sum();
    let monthly = input * per_month(days);
    let per_call = if invocations > 0.0 { input / invocations } else { 0.0 };
    let names: Vec<&str> = models.iter().map(|u| u.model.as_str()).collect();
    let resource = match names.as_slice() {
        [one] => one.to_string(),
        [first, rest @ ..] => format!("{first} (and {} more billed as it)", rest.len()),
        [] => String::new(),
    };
    let mut f = Finding::new(
        MODULE,
        "",
        resource,
        format!("{} input tokens a month ({} a call on average), none from the prompt cache", qty(monthly), qty(per_call)),
        "Cache the prompt's fixed part (system prompt, tools, documents) with cache points: repeated input then bills at a fraction",
        format!("up to {} input tokens a month", qty(monthly)),
        Impact::of(monthly, TOKENS_MEDIUM, TOKENS_HIGH),
    )
    .kind("bedrock-caching")
    .score(monthly / TOKENS_HIGH)
    .evidence(format!("{} invocations, {} output tokens a month", qty(invocations * per_month(days)), qty(output * per_month(days))))
    .caveat("Caching saves only on the part of the prompt that repeats from call to call, and writing to the cache costs a little more than input: the figure is all the input these calls bill, the most it could save, never reached.");
    if names.len() > 1 {
        f = f.evidence(format!("model ids billed as one: {}", names.join(", ")));
    }
    match billed {
        Some((service, usage, quantity)) if quantity > 0.0 => {
            f = f.ce_service(service).billed_at_most(&usage, quantity).evidence(format!("input on the bill as {usage}, these calls' share of it"));
        }
        _ => f = f.caveat("The bill does not show this model's input tokens under a name dcli recognises: not priced."),
    }
    f
}

/// Invocation logging that writes prompts and answers to CloudWatch Logs.
fn logging(scan: &Scan) -> aws::Result<Option<Finding>> {
    let Some(config) = bedrock::logging(scan.aws)? else { return Ok(None) };
    let Some(group) = config.cloud_watch_config.as_ref().map(|c| c.log_group_name.clone()).filter(|g| !g.is_empty()) else {
        return Ok(None);
    };
    if !config.text_data_delivery_enabled {
        return Ok(None);
    }
    let q = [query("lg", "AWS/Logs", "IncomingBytes", &[("LogGroupName", &group)], "Sum", DAY)];
    let bytes = metric_data(scan.aws, &q, scan.days)?.first().map(total).unwrap_or(0.0);
    let gb = bytes / GB * per_month(scan.days);
    if gb < 1.0 {
        return Ok(None);
    }
    let mut f = Finding::new(
        MODULE,
        "",
        format!("invocation logs ({group})"),
        format!("every prompt and answer goes to CloudWatch Logs: {} GB a month", qty(gb)),
        if config.s3_config.as_ref().is_some_and(|s| !s.bucket_name.is_empty()) {
            "Keep the copy in S3 and stop the one in CloudWatch Logs (or stop logging text)".to_string()
        } else {
            "Send invocation logs to S3 instead of CloudWatch Logs (or stop logging text)".to_string()
        },
        format!("up to {} GB ingested a month", qty(gb)),
        Impact::of(gb, LOG_GB_MEDIUM, LOG_GB_HIGH),
    )
    .kind("bedrock-logging")
    .score(gb / LOG_GB_HIGH)
    .ce_service("AmazonCloudWatch")
    // Invocation logs are ingested as standard logs.
    .billed_at_most(billing::LOGS_INGESTED, gb)
    .caveat("Prompts often hold personal or confidential data: logging them whole has a cost beyond the bill.");
    if !config.s3_config.as_ref().is_some_and(|s| !s.bucket_name.is_empty()) {
        f = f.caveat("Storing them in S3 costs a little per GB too, far less than ingesting them.");
    }
    Ok(Some(f))
}

/// The model a provisioned throughput serves: `anthropic.claude-…` from its foundation model's
/// ARN (`arn:aws:bedrock:…::foundation-model/anthropic.claude-…`).
fn provisioned_model(p: &ProvisionedThroughput) -> String {
    let arn = if p.foundation_model_arn.is_empty() { &p.model_arn } else { &p.foundation_model_arn };
    arn.rsplit_once('/').map_or(arn.as_str(), |(_, m)| m).to_string()
}

/// The usage type the bill uses for provisioned throughput of `model` (hours of model units):
/// in the model's own service, or under `Amazon Bedrock` naming the model.
fn provisioned_bill(s: &Snapshot, model: &str) -> Option<(String, String)> {
    let words = model_words(model);
    if words.is_empty() {
        return None;
    }
    let compact = words.concat();
    s.usage
        .iter()
        .filter(|u| u.service.contains("Bedrock") && u.unit.to_ascii_lowercase().contains('h'))
        .filter(|u| {
            let name = billing::split_region(&u.usage_type).1.to_ascii_lowercase();
            if !name.contains("provisioned") {
                return false;
            }
            if u.service == "Amazon Bedrock" {
                name.replace(['-', '_', '.'], "").contains(&compact)
            } else {
                let sw = service_words(&u.service);
                words.iter().all(|w| sw.contains(w)) && sw.len() == words.len()
            }
        })
        .max_by(|a, b| a.cost.total_cmp(&b.cost))
        .map(|u| (u.service.clone(), billing::split_region(&u.usage_type).1.to_string()))
}

/// Provisioned throughput no request seems to have used over the window. Bedrock's metrics
/// for it are read by its ARN as the model id; calls reported under another id would not show,
/// so the finding says so and is a ceiling, not a firm saving.
fn idle_provisioned(p: &ProvisionedThroughput, costs: Option<&Snapshot>, days: u32) -> Finding {
    let hours = MONTH / 3600.0 * p.model_units.max(1) as f64;
    let committed = !p.commitment_duration.is_empty();
    let model = provisioned_model(p);
    let mut f = Finding::new(
        MODULE,
        "",
        p.provisioned_model_name.clone(),
        format!(
            "{} model unit{} provisioned, no invocation seen in {}",
            p.model_units,
            if p.model_units == 1 { "" } else { "s" },
            super::days_label(days)
        ),
        if committed {
            "Check nothing calls it, and do not renew it when the commitment ends (it cannot be deleted for less before then)".to_string()
        } else {
            "Check nothing calls it (its invocations in the console), then delete it: it is billed every hour it exists".to_string()
        },
        format!("{} model-unit hours a month", qty(hours)),
        Impact::Medium,
    )
    .kind("bedrock-provisioned")
    .score(1.0)
    .evidence(format!("model {model}"))
    .caveat("Invocations were read by the provisioned model's ARN: calls reported under another id would not show here.");
    if let Some(end) = &p.commitment_expiration_time {
        f = f.evidence(format!("committed ({}) until {}", p.commitment_duration, end.get(..10).unwrap_or(end)));
    }
    if committed {
        // A commitment is paid to its end whatever happens: nothing saves before then.
        return f.caveat("Committed: nothing is saved before the commitment ends, so it is not priced.");
    }
    match costs.and_then(|s| provisioned_bill(s, &model)) {
        Some((service, usage)) => f.ce_service(service).billed_at_most(&usage, hours),
        None => f.caveat("The bill does not show this model's provisioned throughput under a name dcli recognises: not priced."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::costs::UsageCost;

    fn usage(service: &str, usage_type: &str, quantity: f64, cost: f64) -> UsageCost {
        UsageCost { service: service.into(), usage_type: usage_type.into(), quantity, cost, unit: "Units".into() }
    }

    fn bill() -> Snapshot {
        Snapshot {
            usage: vec![
                usage("Claude Sonnet 4 (Amazon Bedrock Edition)", "USE1-MP:USE1_InputTokenCount-Units", 900_000.0, 2700.0),
                usage("Claude Sonnet 4 (Amazon Bedrock Edition)", "USE1-MP:USE1_OutputTokenCount-Units", 50_000.0, 750.0),
                usage("Claude Sonnet 4.5 (Amazon Bedrock Edition)", "USE1-MP:USE1_InputTokenCount-Units", 10.0, 0.03),
                usage("Amazon Bedrock", "USE1-NovaLite-input-tokens", 5_000.0, 0.3),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn model_ids_name_their_model() {
        assert_eq!(model_words("us.anthropic.claude-sonnet-4-20250514-v1:0"), ["claude", "sonnet", "4"]);
        assert_eq!(model_words("anthropic.claude-3-5-haiku-20241022-v1:0"), ["claude", "3", "5", "haiku"]);
        assert_eq!(model_words("amazon.nova-lite-v1:0"), ["nova", "lite"]);
        assert_eq!(service_words("Claude Sonnet 4.5 (Amazon Bedrock Edition)"), ["claude", "sonnet", "4", "5"]);
        assert!(caching_supported("us.anthropic.claude-sonnet-4-20250514-v1:0") && !caching_supported("anthropic.claude-3-haiku-20240307-v1:0"));
    }

    #[test]
    fn the_bill_is_found_by_the_model_name() {
        let s = bill();
        let (service, usage, q) = bill_for_model(&s, "us.anthropic.claude-sonnet-4-20250514-v1:0").unwrap();
        assert_eq!((service.as_str(), usage.as_str()), ("Claude Sonnet 4 (Amazon Bedrock Edition)", "MP:USE1_InputTokenCount-Units"));
        assert!((q - 900_000.0 * 30.4375 / 30.0).abs() < 1e-6);
        let (service, usage, _) = bill_for_model(&s, "amazon.nova-lite-v1:0").unwrap();
        assert_eq!((service.as_str(), usage.as_str()), ("Amazon Bedrock", "NovaLite-input-tokens"));
        assert!(bill_for_model(&s, "meta.llama3-70b-instruct-v1:0").is_none());
    }

    #[test]
    fn long_prompts_without_cache_are_found() {
        let u = ModelUse {
            model: "us.anthropic.claude-sonnet-4-20250514-v1:0".into(),
            invocations: 100_000.0,
            input: 400_000_000.0,
            output: 20_000_000.0,
            ..Default::default()
        };
        let found = caching_findings(std::slice::from_ref(&u), Some(&bill()), 14);
        let f = &found[0];
        assert_eq!((f.kind, f.ce_service.as_deref()), ("bedrock-caching", Some("Claude Sonnet 4 (Amazon Bedrock Edition)")));
        assert!(f.billed[0].at_most);
        assert_eq!(f.impact, Impact::High);
        assert!(caching_findings(&[ModelUse { cache_read: 1.0, ..u.clone() }], None, 14).is_empty(), "it caches already");
        assert!(caching_findings(&[ModelUse { input: 50_000_000.0, ..u.clone() }], None, 14).is_empty(), "short prompts");
        let unpriced = caching_findings(std::slice::from_ref(&u), None, 14);
        assert!(unpriced[0].billed.is_empty() && unpriced[0].caveats.iter().any(|c| c.contains("not priced")));
        // An inference profile and its foundation model are billed as one: one finding, and
        // a cached model on the same bill keeps its share.
        let base = ModelUse { model: "anthropic.claude-sonnet-4-20250514-v1:0".into(), ..u.clone() };
        let cached = ModelUse { model: "eu.anthropic.claude-sonnet-4-20250514-v1:0".into(), cache_read: 1.0, ..u.clone() };
        let found = caching_findings(&[u.clone(), base, cached], Some(&bill()), 14);
        assert_eq!(found.len(), 1);
        let bill_q = 900_000.0 * 30.4375 / 30.0;
        assert!((found[0].billed[0].quantity - bill_q * 2.0 / 3.0).abs() < 1e-6, "{}", found[0].billed[0].quantity);
        assert!(found[0].evidence.iter().any(|e| e.contains("billed as one")));
    }

    #[test]
    fn idle_provisioned_throughput() {
        let p = ProvisionedThroughput { provisioned_model_name: "pt-1".into(), model_units: 2, status: "InService".into(), ..Default::default() };
        let p = ProvisionedThroughput { foundation_model_arn: "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-sonnet-4-20250514-v1:0".into(), ..p };
        let mut s = bill();
        s.usage.push(UsageCost { unit: "Hrs".into(), ..usage("Claude Sonnet 4 (Amazon Bedrock Edition)", "USE1-ProvisionedThroughput-Units", 720.0, 30_000.0) });
        s.usage.push(UsageCost { unit: "Hrs".into(), ..usage("Claude Opus 4 (Amazon Bedrock Edition)", "USE1-ProvisionedThroughput-Units", 720.0, 90_000.0) });
        let f = idle_provisioned(&p, Some(&s), 14);
        assert_eq!(f.kind, "bedrock-provisioned");
        assert!(f.recommendation.contains("then delete it"));
        // Its own model's rate, not the dearest one on the bill, and a ceiling.
        assert_eq!(f.ce_service.as_deref(), Some("Claude Sonnet 4 (Amazon Bedrock Edition)"));
        assert!(f.billed[0].at_most && f.caveats.iter().any(|c| c.contains("another id")));
        let committed = ProvisionedThroughput { commitment_duration: "SixMonths".into(), commitment_expiration_time: Some("2027-01-01T00:00:00Z".into()), ..p.clone() };
        let f = idle_provisioned(&committed, Some(&s), 14);
        assert!(f.recommendation.contains("do not renew") && f.evidence.iter().any(|e| e.contains("2027-01-01")));
        assert!(f.billed.is_empty(), "committed: nothing saves before the end");
        // Odd text in the date never panics.
        let odd = ProvisionedThroughput { commitment_duration: "OneMonth".into(), commitment_expiration_time: Some("2027-ñ".into()), ..p };
        assert!(idle_provisioned(&odd, None, 14).evidence.iter().any(|e| e.contains("2027-ñ")));
    }
}

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
    let mut out: Vec<Finding> = usage(scan.aws, &models, scan.days)?
        .iter()
        .filter_map(|u| caching(u, costs.as_ref(), scan.days))
        .collect();
    if let Some(f) = logging(scan)? {
        out.push(f);
    }
    let provisioned: Vec<ProvisionedThroughput> = bedrock::provisioned(scan.aws)?.into_iter().filter(|p| p.status == "InService").collect();
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

/// A model with long prompts and nothing cached.
fn caching(u: &ModelUse, costs: Option<&Snapshot>, days: u32) -> Option<Finding> {
    if !caching_supported(&u.model) || u.invocations <= 0.0 || u.cache_read + u.cache_write > 0.0 {
        return None;
    }
    let monthly = u.input * per_month(days);
    let per_call = u.input / u.invocations;
    if monthly < TOKENS_MEDIUM || per_call < MIN_PROMPT {
        return None;
    }
    let mut f = Finding::new(
        MODULE,
        "",
        u.model.clone(),
        format!("{} input tokens a month ({} a call on average), none from the prompt cache", qty(monthly), qty(per_call)),
        "Cache the prompt's fixed part (system prompt, tools, documents) with cache points: repeated input then bills at a fraction",
        format!("up to {} input tokens a month", qty(monthly)),
        Impact::of(monthly, TOKENS_MEDIUM, TOKENS_HIGH),
    )
    .kind("bedrock-caching")
    .score(monthly / TOKENS_HIGH)
    .evidence(format!("{} invocations, {} output tokens a month", qty(u.invocations * per_month(days)), qty(u.output * per_month(days))))
    .caveat("Caching saves only on the part of the prompt that repeats from call to call, and writing to the cache costs a little more than input: the figure is all the input the model bills, the most it could save, never reached.");
    match costs.and_then(|s| bill_for_model(s, &u.model)) {
        Some((service, usage, quantity)) => {
            f = f.ce_service(service).billed_at_most(&usage, quantity).evidence(format!("input on the bill as {usage}"));
        }
        None => f = f.caveat("The bill does not show this model's input tokens under a name dcli recognises: not priced."),
    }
    Some(f)
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

/// Provisioned throughput no request used over the window.
fn idle_provisioned(p: &ProvisionedThroughput, costs: Option<&Snapshot>, days: u32) -> Finding {
    let hours = MONTH / 3600.0 * p.model_units.max(1) as f64;
    let committed = !p.commitment_duration.is_empty();
    let mut f = Finding::new(
        MODULE,
        "",
        p.provisioned_model_name.clone(),
        format!("{} model unit{} provisioned, no invocation in {}", p.model_units, if p.model_units == 1 { "" } else { "s" }, super::days_label(days)),
        if committed {
            "Do not renew it when the commitment ends (it cannot be deleted for less before then)".to_string()
        } else {
            "Delete it: it is billed every hour it exists".to_string()
        },
        format!("{} model-unit hours a month", qty(hours)),
        Impact::High,
    )
    .kind("bedrock-provisioned")
    .score(2.0)
    .evidence(format!("model {}", if p.foundation_model_arn.is_empty() { &p.model_arn } else { &p.foundation_model_arn }));
    if let Some(end) = &p.commitment_expiration_time {
        f = f.evidence(format!("committed ({}) until {}", p.commitment_duration, &end[..end.len().min(10)]));
    }
    // Provisioned throughput's usage type, when the bill names one (hours of model units).
    let billed = costs.and_then(|s| {
        s.usage
            .iter()
            .filter(|u| u.service.contains("Bedrock") && u.usage_type.to_ascii_lowercase().contains("provisioned"))
            .max_by(|a, b| a.cost.total_cmp(&b.cost))
            .map(|u| (u.service.clone(), billing::split_region(&u.usage_type).1.to_string(), u.unit.to_ascii_lowercase()))
    });
    match billed {
        Some((service, usage, unit)) if unit.contains("h") => {
            f = f.ce_service(service).billed(&usage, hours);
            if committed {
                f = f.caveat("Committed: it saves only from the end of the commitment.");
            }
        }
        _ => f = f.caveat("The bill does not show provisioned throughput under a name dcli recognises: not priced."),
    }
    f
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
        let f = caching(&u, Some(&bill()), 14).unwrap();
        assert_eq!((f.kind, f.ce_service.as_deref()), ("bedrock-caching", Some("Claude Sonnet 4 (Amazon Bedrock Edition)")));
        assert!(f.billed[0].at_most);
        assert_eq!(f.impact, Impact::High);
        assert!(caching(&ModelUse { cache_read: 1.0, ..u.clone() }, None, 14).is_none(), "it caches already");
        assert!(caching(&ModelUse { input: 50_000_000.0, ..u.clone() }, None, 14).is_none(), "short prompts");
        let unpriced = caching(&u, None, 14).unwrap();
        assert!(unpriced.billed.is_empty() && unpriced.caveats.iter().any(|c| c.contains("not priced")));
    }

    #[test]
    fn idle_provisioned_throughput() {
        let p = ProvisionedThroughput { provisioned_model_name: "pt-1".into(), model_units: 2, status: "InService".into(), ..Default::default() };
        let f = idle_provisioned(&p, None, 14);
        assert_eq!(f.kind, "bedrock-provisioned");
        assert!(f.recommendation.starts_with("Delete it"));
        let committed = ProvisionedThroughput { commitment_duration: "SixMonths".into(), commitment_expiration_time: Some("2027-01-01T00:00:00Z".into()), ..p };
        let f = idle_provisioned(&committed, None, 14);
        assert!(f.recommendation.contains("Do not renew") && f.evidence.iter().any(|e| e.contains("2027-01-01")));
    }
}

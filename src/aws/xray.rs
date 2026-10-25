//! AWS X-Ray (restJson1), read only: the sampling rules that decide how many traces are
//! recorded, and the groups with Insights on. Reading traces (GetTraceSummaries) is billed per
//! trace, so nothing here reads them.

use serde::Deserialize;
use serde_json::json;

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service { endpoint: "xray", signing_name: "xray", target_prefix: "" };

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SamplingRule {
    pub rule_name: String,
    pub priority: i64,
    /// The share of matching requests recorded after the reservoir, 0 to 1.
    pub fixed_rate: f64,
    /// Requests recorded each second before the fixed rate applies.
    pub reservoir_size: i64,
    pub service_name: String,
    pub service_type: String,
    pub host: String,
    #[serde(rename = "HTTPMethod")]
    pub http_method: String,
    #[serde(rename = "URLPath")]
    pub url_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Group {
    pub group_name: String,
    pub filter_expression: String,
    pub insights_configuration: Option<InsightsConfiguration>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct InsightsConfiguration {
    pub insights_enabled: bool,
    pub notifications_enabled: bool,
}

/// Reads every page of a POST listing whose body takes `NextToken`.
fn pages<T, Out: serde::de::DeserializeOwned>(aws: &Aws, path: &str, mut take: impl FnMut(Out) -> (Vec<T>, Option<String>)) -> Result<Vec<T>> {
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..20 {
        let body = match &token {
            Some(t) => json!({ "NextToken": t }),
            None => json!({}),
        };
        let (items, next) = take(aws.rest(&SERVICE, "POST", &[path], &[], Some(&body))?);
        all.extend(items);
        match next.filter(|t| !t.is_empty()) {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    Ok(all)
}

pub fn sampling_rules(aws: &Aws) -> Result<Vec<SamplingRule>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Record {
        sampling_rule: SamplingRule,
    }
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        sampling_rule_records: Vec<Record>,
        next_token: Option<String>,
    }
    pages(aws, "GetSamplingRules", |o: Out| (o.sampling_rule_records.into_iter().map(|r| r.sampling_rule).collect(), o.next_token))
}

pub fn groups(aws: &Aws) -> Result<Vec<Group>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        groups: Vec<Group>,
        next_token: Option<String>,
    }
    pages(aws, "Groups", |o: Out| (o.groups, o.next_token))
}

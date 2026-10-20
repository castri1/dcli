//! EventBridge (awsJson1_1): buses, rules, targets, TestEventPattern, PutEvents.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service { endpoint: "events", signing_name: "events", target_prefix: "AWSEvents" };

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct EventBus {
    pub name: String,
    pub arn: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Rule {
    pub name: String,
    pub arn: String,
    /// JSON text.
    pub event_pattern: Option<String>,
    pub schedule_expression: Option<String>,
    /// `ENABLED`, `DISABLED`, `ENABLED_WITH_ALL_CLOUDTRAIL_MANAGEMENT_EVENTS`.
    pub state: String,
    pub description: Option<String>,
    pub event_bus_name: Option<String>,
    pub managed_by: Option<String>,
    pub role_arn: Option<String>,
    pub created_by: Option<String>,
}

impl Rule {
    pub fn enabled(&self) -> bool {
        self.state.starts_with("ENABLED")
    }

    /// `schedule rate(1 day)` or `source=orders.service · detail-type=OrderPaid`.
    pub fn summary(&self) -> String {
        if let Some(s) = self.schedule_expression.as_deref().filter(|s| !s.is_empty()) {
            return format!("schedule {s}");
        }
        let Some(pattern) = self.event_pattern.as_deref().and_then(|p| serde_json::from_str::<Value>(p).ok()) else {
            return String::new();
        };
        let mut parts = Vec::new();
        for key in ["source", "detail-type"] {
            if let Some(v) = pattern.get(key).and_then(|v| v.as_array()).and_then(|a| a.first()).map(sample_scalar) {
                parts.push(format!("{key}={}", v.as_str().map_or_else(|| v.to_string(), str::to_string)));
            }
        }
        if parts.is_empty() { "pattern".into() } else { parts.join(" · ") }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Target {
    pub id: String,
    pub arn: String,
    pub role_arn: Option<String>,
    pub input: Option<String>,
    pub input_path: Option<String>,
    pub input_transformer: Option<InputTransformer>,
    pub dead_letter_config: Option<DeadLetterConfig>,
    pub retry_policy: Option<RetryPolicy>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct InputTransformer {
    pub input_paths_map: BTreeMap<String, String>,
    pub input_template: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DeadLetterConfig {
    pub arn: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct RetryPolicy {
    pub maximum_retry_attempts: Option<i64>,
    pub maximum_event_age_in_seconds: Option<i64>,
}

impl Target {
    /// `lambda`, `states`, `sqs`… (the ARN's service).
    pub fn service(&self) -> &str {
        self.arn.split(':').nth(2).unwrap_or("")
    }

    /// Readable service name for the list.
    pub fn kind(&self) -> &str {
        match self.service() {
            "lambda" => "Lambda",
            "states" => "Step Functions",
            "sqs" => "SQS",
            "sns" => "SNS",
            "events" => "EventBridge",
            "logs" => "CloudWatch Logs",
            "kinesis" => "Kinesis",
            "firehose" => "Firehose",
            "ecs" => "ECS",
            "codepipeline" => "CodePipeline",
            "codebuild" => "CodeBuild",
            "states-express" => "Step Functions",
            other => other,
        }
    }

    /// The resource name at the end of the ARN (`function:name`, `stateMachine:name`, `queue`).
    pub fn name(&self) -> String {
        let parts: Vec<&str> = self.arn.split(':').collect();
        match self.service() {
            // arn:aws:lambda:region:account:function:name[:qualifier]
            "lambda" => parts.get(6).copied().unwrap_or(&self.arn).to_string(),
            // arn:aws:logs:region:account:log-group:/aws/x[:*]
            "logs" => parts.get(6).copied().unwrap_or(&self.arn).to_string(),
            _ => {
                let last = parts.last().copied().unwrap_or(&self.arn);
                last.rsplit('/').next().unwrap_or(last).to_string()
            }
        }
    }

    /// What the target receives: the whole event, a constant, a path or a template.
    pub fn input_summary(&self) -> String {
        if let Some(i) = self.input.as_deref().filter(|i| !i.is_empty()) {
            return format!("constant {}", crate::util::ellipsize(i, 60));
        }
        if let Some(p) = self.input_path.as_deref().filter(|p| !p.is_empty()) {
            return format!("path {p}");
        }
        if let Some(t) = &self.input_transformer {
            return format!("template {}", crate::util::ellipsize(&t.input_template.split_whitespace().collect::<Vec<_>>().join(" "), 60));
        }
        "the whole event".into()
    }
}

/// One `PutEvents` entry's outcome.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PutResult {
    pub event_id: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

pub fn list_event_buses(aws: &Aws) -> Result<Vec<EventBus>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        event_buses: Vec<EventBus>,
        next_token: Option<String>,
    }
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut input = json!({ "Limit": 100 });
        if let Some(t) = &token {
            input["NextToken"] = json!(t);
        }
        let out: Out = aws.call(&SERVICE, "ListEventBuses", &input)?;
        all.extend(out.event_buses);
        match out.next_token.filter(|t| !t.is_empty()) {
            Some(t) => token = Some(t),
            None => return Ok(all),
        }
    }
}

pub fn list_rules(aws: &Aws, bus: &str) -> Result<Vec<Rule>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        rules: Vec<Rule>,
        next_token: Option<String>,
    }
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut input = json!({ "EventBusName": bus, "Limit": 100 });
        if let Some(t) = &token {
            input["NextToken"] = json!(t);
        }
        let out: Out = aws.call(&SERVICE, "ListRules", &input)?;
        all.extend(out.rules);
        match out.next_token.filter(|t| !t.is_empty()) {
            Some(t) => token = Some(t),
            None => return Ok(all),
        }
    }
}

pub fn describe_rule(aws: &Aws, bus: &str, name: &str) -> Result<Rule> {
    aws.call(&SERVICE, "DescribeRule", &json!({ "Name": name, "EventBusName": bus }))
}

pub fn list_targets(aws: &Aws, bus: &str, rule: &str) -> Result<Vec<Target>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        targets: Vec<Target>,
        next_token: Option<String>,
    }
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut input = json!({ "Rule": rule, "EventBusName": bus, "Limit": 100 });
        if let Some(t) = &token {
            input["NextToken"] = json!(t);
        }
        let out: Out = aws.call(&SERVICE, "ListTargetsByRule", &input)?;
        all.extend(out.targets);
        match out.next_token.filter(|t| !t.is_empty()) {
            Some(t) => token = Some(t),
            None => return Ok(all),
        }
    }
}

/// Whether `event` (a full envelope, see [`envelope`]) matches `pattern`.
pub fn test_event_pattern(aws: &Aws, pattern: &str, event: &Value) -> Result<bool> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        result: bool,
    }
    let input = json!({ "EventPattern": pattern, "Event": event.to_string() });
    let out: Out = aws.call(&SERVICE, "TestEventPattern", &input)?;
    Ok(out.result)
}

/// Sends one event; `detail` is a JSON object as text.
pub fn put_event(aws: &Aws, bus: &str, source: &str, detail_type: &str, detail: &str) -> Result<PutResult> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        entries: Vec<PutResult>,
    }
    let input = json!({ "Entries": [{
        "EventBusName": bus,
        "Source": source,
        "DetailType": detail_type,
        "Detail": detail,
    }] });
    let out: Out = aws.call(&SERVICE, "PutEvents", &input)?;
    Ok(out.entries.into_iter().next().unwrap_or_default())
}

/// The event as EventBridge delivers it, for TestEventPattern.
pub fn envelope(source: &str, detail_type: &str, detail: Value, account: &str, region: &str) -> Value {
    json!({
        "id": crate::util::request_token(),
        "version": "0",
        "account": account,
        "time": crate::util::rfc3339(crate::util::now_secs() as i64),
        "region": region,
        "resources": [],
        "source": source,
        "detail-type": detail_type,
        "detail": detail,
    })
}

/// The account in an ARN (`arn:aws:events:region:ACCOUNT:rule/…`).
pub fn account_of(arn: &str) -> Option<&str> {
    arn.split(':').nth(4).filter(|a| !a.is_empty())
}

/// A value that satisfies one element of a pattern array: the element itself, or an
/// example for content filters (`prefix`, `numeric`, `exists`…).
fn sample_scalar(element: &Value) -> Value {
    let Some(filter) = element.as_object() else { return element.clone() };
    let Some((op, arg)) = filter.iter().next() else { return Value::Null };
    match op.as_str() {
        "prefix" | "suffix" | "equals-ignore-case" => match arg {
            Value::Object(o) => o.values().next().cloned().unwrap_or(Value::Null),
            other => other.clone(),
        },
        "wildcard" => Value::String(arg.as_str().unwrap_or("").replace('*', "x")),
        "numeric" => {
            // [">", 100, "<=", 200] → a number inside the range
            let ops = arg.as_array().cloned().unwrap_or_default();
            let mut low = None;
            let mut high = None;
            for pair in ops.chunks(2) {
                let (Some(o), Some(n)) = (pair.first().and_then(Value::as_str), pair.get(1).and_then(Value::as_f64)) else {
                    continue;
                };
                match o {
                    ">" => low = Some(n + 1.0),
                    ">=" | "=" => low = Some(n),
                    "<" => high = Some(n - 1.0),
                    "<=" => high = Some(n),
                    _ => {}
                }
            }
            let n = low.or(high).unwrap_or(0.0);
            if n.fract() == 0.0 { json!(n as i64) } else { json!(n) }
        }
        "anything-but" => Value::String("something-else".into()),
        "exists" => Value::String("value".into()),
        "cidr" => Value::String(arg.as_str().unwrap_or("10.0.0.0/8").split('/').next().unwrap_or("10.0.0.1").to_string()),
        _ => Value::Null,
    }
}

/// An object whose keys follow the pattern's (nested) keys, each with a value its filter accepts.
fn sample_object(pattern: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, filter) in pattern {
        let value = match filter {
            Value::Object(nested) => Value::Object(sample_object(nested)),
            Value::Array(options) => {
                // {"exists": false} asks for the key to be absent.
                if options.iter().any(|o| o.get("exists") == Some(&Value::Bool(false))) {
                    continue;
                }
                options.first().map(sample_scalar).unwrap_or(Value::Null)
            }
            other => other.clone(),
        };
        out.insert(key.clone(), value);
    }
    out
}

/// `(source, detail-type, detail JSON)` of an event the pattern would match, to prefill a
/// test event. Empty strings and `{}` where the pattern says nothing.
pub fn sample_from_pattern(pattern: &str) -> (String, String, String) {
    let Ok(Value::Object(p)) = serde_json::from_str::<Value>(pattern) else {
        return (String::new(), String::new(), "{}".into());
    };
    let first = |key: &str| {
        p.get(key)
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .map(sample_scalar)
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
            .unwrap_or_default()
    };
    let detail = match p.get("detail") {
        Some(Value::Object(d)) => Value::Object(sample_object(d)),
        _ => json!({}),
    };
    (first("source"), first("detail-type"), detail.to_string())
}

/// An event pattern laid out for reading: objects indented, arrays and short objects on
/// one line (`"source": ["orders.service"]`). Still valid JSON. `None` if it is not JSON.
pub fn pretty_pattern(text: &str) -> Option<String> {
    fn write(v: &Value, indent: usize, out: &mut String) {
        match v {
            Value::Object(map) => {
                let compact = v.to_string();
                if map.is_empty() || (indent > 0 && compact.len() <= 60) {
                    out.push_str(&compact);
                    return;
                }
                out.push_str("{\n");
                for (i, (k, value)) in map.iter().enumerate() {
                    out.push_str(&" ".repeat(indent + 2));
                    out.push_str(&Value::String(k.clone()).to_string());
                    out.push_str(": ");
                    write(value, indent + 2, out);
                    out.push_str(if i + 1 < map.len() { ",\n" } else { "\n" });
                }
                out.push_str(&" ".repeat(indent));
                out.push('}');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let value: Value = serde_json::from_str(text).ok()?;
    let mut out = String::new();
    write(&value, 0, &mut out);
    Some(out)
}

/// Console page of a rule.
pub fn console_url(region: &str, bus: &str, rule: &str) -> String {
    super::console_url(
        region,
        &format!(
            "events/home?region={region}#/eventbus/{}/rules/{}",
            crate::util::uri_encode(bus),
            crate::util::uri_encode(rule)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_match_the_pattern_filters() {
        let pattern = r#"{
            "source": [{"prefix": "orders."}],
            "detail-type": ["OrderPaid", "OrderRefunded"],
            "detail": {
                "amount": [{"numeric": [">", 100, "<=", 500]}],
                "customer": {"tier": ["gold"], "id": [{"exists": true}]},
                "coupon": [{"exists": false}],
                "currency": [{"anything-but": "EUR"}]
            }
        }"#;
        let (source, detail_type, detail) = sample_from_pattern(pattern);
        assert_eq!(source, "orders.");
        assert_eq!(detail_type, "OrderPaid");
        let d: Value = serde_json::from_str(&detail).unwrap();
        assert_eq!(d["amount"], json!(101));
        assert_eq!(d["customer"]["tier"], json!("gold"));
        assert_eq!(d["customer"]["id"], json!("value"));
        assert!(d.get("coupon").is_none());
        assert_eq!(d["currency"], json!("something-else"));
        assert_eq!(sample_from_pattern("not json"), (String::new(), String::new(), "{}".into()));
    }

    #[test]
    fn lays_out_patterns_compactly() {
        let text = r#"{"source":["orders.service"],"detail":{"amount":[{"numeric":[">",100]}],"customer":{"tier":["gold"]}}}"#;
        let pretty = pretty_pattern(text).unwrap();
        assert_eq!(
            pretty,
            "{\n  \"source\": [\"orders.service\"],\n  \"detail\": {\n    \"amount\": [{\"numeric\":[\">\",100]}],\n    \"customer\": {\"tier\":[\"gold\"]}\n  }\n}"
        );
        assert_eq!(serde_json::from_str::<Value>(&pretty).unwrap(), serde_json::from_str::<Value>(text).unwrap());
        assert!(pretty_pattern("nope").is_none());
    }

    #[test]
    fn describes_rules_and_targets() {
        let rule = Rule {
            event_pattern: Some(r#"{"source":["orders.service"],"detail-type":["OrderPaid"]}"#.into()),
            state: "ENABLED".into(),
            ..Default::default()
        };
        assert_eq!(rule.summary(), "source=orders.service · detail-type=OrderPaid");
        assert!(rule.enabled());
        let schedule = Rule { schedule_expression: Some("rate(1 day)".into()), state: "DISABLED".into(), ..Default::default() };
        assert_eq!(schedule.summary(), "schedule rate(1 day)");
        assert!(!schedule.enabled());

        let lambda = Target { arn: "arn:aws:lambda:us-east-1:123:function:payments:live".into(), ..Default::default() };
        assert_eq!((lambda.kind(), lambda.name().as_str()), ("Lambda", "payments"));
        let sfn = Target { arn: "arn:aws:states:us-east-1:123:stateMachine:fulfil".into(), ..Default::default() };
        assert_eq!((sfn.kind(), sfn.name().as_str()), ("Step Functions", "fulfil"));
        let logs = Target { arn: "arn:aws:logs:us-east-1:123:log-group:/aws/events/orders".into(), ..Default::default() };
        assert_eq!(logs.name(), "/aws/events/orders");
        assert_eq!(account_of("arn:aws:events:us-east-1:123456789012:rule/x"), Some("123456789012"));
    }
}

use serde::Deserialize;
use serde_json::json;

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service {
    endpoint: "logs",
    signing_name: "logs",
    target_prefix: "Logs_20140328",
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogEvent {
    pub timestamp: Option<i64>,
    pub message: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogPage {
    pub events: Vec<LogEvent>,
    pub next_forward_token: Option<String>,
}

/// One page of a stream, oldest first. Pass the previous `next_forward_token` to continue;
/// AWS returns the same token again when there is nothing new.
pub fn get_log_events(aws: &Aws, group: &str, stream: &str, token: Option<&str>) -> Result<LogPage> {
    let mut input = json!({
        "logGroupName": group,
        "logStreamName": stream,
        "startFromHead": true,
        "limit": 1000,
    });
    if let Some(t) = token {
        input["nextToken"] = json!(t);
    }
    aws.call(&SERVICE, "GetLogEvents", &input)
}

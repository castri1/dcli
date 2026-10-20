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

/// Every line of a stream, oldest first, reading at most `max_pages` pages of 1,000 events.
/// Returns the lines and whether the stream had more.
pub fn read_all(aws: &Aws, group: &str, stream: &str, max_pages: usize) -> Result<(Vec<String>, bool)> {
    let mut lines = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..max_pages {
        let page = get_log_events(aws, group, stream, token.as_deref())?;
        let done = page.events.is_empty() || page.next_forward_token == token;
        for event in page.events {
            if event.message.trim().is_empty() {
                lines.push(String::new());
            }
            lines.extend(event.message.lines().map(|l| l.trim_end().to_string()));
        }
        if done {
            return Ok((lines, false));
        }
        token = page.next_forward_token;
    }
    Ok((lines, true))
}

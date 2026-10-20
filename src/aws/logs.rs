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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogGroup {
    pub log_group_name: String,
    pub retention_in_days: Option<i64>,
    pub stored_bytes: Option<i64>,
    /// Unix milliseconds.
    pub creation_time: Option<i64>,
}

/// One page of log groups, optionally only those starting with `prefix`.
pub fn describe_log_groups(aws: &Aws, prefix: Option<&str>, token: Option<&str>) -> Result<(Vec<LogGroup>, Option<String>)> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        log_groups: Vec<LogGroup>,
        next_token: Option<String>,
    }
    let mut input = json!({ "limit": 50 });
    if let Some(p) = prefix.filter(|p| !p.is_empty()) {
        input["logGroupNamePrefix"] = json!(p);
    }
    if let Some(t) = token {
        input["nextToken"] = json!(t);
    }
    let out: Out = aws.call(&SERVICE, "DescribeLogGroups", &input)?;
    Ok((out.log_groups, out.next_token.filter(|t| !t.is_empty())))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FilteredEvent {
    pub log_stream_name: String,
    /// Unix milliseconds.
    pub timestamp: i64,
    pub message: String,
    pub event_id: String,
}

/// One page of events of a whole group (every stream), oldest first, from `start_ms`.
/// `pattern` uses the CloudWatch filter syntax (plain words match anywhere).
pub fn filter_log_events(
    aws: &Aws,
    group: &str,
    start_ms: i64,
    pattern: &str,
    token: Option<&str>,
) -> Result<(Vec<FilteredEvent>, Option<String>)> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        events: Vec<FilteredEvent>,
        next_token: Option<String>,
    }
    let mut input = json!({ "logGroupName": group, "startTime": start_ms, "limit": 1000 });
    if !pattern.trim().is_empty() {
        input["filterPattern"] = json!(pattern.trim());
    }
    if let Some(t) = token {
        input["nextToken"] = json!(t);
    }
    let out: Out = aws.call(&SERVICE, "FilterLogEvents", &input)?;
    Ok((out.events, out.next_token.filter(|t| !t.is_empty())))
}

/// `HH:MM:SS.mmm  message` lines for an event (UTC; `with_date` prefixes `YYYY-MM-DD`);
/// continuation lines are indented.
pub fn event_lines(e: &FilteredEvent, with_date: bool) -> Vec<String> {
    let secs = e.timestamp.div_euclid(1000);
    let (y, mo, d, h, m, s) = crate::util::utc_parts(secs);
    let date = if with_date { format!("{y:04}-{mo:02}-{d:02} ") } else { String::new() };
    let stamp = format!("{date}{h:02}:{m:02}:{s:02}.{:03}", e.timestamp.rem_euclid(1000));
    let mut out = Vec::new();
    for (i, line) in e.message.trim_end().lines().enumerate() {
        if i == 0 {
            out.push(format!("{stamp}  {}", line.trim_end()));
        } else {
            out.push(format!("{}  {}", " ".repeat(stamp.len()), line.trim_end()));
        }
    }
    if out.is_empty() {
        out.push(stamp);
    }
    out
}

/// `15m`, `2h`, `1d`, `30s` → seconds.
pub fn parse_since(s: &str) -> Option<i64> {
    let s = s.trim();
    let (number, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = number.parse().ok()?;
    Some(n * match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return None,
    })
}

/// Console page of a log group.
pub fn console_url(region: &str, group: &str) -> String {
    super::console_url(region, &format!("cloudwatch/home?region={region}#logsV2:log-groups/log-group/{}", group.replace('/', "$252F")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_events_and_durations() {
        let e = FilteredEvent { timestamp: 1_440_938_160_123, message: "first\nsecond\n".into(), ..Default::default() };
        assert_eq!(event_lines(&e, false), vec!["12:36:00.123  first".to_string(), "              second".to_string()]);
        assert_eq!(event_lines(&e, true)[0], "2015-08-30 12:36:00.123  first");
        assert_eq!(parse_since("15m"), Some(900));
        assert_eq!(parse_since("2d"), Some(172_800));
        assert_eq!(parse_since("15"), None);
        assert_eq!(parse_since("x"), None);
    }
}

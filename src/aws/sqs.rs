//! SQS (awsJson1_0, `X-Amz-Target: AmazonSQS.<Operation>`): queues with their counts and
//! dead-letter relationships, the messages of dead-letter queues, and redrive (message
//! move tasks). Shared by the SQS module and `dcli sqs`.

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service { endpoint: "sqs", signing_name: "sqs", target_prefix: "AmazonSQS" };

/// Messages read at most when looking into a dead-letter queue.
pub const PEEK_MAX: usize = 50;

#[derive(Debug, Clone, Default)]
pub struct Queue {
    pub url: String,
    pub name: String,
    /// Every attribute (`ApproximateNumberOfMessages`, `RedrivePolicy`…), as SQS returns them.
    pub attributes: HashMap<String, String>,
}

/// Where a queue sends the messages it could not process.
#[derive(Debug, Clone, PartialEq)]
pub struct RedrivePolicy {
    pub dead_letter_arn: String,
    /// Receives before a message moves to the dead-letter queue.
    pub max_receive_count: i64,
}

impl Queue {
    fn new(url: String, attributes: HashMap<String, String>) -> Queue {
        Queue { name: queue_name(&url).to_string(), url, attributes }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attributes.get(name).map(String::as_str).filter(|v| !v.is_empty())
    }

    fn count(&self, name: &str) -> i64 {
        self.attr(name).and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    pub fn arn(&self) -> &str {
        self.attr("QueueArn").unwrap_or_default()
    }

    pub fn fifo(&self) -> bool {
        self.attr("FifoQueue") == Some("true") || self.name.ends_with(".fifo")
    }

    /// Messages ready to be received.
    pub fn visible(&self) -> i64 {
        self.count("ApproximateNumberOfMessages")
    }

    /// Messages received and not yet deleted (being processed).
    pub fn in_flight(&self) -> i64 {
        self.count("ApproximateNumberOfMessagesNotVisible")
    }

    pub fn delayed(&self) -> i64 {
        self.count("ApproximateNumberOfMessagesDelayed")
    }

    pub fn redrive(&self) -> Option<RedrivePolicy> {
        parse_redrive(self.attr("RedrivePolicy")?)
    }

    /// Which queues may use this one as their dead-letter queue, in words.
    pub fn redrive_allow(&self) -> Option<String> {
        let v: Value = serde_json::from_str(self.attr("RedriveAllowPolicy")?).ok()?;
        Some(match v["redrivePermission"].as_str()? {
            "allowAll" => "any queue".into(),
            "denyAll" => "no queue".into(),
            "byQueue" => {
                let names: Vec<&str> = v["sourceQueueArns"].as_array()?.iter().filter_map(Value::as_str).map(queue_name).collect();
                format!("only {}", names.join(", "))
            }
            other => other.to_string(),
        })
    }

    /// `SSE-SQS`, `KMS alias/…` or `none`.
    pub fn encryption(&self) -> String {
        match (self.attr("KmsMasterKeyId"), self.attr("SqsManagedSseEnabled")) {
            (Some(key), _) => format!("KMS {key}"),
            (None, Some("true")) => "SSE-SQS (managed by SQS)".into(),
            _ => "none".into(),
        }
    }
}

fn parse_redrive(text: &str) -> Option<RedrivePolicy> {
    let v: Value = serde_json::from_str(text).ok()?;
    let max = match &v["maxReceiveCount"] {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => s.parse().ok()?,
        _ => return None,
    };
    Some(RedrivePolicy { dead_letter_arn: v["deadLetterTargetArn"].as_str()?.to_string(), max_receive_count: max })
}

/// The queue name of a URL (`https://sqs…/123/orders`), an ARN (`arn:aws:sqs:…:orders`) or
/// a name.
pub fn queue_name(id: &str) -> &str {
    let id = id.trim_end_matches('/');
    let after_slash = id.rsplit('/').next().unwrap_or(id);
    if id.starts_with("arn:") { id.rsplit(':').next().unwrap_or(id) } else { after_slash }
}

/// Every queue URL in the account and region.
pub fn list_queue_urls(aws: &Aws) -> Result<Vec<String>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        queue_urls: Vec<String>,
        next_token: Option<String>,
    }
    let mut urls = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut input = json!({ "MaxResults": 1000 });
        if let Some(t) = &token {
            input["NextToken"] = json!(t);
        }
        let out: Out = aws.call_json10(&SERVICE, "ListQueues", &input)?;
        urls.extend(out.queue_urls);
        match out.next_token.filter(|t| !t.is_empty()) {
            Some(t) => token = Some(t),
            None => return Ok(urls),
        }
    }
}

pub fn queue(aws: &Aws, url: &str) -> Result<Queue> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        attributes: HashMap<String, String>,
    }
    let out: Out = aws.call_json10(&SERVICE, "GetQueueAttributes", &json!({ "QueueUrl": url, "AttributeNames": ["All"] }))?;
    Ok(Queue::new(url.to_string(), out.attributes))
}

/// Changes attributes of a queue (`ReceiveMessageWaitTimeSeconds`…).
pub fn set_attributes(aws: &Aws, url: &str, attributes: &[(&str, &str)]) -> Result<()> {
    let attributes: HashMap<&str, &str> = attributes.iter().copied().collect();
    let _: Value = aws.call_json10(&SERVICE, "SetQueueAttributes", &json!({ "QueueUrl": url, "Attributes": attributes }))?;
    Ok(())
}

/// Every queue with its attributes (read in parallel), by name.
pub fn list_queues(aws: &Aws) -> Result<Vec<Queue>> {
    let urls = list_queue_urls(aws)?;
    let mut queues = Vec::with_capacity(urls.len());
    for chunk in urls.chunks(16) {
        let results: Vec<Result<Queue>> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunk.iter().map(|url| scope.spawn(move || queue(aws, url))).collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(super::AwsError::Network("thread panicked".into())))).collect()
        });
        for r in results {
            queues.push(r?);
        }
    }
    queues.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(queues)
}

/// The URL of a queue given by name, URL or ARN.
pub fn resolve(aws: &Aws, id: &str) -> Result<String> {
    if id.starts_with("https://") || id.starts_with("http://") {
        return Ok(id.to_string());
    }
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        queue_url: String,
    }
    let mut input = json!({ "QueueName": queue_name(id) });
    // arn:aws:sqs:region:account:name → the owner's account.
    if let Some(account) = id.strip_prefix("arn:").and_then(|rest| rest.split(':').nth(3)).filter(|a| !a.is_empty()) {
        input["QueueOwnerAWSAccountId"] = json!(account);
    }
    let out: Out = aws.call_json10(&SERVICE, "GetQueueUrl", &input)?;
    Ok(out.queue_url)
}

/// URLs of the queues that use `url` as their dead-letter queue.
pub fn dead_letter_sources(aws: &Aws, url: &str) -> Result<Vec<String>> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Out {
        #[serde(rename = "queueUrls")]
        queue_urls: Vec<String>,
        #[serde(rename = "NextToken")]
        next_token: Option<String>,
    }
    let mut urls = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut input = json!({ "QueueUrl": url, "MaxResults": 1000 });
        if let Some(t) = &token {
            input["NextToken"] = json!(t);
        }
        let out: Out = aws.call_json10(&SERVICE, "ListDeadLetterSourceQueues", &input)?;
        urls.extend(out.queue_urls);
        match out.next_token.filter(|t| !t.is_empty()) {
            Some(t) => token = Some(t),
            None => return Ok(urls),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MessageAttribute {
    pub data_type: String,
    pub string_value: Option<String>,
    /// Base64.
    pub binary_value: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Message {
    pub message_id: String,
    pub body: String,
    /// `SentTimestamp`, `ApproximateReceiveCount`, `MessageGroupId`…
    pub attributes: HashMap<String, String>,
    pub message_attributes: BTreeMap<String, MessageAttribute>,
}

impl Message {
    fn millis(&self, name: &str) -> Option<f64> {
        self.attributes.get(name)?.parse::<f64>().ok().map(|ms| ms / 1000.0)
    }

    /// Unix seconds.
    pub fn sent(&self) -> Option<f64> {
        self.millis("SentTimestamp")
    }

    pub fn first_received(&self) -> Option<f64> {
        self.millis("ApproximateFirstReceiveTimestamp")
    }

    pub fn receive_count(&self) -> i64 {
        self.attributes.get("ApproximateReceiveCount").and_then(|c| c.parse().ok()).unwrap_or(0)
    }

    pub fn group(&self) -> Option<&str> {
        self.attributes.get("MessageGroupId").map(String::as_str)
    }

    /// The body on one line, envelopes unwrapped: an EventBridge event as its detail type
    /// and detail, an SNS notification as its message. Sanitized: decoding the JSON turns
    /// `\u001b` escapes in the body into real control characters.
    pub fn preview(&self) -> String {
        crate::util::sanitize(&self.unwrapped())
    }

    fn unwrapped(&self) -> String {
        let Ok(v) = serde_json::from_str::<Value>(&self.body) else {
            return self.body.split_whitespace().collect::<Vec<_>>().join(" ");
        };
        if let (Some(kind), Some(detail)) = (v["detail-type"].as_str(), v.get("detail")) {
            return format!("{kind} {detail}");
        }
        if let (Some("Notification"), Some(message)) = (v["Type"].as_str(), v["Message"].as_str()) {
            return message.split_whitespace().collect::<Vec<_>>().join(" ");
        }
        v.to_string()
    }

    /// The body, pretty-printed when it is JSON.
    pub fn pretty_body(&self) -> String {
        crate::util::pretty_json(&self.body).unwrap_or_else(|| self.body.clone())
    }
}

/// Looks at up to `max` messages without consuming them: receives them with a visibility
/// timeout of 0, so they stay available, and repeats (SQS returns a sample per call) until
/// no new ones come. Each look counts as a receive: only for dead-letter queues.
pub fn peek(aws: &Aws, url: &str, max: usize) -> Result<Vec<Message>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        messages: Vec<Message>,
    }
    let input = json!({
        "QueueUrl": url,
        "MaxNumberOfMessages": 10,
        "VisibilityTimeout": 0,
        "WaitTimeSeconds": 0,
        "AttributeNames": ["All"],
        "MessageAttributeNames": ["All"],
    });
    let mut messages: Vec<Message> = Vec::new();
    let mut idle = 0;
    for _ in 0..max.div_ceil(10) * 3 {
        let out: Out = aws.call_json10(&SERVICE, "ReceiveMessage", &input)?;
        let before = messages.len();
        for m in out.messages {
            if messages.len() < max && !messages.iter().any(|seen| seen.message_id == m.message_id) {
                messages.push(m);
            }
        }
        idle = if messages.len() == before { idle + 1 } else { 0 };
        if messages.len() >= max || idle >= 2 {
            break;
        }
    }
    messages.sort_by(|a, b| a.sent().unwrap_or(0.0).total_cmp(&b.sent().unwrap_or(0.0)));
    Ok(messages)
}

/// A redrive: messages moving from a dead-letter queue to their source queues (or another).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MoveTask {
    /// Only while it runs.
    pub task_handle: Option<String>,
    /// `RUNNING`, `COMPLETED`, `CANCELLING`, `CANCELLED`, `FAILED`.
    pub status: String,
    pub source_arn: String,
    pub destination_arn: Option<String>,
    pub approximate_number_of_messages_moved: i64,
    pub approximate_number_of_messages_to_move: Option<i64>,
    pub failure_reason: Option<String>,
    /// Unix milliseconds.
    pub started_timestamp: Option<i64>,
}

impl MoveTask {
    pub fn running(&self) -> bool {
        matches!(self.status.as_str(), "RUNNING" | "CANCELLING")
    }

    pub fn started(&self) -> Option<f64> {
        self.started_timestamp.map(|ms| ms as f64 / 1000.0)
    }

    /// `the source queues` or the destination queue's name.
    pub fn destination(&self) -> String {
        self.destination_arn.as_deref().map_or_else(|| "their source queues".into(), |d| queue_name(d).to_string())
    }
}

/// Starts moving the messages of the dead-letter queue `source_arn` back to their source
/// queues, or to `destination_arn`. Returns the task handle.
pub fn start_move(aws: &Aws, source_arn: &str, destination_arn: Option<&str>) -> Result<String> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        task_handle: String,
    }
    let mut input = json!({ "SourceArn": source_arn });
    if let Some(d) = destination_arn {
        input["DestinationArn"] = json!(d);
    }
    let out: Out = aws.call_json10(&SERVICE, "StartMessageMoveTask", &input)?;
    Ok(out.task_handle)
}

/// The recent redrives of a dead-letter queue, newest first.
pub fn list_moves(aws: &Aws, source_arn: &str) -> Result<Vec<MoveTask>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        results: Vec<MoveTask>,
    }
    let out: Out = aws.call_json10(&SERVICE, "ListMessageMoveTasks", &json!({ "SourceArn": source_arn, "MaxResults": 10 }))?;
    Ok(out.results)
}

/// Stops a redrive; returns how many messages it had moved.
pub fn cancel_move(aws: &Aws, task_handle: &str) -> Result<i64> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        approximate_number_of_messages_moved: i64,
    }
    let out: Out = aws.call_json10(&SERVICE, "CancelMessageMoveTask", &json!({ "TaskHandle": task_handle }))?;
    Ok(out.approximate_number_of_messages_moved)
}

/// The age of the oldest message, in seconds (CloudWatch, the last 15 minutes).
pub fn oldest_age(aws: &Aws, name: &str) -> Result<Option<f64>> {
    use super::metrics::{Query, get_metric_data};
    let now = crate::util::now_secs();
    let query = Query {
        id: "age".into(),
        namespace: "AWS/SQS".into(),
        name: "ApproximateAgeOfOldestMessage".into(),
        dimensions: vec![("QueueName".into(), name.into())],
        stat: "Maximum".into(),
        period: 60,
    };
    let series = get_metric_data(aws, &[query], now - 900.0, now)?;
    Ok(series.first().and_then(|s| s.points.last()).map(|p| p.1))
}

pub fn console_url(region: &str, url: &str) -> String {
    super::console_url(region, &format!("sqs/v3/home?region={region}#/queues/{}", crate::util::uri_encode(url)))
}

/// Seconds as `4 days`, `30 s`, `2 h`… for queue settings.
pub fn fmt_seconds(text: &str) -> String {
    let Ok(s) = text.parse::<u64>() else { return text.to_string() };
    match s {
        0 => "0 s".into(),
        s if s % 86400 == 0 => format!("{} day{}", s / 86400, if s / 86400 == 1 { "" } else { "s" }),
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s if s % 60 == 0 && s >= 60 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_cannot_carry_escape_sequences() {
        let m = Message { body: r#"{"Type": "Notification", "Message": "boom \u001b]0;pwned\u0007 done"}"#.into(), ..Default::default() };
        assert!(!m.preview().chars().any(crate::util::is_unsafe), "{:?}", m.preview());
    }

    #[test]
    fn names_and_policies() {
        assert_eq!(queue_name("https://sqs.us-east-1.amazonaws.com/123456789012/orders-dlq"), "orders-dlq");
        assert_eq!(queue_name("arn:aws:sqs:us-east-1:123456789012:payments.fifo"), "payments.fifo");
        assert_eq!(queue_name("orders"), "orders");
        let p = parse_redrive(r#"{"deadLetterTargetArn":"arn:aws:sqs:us-east-1:1:orders-dlq","maxReceiveCount":"5"}"#).unwrap();
        assert_eq!(p, RedrivePolicy { dead_letter_arn: "arn:aws:sqs:us-east-1:1:orders-dlq".into(), max_receive_count: 5 });
        assert_eq!(parse_redrive(r#"{"deadLetterTargetArn":"x","maxReceiveCount":3}"#).unwrap().max_receive_count, 3);
        let q = Queue::new(
            "https://sqs.us-east-1.amazonaws.com/1/orders-dlq".into(),
            HashMap::from([(
                "RedriveAllowPolicy".to_string(),
                r#"{"redrivePermission":"byQueue","sourceQueueArns":["arn:aws:sqs:us-east-1:1:orders"]}"#.to_string(),
            )]),
        );
        assert_eq!(q.redrive_allow().as_deref(), Some("only orders"));
        assert_eq!(fmt_seconds("1209600"), "14 days");
        assert_eq!(fmt_seconds("30"), "30 s");
        assert_eq!(fmt_seconds("300"), "5 min");
        let event = Message { body: r#"{"version":"0","detail-type":"OrderPaid","detail":{"orderId":"ORD-1"}}"#.into(), ..Default::default() };
        assert_eq!(event.preview(), r#"OrderPaid {"orderId":"ORD-1"}"#);
        let plain = Message { body: "line one\n  line two".into(), ..Default::default() };
        assert_eq!(plain.preview(), "line one line two");
    }
}

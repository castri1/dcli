//! SQS: queues polled without long polling. Every ReceiveMessage is a billed request, the
//! empty ones too; long polling waits for messages instead of answering empty at once.

use super::{Finding, Fix, Impact, Scan, metric_data, per_month, percent, qty, query, query_id, total};
use crate::aws::sqs::{self, Queue};
use crate::aws::{self, metrics::Series};

const MODULE: &str = "SQS";
/// Empty receives a month: listed from 1 M, medium impact from 10 M, high from 100 M.
const EMPTY_LOW: f64 = 1e6;
const EMPTY_MEDIUM: f64 = 1e7;
const EMPTY_HIGH: f64 = 1e8;

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let queues = sqs::list_queues(scan.aws)?;
    let short: Vec<&Queue> = queues.iter().filter(|q| wait_seconds(q) == 0).collect();
    if short.is_empty() {
        return Ok(Vec::new());
    }
    let mut queries = Vec::new();
    for (i, q) in short.iter().enumerate() {
        let dims = [("QueueName", q.name.as_str())];
        queries.push(query(query_id("emp", i), "AWS/SQS", "NumberOfEmptyReceives", &dims, "Sum", 86_400));
        queries.push(query(query_id("rcv", i), "AWS/SQS", "NumberOfMessagesReceived", &dims, "Sum", 86_400));
    }
    let series = metric_data(scan.aws, &queries, scan.days)?;
    Ok(short.iter().zip(series.chunks(2)).filter_map(|(q, s)| finding(q, &s[0], &s[1], scan.days)).collect())
}

fn wait_seconds(q: &Queue) -> i64 {
    q.attr("ReceiveMessageWaitTimeSeconds").and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn finding(q: &Queue, empty: &Series, received: &Series, days: u32) -> Option<Finding> {
    let (empty, received) = (total(empty), total(received));
    let monthly = empty * per_month(days);
    if monthly < EMPTY_LOW {
        return None;
    }
    let share = empty / (empty + received).max(1.0);
    Some(
        Finding::new(
            MODULE,
            &q.url,
            &q.name,
            format!("short polling: {} of receives come back empty", percent(share)),
            "Turn on long polling (receives wait up to 20 s for messages)",
            format!("up to −{} requests a month", qty(monthly)),
            Impact::of(monthly, EMPTY_MEDIUM, EMPTY_HIGH),
        )
        .score(monthly / EMPTY_HIGH)
        .evidence(format!("{} empty receives and {} messages received in {days} days", qty(empty), qty(received)))
        .evidence("ReceiveMessageWaitTimeSeconds is 0")
        .caveat("Consumers that pass their own WaitTimeSeconds keep it.")
        .caveat("Lambda triggers long-poll already: empty receives that come from them do not change.")
        .fix(Fix::LongPolling { queue_url: q.url.clone() }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn queue(name: &str, wait: &str) -> Queue {
        let attributes = HashMap::from([("ReceiveMessageWaitTimeSeconds".to_string(), wait.to_string())]);
        Queue { url: format!("https://sqs.us-east-1.amazonaws.com/1/{name}"), name: name.into(), attributes }
    }

    fn sum(v: f64) -> Series {
        Series { points: vec![(0.0, v)], ..Default::default() }
    }

    #[test]
    fn empty_receives_without_long_polling() {
        let q = queue("notifications", "0");
        assert_eq!(wait_seconds(&q), 0);
        assert_eq!(wait_seconds(&queue("orders", "20")), 20);
        // 9.2 M empty receives in 14 days: ≈ 20 M a month.
        let f = finding(&q, &sum(9.2e6), &sum(0.8e6), 14).unwrap();
        assert_eq!(f.summary, "short polling: 92 % of receives come back empty");
        assert_eq!(f.reduction, "up to −20 M requests a month");
        assert_eq!(f.impact, Impact::Medium);
        assert_eq!(f.fix, Some(Fix::LongPolling { queue_url: q.url.clone() }));
        assert_eq!(f.target, q.url);
        // A few empty receives are not worth a finding.
        assert!(finding(&q, &sum(100_000.0), &sum(0.0), 14).is_none());
    }
}

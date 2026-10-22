//! CloudWatch alarms: alarms without data for weeks, usually because what they watch is
//! gone (each alarm, and each metric of a metric math alarm, is billed every month).

use super::{Finding, Impact, Scan};
use crate::aws;
use crate::aws::alarms::{self, Alarm, State, fmt_period};
use crate::util::{fmt_utc, now_secs, show_invisible};

const MODULE: &str = "Alarms";
/// Days without data (or the scan window, if longer) before an alarm is listed.
const SILENT_DAYS: f64 = 14.0;

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let silent = SILENT_DAYS.max(f64::from(scan.days));
    Ok(alarms::describe_alarms(scan.aws)?.iter().filter_map(|a| without_data(a, silent, now_secs())).collect())
}

/// Metrics the alarm reads: one, those of its math, none for a composite alarm.
fn metric_count(a: &Alarm) -> usize {
    if a.metric.is_some() { 1 } else { a.queries.iter().filter(|q| q.metric.is_some()).count() }
}

fn without_data(a: &Alarm, silent_days: f64, now: f64) -> Option<Finding> {
    if a.state != State::InsufficientData {
        return None;
    }
    let since = a.since?;
    let days = ((now - since) / 86_400.0).floor();
    if days < silent_days {
        return None;
    }
    let reduction = match metric_count(a) {
        0 | 1 => "1 alarm a month".to_string(),
        n => format!("1 alarm of {n} metrics a month"),
    };
    let mut finding = Finding::new(
        MODULE,
        &a.name,
        show_invisible(&a.name),
        format!("no data for {days:.0} days"),
        "Delete it if what it watches is gone, or point it at a metric that exists",
        reduction,
        Impact::Low,
    )
    .score(days / 3650.0)
    .evidence(format!("INSUFFICIENT_DATA since {}", fmt_utc(since)))
    .caveat("Alarms on rare events (a nightly job, errors that seldom happen) can lack data and still be useful.");
    if let Some(m) = &a.metric {
        let dims: Vec<String> = m.dimensions.iter().map(|(k, v)| format!("{k}={v}")).collect();
        finding = finding.evidence(format!("watches {} {} ({}) every {}", m.namespace, m.name, dims.join(", "), fmt_period(m.period)));
        if m.period < 60 {
            finding = finding.evidence("high resolution (period under a minute)");
        }
    }
    if let Some(reason) = &a.reason {
        finding = finding.evidence(format!("reason: {reason}"));
    }
    Some(finding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::alarms::MetricStat;

    const NOW: f64 = 1_790_000_000.0;

    fn alarm(state: State, days_ago: f64) -> Alarm {
        Alarm {
            name: "legacy-queue-age".into(),
            arn: String::new(),
            composite: false,
            description: None,
            state,
            reason: Some("Insufficient Data: 1 datapoint was unknown.".into()),
            since: Some(NOW - days_ago * 86_400.0),
            metric: Some(MetricStat {
                namespace: "AWS/SQS".into(),
                name: "ApproximateAgeOfOldestMessage".into(),
                dimensions: vec![("QueueName".into(), "legacy".into())],
                stat: "Maximum".into(),
                period: 10,
            }),
            queries: Vec::new(),
            threshold: Some(600.0),
            comparison: None,
            evaluation_periods: None,
            datapoints_to_alarm: None,
            treat_missing: None,
            threshold_metric: None,
            rule: None,
            actions_enabled: true,
            alarm_actions: Vec::new(),
            ok_actions: Vec::new(),
            insufficient_actions: Vec::new(),
        }
    }

    #[test]
    fn alarms_without_data_for_weeks() {
        let f = without_data(&alarm(State::InsufficientData, 45.0), 14.0, NOW).unwrap();
        assert_eq!(f.summary, "no data for 45 days");
        assert_eq!(f.reduction, "1 alarm a month");
        assert!(f.evidence.contains(&"watches AWS/SQS ApproximateAgeOfOldestMessage (QueueName=legacy) every 10 s".to_string()));
        assert!(f.evidence.contains(&"high resolution (period under a minute)".to_string()));
        assert!(without_data(&alarm(State::InsufficientData, 3.0), 14.0, NOW).is_none());
        assert!(without_data(&alarm(State::Ok, 45.0), 14.0, NOW).is_none());
    }

    #[test]
    fn names_show_their_invisible_characters() {
        let mut a = alarm(State::InsufficientData, 45.0);
        a.name = "orders\u{200b}-age".into();
        let f = without_data(&a, 14.0, NOW).unwrap();
        assert_eq!(f.target, "orders\u{200b}-age");
        assert_ne!(f.resource, f.target);
    }
}

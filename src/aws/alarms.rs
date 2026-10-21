//! CloudWatch alarms (DescribeAlarms, DescribeAlarmHistory) through [`super::cloudwatch`]:
//! metric and composite alarms, a one-line summary of what makes each one fire, and their
//! state changes. Shared by the Alarms module and `dcli alarms`.

use serde::Serialize;

use super::cloudwatch::{SERVICE, VERSION};
use super::metrics::Query;
use super::{Aws, Result};
use crate::util::{parse_rfc3339, xml_text, xml_text_raw};

/// Alarm states, in the order they are listed: firing first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum State {
    #[serde(rename = "ALARM")]
    Alarm,
    #[serde(rename = "INSUFFICIENT_DATA")]
    InsufficientData,
    #[serde(rename = "OK")]
    Ok,
}

impl State {
    pub fn parse(s: &str) -> State {
        match s {
            "ALARM" => State::Alarm,
            "OK" => State::Ok,
            _ => State::InsufficientData,
        }
    }

    pub fn api(self) -> &'static str {
        match self {
            State::Alarm => "ALARM",
            State::InsufficientData => "INSUFFICIENT_DATA",
            State::Ok => "OK",
        }
    }
}

/// A metric read with a statistic over a period.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MetricStat {
    pub namespace: String,
    pub name: String,
    pub dimensions: Vec<(String, String)>,
    /// `Sum`, `Average`, `Maximum`, `p95`…
    pub stat: String,
    pub period: u32,
}

/// One query of a metric math alarm: a metric, or an expression over the others.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MathQuery {
    pub id: String,
    pub expression: Option<String>,
    pub label: Option<String>,
    pub metric: Option<MetricStat>,
    pub return_data: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Alarm {
    /// As AWS has it (sent back to AWS); display it through `show_invisible`.
    pub name: String,
    pub arn: String,
    pub composite: bool,
    pub description: Option<String>,
    pub state: State,
    pub reason: Option<String>,
    /// When it entered its current state (Unix seconds).
    pub since: Option<f64>,
    /// The metric of a single-metric alarm.
    pub metric: Option<MetricStat>,
    /// The queries of a metric math alarm.
    pub queries: Vec<MathQuery>,
    pub threshold: Option<f64>,
    /// `GreaterThanThreshold`…
    pub comparison: Option<String>,
    pub evaluation_periods: Option<u32>,
    pub datapoints_to_alarm: Option<u32>,
    pub treat_missing: Option<String>,
    /// Anomaly detection: the query whose band is the threshold.
    pub threshold_metric: Option<String>,
    /// Composite alarms: `ALARM("a") OR ALARM("b")`.
    pub rule: Option<String>,
    pub actions_enabled: bool,
    pub alarm_actions: Vec<String>,
    pub ok_actions: Vec<String>,
    pub insufficient_actions: Vec<String>,
}

impl Alarm {
    /// The metric the alarm watches, when it is one metric (directly, or the only metric of
    /// a math alarm without expressions).
    pub fn graphable(&self) -> Option<&MetricStat> {
        if let Some(m) = &self.metric {
            return Some(m);
        }
        if self.queries.iter().any(|q| q.expression.is_some()) {
            return None;
        }
        let mut metrics = self.queries.iter().filter(|q| q.return_data).filter_map(|q| q.metric.as_ref());
        match (metrics.next(), metrics.next()) {
            (Some(m), None) => Some(m),
            _ => None,
        }
    }

    /// The expression of a metric math alarm (the query it alarms on).
    pub fn expression(&self) -> Option<&str> {
        self.queries.iter().find(|q| q.return_data && q.expression.is_some()).and_then(|q| q.expression.as_deref())
    }

    /// The metric to graph, as a GetMetricData query.
    pub fn query(&self) -> Option<Query> {
        let m = self.graphable()?;
        Some(Query {
            id: "alarm".into(),
            namespace: m.namespace.clone(),
            name: m.name.clone(),
            dimensions: m.dimensions.clone(),
            stat: m.stat.clone(),
            period: m.period.max(1),
        })
    }

    pub fn period(&self) -> Option<u32> {
        self.metric.as_ref().map(|m| m.period).or_else(|| self.queries.iter().find_map(|q| q.metric.as_ref().map(|m| m.period)))
    }

    /// The dimensions of the metric (or of the first metric of a math alarm).
    pub fn dimensions(&self) -> &[(String, String)] {
        self.metric
            .as_ref()
            .or_else(|| self.queries.iter().find_map(|q| q.metric.as_ref()))
            .map_or(&[], |m| m.dimensions.as_slice())
    }

    /// What makes it fire, in one line: `Errors > 2 for 3 of 3 × 1 min`.
    pub fn condition(&self) -> String {
        if let Some(rule) = &self.rule {
            return rule.split_whitespace().collect::<Vec<_>>().join(" ");
        }
        let subject = match (&self.metric, self.expression()) {
            (Some(m), _) => match m.stat.as_str() {
                "Sum" | "SampleCount" | "" => m.name.clone(),
                stat => format!("{stat} {}", m.name),
            },
            (None, Some(e)) => e.to_string(),
            (None, None) => self.graphable().map_or_else(|| "?".into(), |m| m.name.clone()),
        };
        let op = self.comparison.as_deref().unwrap_or_default();
        let mut text = match op {
            "LessThanLowerOrGreaterThanUpperThreshold" => format!("{subject} outside the band"),
            "LessThanLowerThreshold" => format!("{subject} below the band"),
            "GreaterThanUpperThreshold" => format!("{subject} above the band"),
            _ => {
                let symbol = match op {
                    "GreaterThanThreshold" => ">",
                    "GreaterThanOrEqualToThreshold" => ">=",
                    "LessThanThreshold" => "<",
                    "LessThanOrEqualToThreshold" => "<=",
                    other => other,
                };
                format!("{subject} {symbol} {}", self.threshold.map(fmt_number).unwrap_or_default())
            }
        };
        if let Some(n) = self.evaluation_periods {
            text.push_str(&format!(" for {} of {n}", self.datapoints_to_alarm.unwrap_or(n)));
            if let Some(p) = self.period() {
                text.push_str(&format!(" × {}", fmt_period(p)));
            }
        }
        text
    }

    /// Whether `value` is on the firing side of the threshold.
    pub fn breaches(&self, value: f64) -> bool {
        let Some(t) = self.threshold else { return false };
        match self.comparison.as_deref() {
            Some("GreaterThanThreshold") => value > t,
            Some("GreaterThanOrEqualToThreshold") => value >= t,
            Some("LessThanThreshold") => value < t,
            Some("LessThanOrEqualToThreshold") => value <= t,
            _ => false,
        }
    }

    /// The module and target that show the alarm's resource, from its dimensions.
    pub fn resource(&self) -> Option<(&'static str, String)> {
        self.dimensions().iter().find_map(|(name, value)| {
            let module = match name.as_str() {
                "FunctionName" => "Lambda",
                "QueueName" => "SQS",
                "TableName" => "DynamoDB",
                "StateMachineArn" => "Step Functions",
                _ => return None,
            };
            Some((module, value.clone()))
        })
    }
}

/// `5`, `2.5`, `0.001`
pub fn fmt_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{v}") }
}

/// `30 s`, `1 min`, `5 min`, `1 h`, `1 day`
pub fn fmt_period(secs: u32) -> String {
    match secs {
        s if s % 86_400 == 0 => format!("{} day{}", s / 86_400, if s == 86_400 { "" } else { "s" }),
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

/// Every metric and composite alarm.
pub fn describe_alarms(aws: &Aws) -> Result<Vec<Alarm>> {
    let mut alarms = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..50 {
        let mut params = vec![("AlarmTypes.member.1", "MetricAlarm"), ("AlarmTypes.member.2", "CompositeAlarm"), ("MaxRecords", "100")];
        if let Some(t) = &token {
            params.push(("NextToken", t));
        }
        let xml = aws.query(&SERVICE, "DescribeAlarms", VERSION, &params)?;
        alarms.extend(parse_alarms(&xml));
        token = xml_text_raw(&xml, "NextToken").filter(|t| !t.is_empty());
        if token.is_none() {
            break;
        }
    }
    Ok(alarms)
}

/// A state change of an alarm.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub at: f64,
    pub summary: String,
    pub from: Option<State>,
    pub to: Option<State>,
}

/// The alarm's recent state changes, newest first.
pub fn history(aws: &Aws, name: &str, max: usize) -> Result<Vec<Change>> {
    let max = max.clamp(1, 100).to_string();
    let params = [
        ("AlarmName", name),
        ("AlarmTypes.member.1", "MetricAlarm"),
        ("AlarmTypes.member.2", "CompositeAlarm"),
        ("HistoryItemType", "StateUpdate"),
        ("ScanBy", "TimestampDescending"),
        ("MaxRecords", max.as_str()),
    ];
    let xml = aws.query(&SERVICE, "DescribeAlarmHistory", VERSION, &params)?;
    Ok(parse_history(&xml))
}

/// When the alarm was firing, from its state changes (newest first), up to `now`.
pub fn firing_spans(changes: &[Change], now: f64) -> Vec<(f64, f64)> {
    let mut spans = Vec::new();
    let mut start: Option<f64> = None;
    for c in changes.iter().rev() {
        match (c.to, start) {
            (Some(State::Alarm), None) => start = Some(c.at),
            (Some(s), Some(from)) if s != State::Alarm => {
                spans.push((from, c.at));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        spans.push((from, now));
    }
    spans
}

// ---- XML ---------------------------------------------------------------------------------

/// The child elements of an XML fragment, in order: (tag, inner XML). Same-named elements
/// may nest (a `member` inside a `member`); self-closing ones have no inner XML.
fn children(xml: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else { break };
        let head = &after[..gt];
        if head.starts_with(['/', '?', '!']) {
            rest = &after[gt + 1..];
            continue;
        }
        let name = head.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or_default();
        let body = &after[gt + 1..];
        if head.ends_with('/') {
            out.push((name, ""));
            rest = body;
            continue;
        }
        let (open, close) = (format!("<{name}>"), format!("</{name}>"));
        let (mut depth, mut at) = (1, 0);
        let end = loop {
            let next_open = body[at..].find(&open).map(|p| p + at);
            let Some(c) = body[at..].find(&close).map(|p| p + at) else { break None };
            match next_open {
                Some(o) if o < c => {
                    depth += 1;
                    at = o + open.len();
                }
                _ => {
                    depth -= 1;
                    if depth == 0 {
                        break Some(c);
                    }
                    at = c + close.len();
                }
            }
        };
        let Some(end) = end else { break };
        out.push((name, &body[..end]));
        rest = &body[end + close.len()..];
    }
    out
}

fn child<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    children(xml).into_iter().find(|(n, _)| *n == tag).map(|(_, b)| b)
}

/// The text of a direct child element, entities decoded and sanitized.
fn text(xml: &str, tag: &str) -> Option<String> {
    xml_text(&format!("<v>{}</v>", child(xml, tag)?), "v").filter(|t| !t.is_empty())
}

/// The text of a direct child element, byte-exact (names sent back to AWS).
fn raw(xml: &str, tag: &str) -> Option<String> {
    xml_text_raw(&format!("<v>{}</v>", child(xml, tag)?), "v")
}

fn members(xml: &str) -> Vec<&str> {
    children(xml).into_iter().filter(|(n, _)| *n == "member").map(|(_, b)| b).collect()
}

fn list(xml: &str, tag: &str) -> Vec<String> {
    child(xml, tag).map(members).unwrap_or_default().into_iter().filter_map(|m| xml_text(&format!("<v>{m}</v>"), "v")).collect()
}

fn dimensions(xml: &str) -> Vec<(String, String)> {
    child(xml, "Dimensions").map(members).unwrap_or_default().into_iter().filter_map(|d| Some((text(d, "Name")?, text(d, "Value")?))).collect()
}

fn number<T: std::str::FromStr>(xml: &str, tag: &str) -> Option<T> {
    text(xml, tag)?.trim().parse().ok()
}

fn metric_stat(xml: &str) -> Option<MetricStat> {
    let metric = child(xml, "Metric")?;
    Some(MetricStat {
        namespace: text(metric, "Namespace").unwrap_or_default(),
        name: text(metric, "MetricName")?,
        dimensions: dimensions(metric),
        stat: text(xml, "Stat").unwrap_or_default(),
        period: number(xml, "Period").unwrap_or(60),
    })
}

fn parse_alarm(m: &str, composite: bool) -> Alarm {
    let metric = text(m, "MetricName").map(|name| MetricStat {
        namespace: text(m, "Namespace").unwrap_or_default(),
        name,
        dimensions: dimensions(m),
        stat: text(m, "Statistic").or_else(|| text(m, "ExtendedStatistic")).unwrap_or_default(),
        period: number(m, "Period").unwrap_or(60),
    });
    let queries = child(m, "Metrics")
        .map(members)
        .unwrap_or_default()
        .into_iter()
        .map(|q| MathQuery {
            id: text(q, "Id").unwrap_or_default(),
            expression: text(q, "Expression"),
            label: text(q, "Label"),
            metric: child(q, "MetricStat").and_then(metric_stat),
            return_data: text(q, "ReturnData").is_none_or(|r| r == "true"),
        })
        .collect();
    let time = |tag: &str| text(m, tag).and_then(|t| parse_rfc3339(&t));
    Alarm {
        name: raw(m, "AlarmName").unwrap_or_default(),
        arn: raw(m, "AlarmArn").unwrap_or_default(),
        composite,
        description: text(m, "AlarmDescription"),
        state: State::parse(&text(m, "StateValue").unwrap_or_default()),
        reason: text(m, "StateReason"),
        since: time("StateTransitionedTimestamp").or_else(|| time("StateUpdatedTimestamp")),
        metric,
        queries,
        threshold: number(m, "Threshold"),
        comparison: text(m, "ComparisonOperator"),
        evaluation_periods: number(m, "EvaluationPeriods"),
        datapoints_to_alarm: number(m, "DatapointsToAlarm"),
        treat_missing: text(m, "TreatMissingData"),
        threshold_metric: text(m, "ThresholdMetricId"),
        rule: text(m, "AlarmRule"),
        actions_enabled: text(m, "ActionsEnabled").is_none_or(|a| a == "true"),
        alarm_actions: list(m, "AlarmActions"),
        ok_actions: list(m, "OKActions"),
        insufficient_actions: list(m, "InsufficientDataActions"),
    }
}

fn result<'a>(xml: &'a str, response: &str, result: &str) -> &'a str {
    let root = children(xml).into_iter().find(|(n, _)| *n == response).map_or(xml, |(_, b)| b);
    child(root, result).unwrap_or(root)
}

fn parse_alarms(xml: &str) -> Vec<Alarm> {
    let body = result(xml, "DescribeAlarmsResponse", "DescribeAlarmsResult");
    let of = |tag: &str, composite: bool| -> Vec<Alarm> {
        child(body, tag).map(members).unwrap_or_default().into_iter().map(|m| parse_alarm(m, composite)).collect()
    };
    let mut alarms = of("MetricAlarms", false);
    alarms.extend(of("CompositeAlarms", true));
    alarms
}

fn parse_history(xml: &str) -> Vec<Change> {
    let body = result(xml, "DescribeAlarmHistoryResponse", "DescribeAlarmHistoryResult");
    child(body, "AlarmHistoryItems")
        .map(members)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| {
            let data: serde_json::Value = text(item, "HistoryData").and_then(|d| serde_json::from_str(&d).ok()).unwrap_or_default();
            let state = |key: &str| data[key]["stateValue"].as_str().map(State::parse);
            Some(Change {
                at: text(item, "Timestamp").and_then(|t| parse_rfc3339(&t))?,
                summary: text(item, "HistorySummary").unwrap_or_default(),
                from: state("oldState"),
                to: state("newState"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALARMS: &str = r#"<DescribeAlarmsResponse xmlns="http://monitoring.amazonaws.com/doc/2010-08-01/">
      <DescribeAlarmsResult><MetricAlarms>
        <member>
          <AlarmName>payments-errors</AlarmName><AlarmArn>arn:aws:cloudwatch:us-east-1:1:alarm:payments-errors</AlarmArn>
          <StateValue>ALARM</StateValue><StateReason>Threshold Crossed &amp; more</StateReason>
          <StateUpdatedTimestamp>2026-10-21T15:41:00.000Z</StateUpdatedTimestamp>
          <StateTransitionedTimestamp>2026-10-21T15:40:00.000Z</StateTransitionedTimestamp>
          <MetricName>Errors</MetricName><Namespace>AWS/Lambda</Namespace><Statistic>Sum</Statistic>
          <Dimensions><member><Name>FunctionName</Name><Value>payments-webhook</Value></member></Dimensions>
          <Period>60</Period><EvaluationPeriods>3</EvaluationPeriods><DatapointsToAlarm>3</DatapointsToAlarm>
          <Threshold>2.0</Threshold><ComparisonOperator>GreaterThanThreshold</ComparisonOperator>
          <ActionsEnabled>true</ActionsEnabled>
          <AlarmActions><member>arn:aws:sns:us-east-1:1:ops-alerts</member></AlarmActions><OKActions/>
        </member>
        <member>
          <AlarmName>api-error-rate</AlarmName><StateValue>OK</StateValue>
          <Metrics>
            <member><Id>errors</Id><MetricStat><Metric><Namespace>AWS/Lambda</Namespace><MetricName>Errors</MetricName>
              <Dimensions><member><Name>FunctionName</Name><Value>orders-sync</Value></member></Dimensions></Metric>
              <Period>300</Period><Stat>Sum</Stat></MetricStat><ReturnData>false</ReturnData></member>
            <member><Id>rate</Id><Expression>100 * errors / calls</Expression><ReturnData>true</ReturnData></member>
          </Metrics>
          <EvaluationPeriods>3</EvaluationPeriods><DatapointsToAlarm>2</DatapointsToAlarm>
          <Threshold>5.0</Threshold><ComparisonOperator>GreaterThanOrEqualToThreshold</ComparisonOperator>
        </member>
      </MetricAlarms>
      <CompositeAlarms><member><AlarmName>checkout-degraded</AlarmName><StateValue>INSUFFICIENT_DATA</StateValue>
        <AlarmRule>ALARM(&quot;a&quot;)
          OR ALARM(&quot;b&quot;)</AlarmRule></member></CompositeAlarms>
      <NextToken>abc</NextToken></DescribeAlarmsResult></DescribeAlarmsResponse>"#;

    #[test]
    fn reads_metric_math_and_composite_alarms() {
        let alarms = parse_alarms(ALARMS);
        assert_eq!(alarms.len(), 3);
        let a = &alarms[0];
        assert_eq!((a.name.as_str(), a.state, a.reason.as_deref()), ("payments-errors", State::Alarm, Some("Threshold Crossed & more")));
        assert_eq!(a.since, parse_rfc3339("2026-10-21T15:40:00Z"));
        assert_eq!(a.dimensions(), [("FunctionName".to_string(), "payments-webhook".to_string())]);
        assert_eq!(a.alarm_actions, ["arn:aws:sns:us-east-1:1:ops-alerts"]);
        assert!(a.ok_actions.is_empty());
        assert_eq!(a.condition(), "Errors > 2 for 3 of 3 × 1 min");
        assert_eq!(a.resource(), Some(("Lambda", "payments-webhook".to_string())));
        assert!(a.breaches(3.0) && !a.breaches(2.0));

        let math = &alarms[1];
        assert!(math.metric.is_none() && math.graphable().is_none());
        assert_eq!(math.condition(), "100 * errors / calls >= 5 for 2 of 3 × 5 min");
        assert_eq!(math.dimensions()[0].1, "orders-sync");

        let composite = &alarms[2];
        assert!(composite.composite);
        assert_eq!(composite.condition(), r#"ALARM("a") OR ALARM("b")"#);
    }

    #[test]
    fn summarizes_conditions() {
        let alarm = |stat: &str, op: &str, period: u32| Alarm {
            metric: Some(MetricStat { namespace: "AWS/Lambda".into(), name: "Duration".into(), dimensions: vec![], stat: stat.into(), period }),
            comparison: Some(op.into()),
            threshold: Some(2500.5),
            evaluation_periods: Some(2),
            datapoints_to_alarm: None,
            ..parse_alarms(ALARMS)[0].clone()
        };
        assert_eq!(alarm("p95", "LessThanOrEqualToThreshold", 3600).condition(), "p95 Duration <= 2500.5 for 2 of 2 × 1 h");
        assert_eq!(alarm("Average", "LessThanLowerOrGreaterThanUpperThreshold", 30).condition(), "Average Duration outside the band for 2 of 2 × 30 s");
        assert_eq!(fmt_period(86_400), "1 day");
    }

    #[test]
    fn reads_history_and_firing_spans() {
        let xml = r#"<DescribeAlarmHistoryResponse><DescribeAlarmHistoryResult><AlarmHistoryItems>
          <member><Timestamp>2026-10-21T15:40:00Z</Timestamp><HistoryItemType>StateUpdate</HistoryItemType>
            <HistorySummary>Alarm updated from OK to ALARM</HistorySummary>
            <HistoryData>{&quot;oldState&quot;:{&quot;stateValue&quot;:&quot;OK&quot;},&quot;newState&quot;:{&quot;stateValue&quot;:&quot;ALARM&quot;}}</HistoryData></member>
          <member><Timestamp>2026-10-21T12:00:00Z</Timestamp><HistorySummary>Alarm updated from ALARM to OK</HistorySummary>
            <HistoryData>{&quot;oldState&quot;:{&quot;stateValue&quot;:&quot;ALARM&quot;},&quot;newState&quot;:{&quot;stateValue&quot;:&quot;OK&quot;}}</HistoryData></member>
          <member><Timestamp>2026-10-21T11:00:00Z</Timestamp><HistorySummary>Alarm updated from OK to ALARM</HistorySummary>
            <HistoryData>{&quot;oldState&quot;:{&quot;stateValue&quot;:&quot;OK&quot;},&quot;newState&quot;:{&quot;stateValue&quot;:&quot;ALARM&quot;}}</HistoryData></member>
        </AlarmHistoryItems></DescribeAlarmHistoryResult></DescribeAlarmHistoryResponse>"#;
        let changes = parse_history(xml);
        assert_eq!(changes.len(), 3);
        assert_eq!((changes[0].from, changes[0].to), (Some(State::Ok), Some(State::Alarm)));
        let t = |s: &str| parse_rfc3339(s).unwrap();
        let now = t("2026-10-21T16:00:00Z");
        assert_eq!(
            firing_spans(&changes, now),
            [(t("2026-10-21T11:00:00Z"), t("2026-10-21T12:00:00Z")), (t("2026-10-21T15:40:00Z"), now)]
        );
    }
}

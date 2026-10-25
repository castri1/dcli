//! CloudWatch metrics (GetMetricData) through [`super::cloudwatch`]: a metric's values over
//! time, for the Lambda charts and the alarms' graphs; and the metrics that exist
//! (ListMetrics), for what custom metrics cost.

use super::cloudwatch::{SERVICE, VERSION};
use super::{Aws, Result};
use crate::util::{parse_rfc3339, rfc3339, xml_blocks, xml_text};

/// A metric to read: `namespace` / `name` with its dimensions, aggregated with `stat`
/// (`Sum`, `Average`, `Maximum`, `p95`…) over `period` seconds. `id` names its series.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub id: String,
    pub namespace: String,
    pub name: String,
    pub dimensions: Vec<(String, String)>,
    pub stat: String,
    pub period: u32,
}

/// A metric's values, oldest first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Series {
    pub id: String,
    pub label: String,
    /// (Unix seconds, value).
    pub points: Vec<(f64, f64)>,
}

/// Reads `queries` (at most 500) between `start` and `end` (Unix seconds). Periods without
/// data have no point.
#[allow(dead_code)] // used by the Lambda and Alarms modules
pub fn get_metric_data(aws: &Aws, queries: &[Query], start: f64, end: f64) -> Result<Vec<Series>> {
    let mut params: Vec<(String, String)> = vec![
        ("StartTime".into(), rfc3339(start as i64)),
        ("EndTime".into(), rfc3339(end as i64)),
        ("ScanBy".into(), "TimestampAscending".into()),
    ];
    for (i, q) in queries.iter().enumerate() {
        let p = format!("MetricDataQueries.member.{}", i + 1);
        params.push((format!("{p}.Id"), q.id.clone()));
        params.push((format!("{p}.MetricStat.Metric.Namespace"), q.namespace.clone()));
        params.push((format!("{p}.MetricStat.Metric.MetricName"), q.name.clone()));
        for (j, (name, value)) in q.dimensions.iter().enumerate() {
            let d = format!("{p}.MetricStat.Metric.Dimensions.member.{}", j + 1);
            params.push((format!("{d}.Name"), name.clone()));
            params.push((format!("{d}.Value"), value.clone()));
        }
        params.push((format!("{p}.MetricStat.Period"), q.period.to_string()));
        params.push((format!("{p}.MetricStat.Stat"), q.stat.clone()));
    }
    let mut series: Vec<Series> = queries.iter().map(|q| Series { id: q.id.clone(), ..Default::default() }).collect();
    let mut token: Option<String> = None;
    for _ in 0..20 {
        let mut page = params.clone();
        if let Some(t) = &token {
            page.push(("NextToken".into(), t.clone()));
        }
        let page: Vec<(&str, &str)> = page.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let xml = aws.query(&SERVICE, "GetMetricData", VERSION, &page)?;
        for result in parse(&xml) {
            if let Some(s) = series.iter_mut().find(|s| s.id == result.id) {
                s.label = result.label;
                s.points.extend(result.points);
            }
        }
        token = xml_text(&xml, "NextToken").filter(|t| !t.is_empty());
        if token.is_none() {
            break;
        }
    }
    for s in &mut series {
        s.points.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    Ok(series)
}

/// A metric that exists, as ListMetrics lists it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metric {
    pub namespace: String,
    pub name: String,
    pub dimensions: Vec<(String, String)>,
}

/// The metrics that received data in the last three hours (those the bill counts as custom
/// metrics, outside the `AWS/` namespaces), at most about `max`; the bool says whether there
/// were more. ListMetrics is free.
pub fn list_recent_metrics(aws: &Aws, max: usize) -> Result<(Vec<Metric>, bool)> {
    // ListMetrics gives up to 500 a page, and may give empty pages with a token: a page cap
    // ends it whatever the answers.
    const MAX_PAGES: usize = 200;
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut page: Vec<(&str, &str)> = vec![("RecentlyActive", "PT3H")];
        if let Some(t) = &token {
            page.push(("NextToken", t));
        }
        let xml = aws.query(&SERVICE, "ListMetrics", VERSION, &page)?;
        out.extend(parse_metrics(&xml));
        token = xml_text(&xml, "NextToken").filter(|t| !t.is_empty());
        match token {
            Some(_) if out.len() < max => {}
            more => return Ok((out, more.is_some())),
        }
    }
    Ok((out, true))
}

fn parse_metrics(xml: &str) -> Vec<Metric> {
    super::xml::result_of(xml, "ListMetrics")
        .list("Metrics")
        .iter()
        .map(|m| Metric {
            namespace: m.string("Namespace"),
            name: m.string("MetricName"),
            dimensions: m.list("Dimensions").iter().map(|d| (d.string("Name"), d.string("Value"))).collect(),
        })
        .collect()
}

fn parse(xml: &str) -> Vec<Series> {
    let results = xml_blocks(xml, "MetricDataResults").into_iter().next().unwrap_or("");
    members(results)
        .into_iter()
        .map(|m| {
            let list = |tag: &str| xml_blocks(m, tag).into_iter().next().map(members).unwrap_or_default();
            let times = list("Timestamps").into_iter().map(|t| parse_rfc3339(t.trim()));
            let values = list("Values").into_iter().map(|v| v.trim().parse::<f64>().ok());
            Series {
                id: xml_text(m, "Id").unwrap_or_default(),
                label: xml_text(m, "Label").unwrap_or_default(),
                points: times.zip(values).filter_map(|(t, v)| Some((t?, v?))).collect(),
            }
        })
        .collect()
}

/// The outermost `<member>` elements of `xml` (in GetMetricData results they nest).
fn members(xml: &str) -> Vec<&str> {
    let bytes = xml.as_bytes();
    let (mut out, mut depth, mut start, mut i) = (Vec::new(), 0usize, 0usize, 0usize);
    while i < bytes.len() {
        if bytes[i..].starts_with(b"<member>") {
            if depth == 0 {
                start = i + 8;
            }
            depth += 1;
            i += 8;
        } else if bytes[i..].starts_with(b"</member>") {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                out.push(&xml[start..i]);
            }
            i += 9;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nested_results() {
        let xml = "<GetMetricDataResponse><GetMetricDataResult><MetricDataResults>\
            <member><Timestamps><member>2026-10-21T10:00:00Z</member><member>2026-10-21T10:05:00Z</member></Timestamps>\
            <Values><member>3.0</member><member>7.5</member></Values><Label>Invocations</Label><Id>inv</Id>\
            <StatusCode>Complete</StatusCode></member>\
            <member><Timestamps/><Values/><Label>Errors</Label><Id>err</Id><StatusCode>Complete</StatusCode></member>\
            </MetricDataResults></GetMetricDataResult></GetMetricDataResponse>";
        let series = parse(xml);
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].id, "inv");
        assert_eq!(series[0].points, [(parse_rfc3339("2026-10-21T10:00:00Z").unwrap(), 3.0), (parse_rfc3339("2026-10-21T10:05:00Z").unwrap(), 7.5)]);
        assert_eq!((series[1].label.as_str(), series[1].points.len()), ("Errors", 0));
    }

    #[test]
    fn reads_listed_metrics() {
        let xml = "<ListMetricsResponse><ListMetricsResult><Metrics>\
            <member><Namespace>LambdaInsights</Namespace><MetricName>memory_utilization</MetricName>\
            <Dimensions><member><Name>function_name</Name><Value>orders &amp; co</Value></member></Dimensions></member>\
            <member><Namespace>Orders/App</Namespace><MetricName>OrderCreated</MetricName><Dimensions/></member>\
            </Metrics><NextToken>abc</NextToken></ListMetricsResult></ListMetricsResponse>";
        let m = parse_metrics(xml);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].dimensions, [("function_name".to_string(), "orders & co".to_string())]);
        assert_eq!((m[1].namespace.as_str(), m[1].name.as_str(), m[1].dimensions.len()), ("Orders/App", "OrderCreated", 0));
    }
}

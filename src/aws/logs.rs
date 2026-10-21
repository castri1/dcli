use serde::Deserialize;
use serde_json::{Value, json};

use super::{Aws, Result, Service};
use crate::util::{fmt_bytes, sanitize};

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

// ---- Logs Insights ----------------------------------------------------------------------

/// One field of an Insights result row.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ResultField {
    pub field: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QueryStatistics {
    pub records_matched: f64,
    pub records_scanned: f64,
    pub bytes_scanned: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QueryResults {
    /// `Scheduled`, `Running`, `Complete`, `Failed`, `Cancelled`, `Timeout` or `Unknown`.
    pub status: String,
    /// Partial while the query runs.
    pub results: Vec<Vec<ResultField>>,
    pub statistics: QueryStatistics,
}

impl QueryResults {
    pub fn running(&self) -> bool {
        matches!(self.status.as_str(), "Scheduled" | "Running" | "")
    }

    /// `1,234 of 50,000 records matched · 12.3 MB scanned` (Insights bills the bytes scanned).
    pub fn summary(&self) -> String {
        let s = &self.statistics;
        let matched = if s.records_scanned > 0.0 {
            format!("{} of {} records matched", count(s.records_matched), count(s.records_scanned))
        } else {
            format!("{} records matched", count(s.records_matched))
        };
        format!("{matched} · {} scanned", fmt_bytes(s.bytes_scanned as i64))
    }
}

/// `1234567` → `1,234,567`.
pub fn count(n: f64) -> String {
    let digits = (n.max(0.0) as u64).to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1 row`, `1,234 rows`.
pub fn rows(n: usize) -> String {
    if n == 1 { "1 row".into() } else { format!("{} rows", count(n as f64)) }
}

/// A saved Insights query.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QueryDefinition {
    pub name: String,
    pub query_string: String,
    pub log_group_names: Vec<String>,
    /// `CWLI` (the Logs Insights language), `SQL` or `PPL`.
    pub query_language: Option<String>,
}

/// Starts an Insights query over `groups` between `start` and `end` (Unix seconds) and
/// returns its id. `language` is `None` for the Logs Insights language.
pub fn start_query(
    aws: &Aws,
    groups: &[String],
    (start, end): (i64, i64),
    query: &str,
    language: Option<&str>,
    limit: Option<u32>,
) -> Result<String> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        query_id: String,
    }
    let mut input = json!({ "logGroupNames": groups, "startTime": start, "endTime": end, "queryString": query });
    if let Some(l) = language.filter(|l| *l != "CWLI") {
        input["queryLanguage"] = json!(l);
    }
    if let Some(n) = limit {
        input["limit"] = json!(n);
    }
    let out: Out = aws.call(&SERVICE, "StartQuery", &input)?;
    Ok(out.query_id)
}

pub fn get_query_results(aws: &Aws, query_id: &str) -> Result<QueryResults> {
    aws.call(&SERVICE, "GetQueryResults", &json!({ "queryId": query_id }))
}

pub fn stop_query(aws: &Aws, query_id: &str) -> Result<()> {
    let _: Value = aws.call(&SERVICE, "StopQuery", &json!({ "queryId": query_id }))?;
    Ok(())
}

/// The saved queries (at most 10 pages of 1,000).
pub fn query_definitions(aws: &Aws) -> Result<Vec<QueryDefinition>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        query_definitions: Vec<QueryDefinition>,
        next_token: Option<String>,
    }
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..10 {
        let mut input = json!({ "maxResults": 1000 });
        if let Some(t) = &token {
            input["nextToken"] = json!(t);
        }
        let out: Out = aws.call(&SERVICE, "DescribeQueryDefinitions", &input)?;
        all.extend(out.query_definitions);
        match out.next_token.filter(|t| !t.is_empty()) {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    Ok(all)
}

/// Every field of the record a result row points to (its `@ptr`), in the order AWS sends them.
pub fn get_log_record(aws: &Aws, ptr: &str) -> Result<Vec<(String, String)>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        log_record: serde_json::Map<String, Value>,
    }
    let out: Out = aws.call(&SERVICE, "GetLogRecord", &json!({ "logRecordPointer": ptr }))?;
    Ok(out
        .log_record
        .into_iter()
        .map(|(k, v)| (sanitize(&k), sanitize(v.as_str().map_or_else(|| v.to_string(), str::to_string).trim_end())))
        .collect())
}

/// Insights results as a table: the fields in the order they first appear (without `@ptr`,
/// the pointer to the whole record), a row per result, each value on one line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Each row's `@ptr`, to read its whole record.
    pub ptrs: Vec<Option<String>>,
}

/// A value on one line and safe to print: line breaks become ⏎, tabs spaces.
fn one_line(value: &str) -> String {
    sanitize(value).trim_end().replace("\r\n", "\n").replace('\n', " ⏎ ").replace('\t', " ")
}

impl Table {
    pub fn new(results: &[Vec<ResultField>]) -> Table {
        let mut columns: Vec<String> = Vec::new();
        for f in results.iter().flatten() {
            if f.field != "@ptr" && !columns.contains(&f.field) {
                columns.push(f.field.clone());
            }
        }
        let rows = results
            .iter()
            .map(|row| columns.iter().map(|c| row.iter().find(|f| &f.field == c).map(|f| one_line(&f.value)).unwrap_or_default()).collect())
            .collect();
        let ptrs = results.iter().map(|row| row.iter().find(|f| f.field == "@ptr").map(|f| f.value.clone())).collect();
        Table { columns: columns.iter().map(|c| one_line(c)).collect(), rows, ptrs }
    }

    /// A Markdown table of the first `max` rows.
    pub fn markdown(&self, max: usize) -> String {
        let row = |values: &[String]| format!("| {} |\n", values.iter().map(|v| v.replace('|', "\\|")).collect::<Vec<_>>().join(" | "));
        let mut out = row(&self.columns);
        out.push_str(&format!("|{}\n", "---|".repeat(self.columns.len())));
        for r in self.rows.iter().take(max) {
            out.push_str(&row(r));
        }
        out
    }

    /// CSV (RFC 4180) with a header line.
    pub fn csv(&self) -> String {
        let field = |v: &String| {
            if v.contains([',', '"', '\n']) { format!("\"{}\"", v.replace('"', "\"\"")) } else { v.clone() }
        };
        let mut out = String::new();
        for values in std::iter::once(&self.columns).chain(&self.rows) {
            out.push_str(&values.iter().map(field).collect::<Vec<_>>().join(","));
            out.push_str("\r\n");
        }
        out
    }

    /// The column to draw as bars: for `stats … by …` results, the last column whose values
    /// are all numbers, when another column says what each row is (a time bin, a group).
    pub fn chart_column(&self) -> Option<usize> {
        if self.rows.len() < 2 || self.columns.len() < 2 {
            return None;
        }
        let numeric = |c: usize| self.rows.iter().all(|r| r[c].trim().parse::<f64>().is_ok());
        let column = (0..self.columns.len()).rev().find(|&c| numeric(c))?;
        (0..self.columns.len()).any(|c| c != column && !numeric(c)).then_some(column)
    }
}

/// Insights results as Markdown for an AI agent or an issue: the query, where and when it
/// ran, what it matched, and the first `max_rows` results.
pub struct Report<'a> {
    pub env: &'a str,
    pub region: &'a str,
    pub groups: &'a [String],
    /// `last 1h · 2026-10-21 13:46 → 14:46 UTC`
    pub range: &'a str,
    pub query: &'a str,
}

impl Report<'_> {
    pub fn markdown(&self, results: &QueryResults, table: &Table, max_rows: usize) -> String {
        let mut md = format!("## CloudWatch Logs Insights results ({})\n\n", self.env);
        let groups: Vec<String> = self.groups.iter().map(|g| format!("`{g}`")).collect();
        md.push_str(&format!("- **Log groups:** {} · region `{}`\n", groups.join(", "), self.region));
        md.push_str(&format!("- **Time range:** {}\n", self.range));
        md.push_str(&format!("- **Status:** {} · {}\n\n", results.status, results.summary()));
        md.push_str(&format!("**Query:**\n\n```\n{}\n```\n\n", self.query.trim()));
        if table.rows.is_empty() {
            md.push_str("No results.\n");
        } else {
            let shown = table.rows.len().min(max_rows);
            let of = if shown < table.rows.len() { format!("first {shown} of {}", rows(table.rows.len())) } else { rows(shown) };
            md.push_str(&format!("**Results** ({of}):\n\n{}", table.markdown(max_rows)));
        }
        md.push_str("\n> Log lines can contain secrets the code printed: review them before sharing outside your team.\n");
        md
    }
}

/// `last 1h · 2026-10-21 13:46 → 14:46 UTC`
pub fn range_label(label: &str, start: i64, end: i64) -> String {
    let (from, to) = (crate::util::fmt_utc(start as f64), crate::util::fmt_utc(end as f64));
    let to = if from.get(..10) == to.get(..10) { to.get(11..).unwrap_or(&to).to_string() } else { to };
    format!("last {label} · {} → {to}", from.trim_end_matches(" UTC"))
}

/// Console page of a log group.
pub fn console_url(region: &str, group: &str) -> String {
    super::console_url(region, &format!("cloudwatch/home?region={region}#logsV2:log-groups/log-group/{}", group.replace('/', "$252F")))
}

/// The Logs Insights page of the console.
pub fn insights_console_url(region: &str) -> String {
    super::console_url(region, &format!("cloudwatch/home?region={region}#logsV2:logs-insights"))
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

    fn row(fields: &[(&str, &str)]) -> Vec<ResultField> {
        fields.iter().map(|(f, v)| ResultField { field: f.to_string(), value: v.to_string() }).collect()
    }

    #[test]
    fn tabulates_insights_results() {
        let table = Table::new(&[
            row(&[("@timestamp", "2026-10-21 10:00:00.000"), ("@message", "ERROR boom\n  at x\t| y"), ("@ptr", "p1")]),
            row(&[("@timestamp", "2026-10-21 10:00:07.000"), ("level", "\u{1b}[31mWARN"), ("@ptr", "p2")]),
        ]);
        assert_eq!(table.columns, ["@timestamp", "@message", "level"]);
        assert_eq!(table.rows[0], ["2026-10-21 10:00:00.000", "ERROR boom ⏎   at x | y", ""]);
        assert_eq!(table.rows[1][2], "WARN");
        assert_eq!(table.ptrs, [Some("p1".to_string()), Some("p2".to_string())]);
        assert_eq!(table.chart_column(), None);
        assert_eq!(
            table.markdown(1),
            "| @timestamp | @message | level |\n|---|---|---|\n| 2026-10-21 10:00:00.000 | ERROR boom ⏎   at x \\| y |  |\n"
        );
        let csv = Table { columns: vec!["a".into(), "b".into()], rows: vec![vec!["x,y".into(), "say \"hi\"".into()]], ptrs: vec![None] }.csv();
        assert_eq!(csv, "a,b\r\n\"x,y\",\"say \"\"hi\"\"\"\r\n");
    }

    #[test]
    fn charts_stats_by_bin() {
        let stats = Table::new(&[
            row(&[("bin(5m)", "2026-10-21 10:00:00.000"), ("count(*)", "12")]),
            row(&[("bin(5m)", "2026-10-21 10:05:00.000"), ("count(*)", "3")]),
        ]);
        assert_eq!(stats.chart_column(), Some(1));
        assert_eq!(count(1_234_567.0), "1,234,567");
        assert_eq!(count(999.0), "999");
        let start = 1_761_043_560; // 2025-10-21 10:46 UTC
        assert_eq!(range_label("1h", start, start + 3600), "last 1h · 2025-10-21 10:46 → 11:46 UTC");
    }
}

//! Performance Insights (awsJson1_1, `X-Amz-Target: PerformanceInsightsv20180227.<Operation>`):
//! an Aurora instance's database load over time and what makes it up: the SQL statements
//! and the wait events its sessions spent their time in. Instances are named by their
//! `DbiResourceId` (`db-…`), and only those with Performance Insights on have any.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service { endpoint: "pi", signing_name: "pi", target_prefix: "PerformanceInsightsv20180227" };

/// One statement or wait event and its part of the load.
#[derive(Debug, Clone, Serialize)]
pub struct Top {
    /// The statement's id (tokenized SQL).
    pub id: Option<String>,
    /// The SQL text, literals replaced by `?` (Performance Insights keeps its first 500
    /// characters), or the wait event's name.
    pub label: String,
    /// The wait event's type (CPU, IO, Lock…).
    pub kind: Option<String>,
    /// Average active sessions over the window.
    pub load: f64,
    /// The load split by wait event type: (type, sessions).
    pub partitions: Vec<(String, f64)>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Dimensions {
    dimensions: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Key {
    dimensions: HashMap<String, String>,
    total: f64,
    partitions: Vec<f64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct DimensionKeys {
    partition_keys: Vec<Dimensions>,
    keys: Vec<Key>,
}

/// The `limit` largest parts of the load between `start` and `end`, grouped by `group`.
fn describe(aws: &Aws, resource: &str, (start, end): (f64, f64), group: &str, limit: usize, partition: bool) -> Result<DimensionKeys> {
    let mut body = json!({
        "ServiceType": "RDS",
        "Identifier": resource,
        "StartTime": start as i64,
        "EndTime": end as i64,
        "Metric": "db.load.avg",
        "GroupBy": {"Group": group, "Limit": limit.clamp(1, 25)},
    });
    if partition {
        body["PartitionBy"] = json!({"Group": "db.wait_event_type", "Limit": 5});
    }
    aws.call(&SERVICE, "DescribeDimensionKeys", &body)
}

/// The statements that made the most load, with the wait types each spent it in.
pub fn top_sql(aws: &Aws, resource: &str, start: f64, end: f64, limit: usize) -> Result<Vec<Top>> {
    let out = describe(aws, resource, (start, end), "db.sql_tokenized", limit, true)?;
    let types: Vec<String> = out
        .partition_keys
        .iter()
        .map(|p| p.dimensions.get("db.wait_event_type.name").cloned().unwrap_or_else(|| "other".into()))
        .collect();
    Ok(out
        .keys
        .into_iter()
        .map(|k| Top {
            id: k.dimensions.get("db.sql_tokenized.id").cloned(),
            label: k.dimensions.get("db.sql_tokenized.statement").cloned().unwrap_or_default(),
            kind: None,
            load: k.total,
            partitions: types.iter().cloned().zip(k.partitions).collect(),
        })
        .collect())
}

/// The wait events sessions spent the most time in.
pub fn top_waits(aws: &Aws, resource: &str, start: f64, end: f64, limit: usize) -> Result<Vec<Top>> {
    let out = describe(aws, resource, (start, end), "db.wait_event", limit, false)?;
    Ok(out
        .keys
        .into_iter()
        .map(|k| Top {
            id: None,
            label: k.dimensions.get("db.wait_event.name").cloned().unwrap_or_default(),
            kind: k.dimensions.get("db.wait_event.type").cloned(),
            load: k.total,
            partitions: Vec::new(),
        })
        .collect())
}

/// The database load (average active sessions) between `start` and `end`, one point per
/// `period` seconds (1, 60, 300, 3600 or 86400).
pub fn load(aws: &Aws, resource: &str, start: f64, end: f64, period: u32) -> Result<Vec<(f64, f64)>> {
    let body = json!({
        "ServiceType": "RDS",
        "Identifier": resource,
        "MetricQueries": [{"Metric": "db.load.avg"}],
        "StartTime": start as i64,
        "EndTime": end as i64,
        "PeriodInSeconds": period,
    });
    let out: Value = aws.call(&SERVICE, "GetResourceMetrics", &body)?;
    let points = out["MetricList"][0]["DataPoints"].as_array().cloned().unwrap_or_default();
    Ok(points.iter().filter_map(|p| Some((p["Timestamp"].as_f64()?, p["Value"].as_f64()?))).collect())
}

/// How to turn Performance Insights on for an instance (dcli does not change it).
pub fn how_to_enable(instance: &str, region: &str) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    format!(
        "aws rds modify-db-instance --db-instance-identifier {} --enable-performance-insights --region {}",
        quote(instance),
        quote(region)
    )
}

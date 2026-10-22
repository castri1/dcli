//! DynamoDB: provisioned capacity far above what is consumed, tables nobody reads or writes,
//! on-demand tables with steady traffic, and large, quiet tables for the Standard-IA class.
//! Global secondary indexes have capacity of their own and are not counted.

use super::{Finding, GB, Impact, MONTH, Scan, metric_data, par_map, peak, per_month, qty, query, query_id, size, total};
use crate::aws::dynamodb::{self, TableDescription};
use crate::aws::metrics::Series;
use crate::aws::{self, Aws};

const MODULE: &str = "DynamoDB";
/// Tables looked at, at most.
const MAX_TABLES: usize = 500;
/// Capacity-unit hours a month removed: medium impact from 50 k, high from 500 k.
const UNIT_HOURS_MEDIUM: f64 = 50_000.0;
const UNIT_HOURS_HIGH: f64 = 500_000.0;
/// Request units a month of a steady on-demand table: listed from 100 M, medium from 1 B,
/// high from 10 B.
const REQUESTS_LOW: f64 = 1e8;
const REQUESTS_MEDIUM: f64 = 1e9;
const REQUESTS_HIGH: f64 = 1e10;
/// Steady: the average is at least this share of the busiest hour.
const STEADY: f64 = 0.5;
/// Stored bytes of a quiet table: Standard-IA from 50 GB (medium from 200 GB, high from
/// 1 TB) when it consumes at most this many units per second per GB stored.
const IA_LOW: f64 = 50.0 * GB;
const IA_MEDIUM: f64 = 200.0 * GB;
const IA_HIGH: f64 = 1024.0 * GB;
const IA_UNITS_PER_GB: f64 = 0.05;
/// An unused on-demand table is listed from this size.
const UNUSED_SIZE: f64 = GB;

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let tables = all_tables(scan.aws)?;
    let described: Vec<TableDescription> =
        par_map(&tables, |name| dynamodb::describe_table(scan.aws, name)).into_iter().collect::<aws::Result<_>>()?;
    let mut queries = Vec::new();
    for (i, t) in described.iter().enumerate() {
        let dims = [("TableName", t.table_name.as_str())];
        queries.push(query(query_id("rd", i), "AWS/DynamoDB", "ConsumedReadCapacityUnits", &dims, "Sum", 3600));
        queries.push(query(query_id("wr", i), "AWS/DynamoDB", "ConsumedWriteCapacityUnits", &dims, "Sum", 3600));
    }
    let series = metric_data(scan.aws, &queries, scan.days)?;
    Ok(described
        .iter()
        .zip(series.chunks(2))
        .flat_map(|(t, s)| table_findings(t, &Traffic::of(&s[0], scan.days), &Traffic::of(&s[1], scan.days), scan.days))
        .collect())
}

fn all_tables(aws: &Aws) -> aws::Result<Vec<String>> {
    let mut all = Vec::new();
    let mut start: Option<String> = None;
    loop {
        let (page, next) = dynamodb::list_tables(aws, start.as_deref())?;
        all.extend(page);
        match next {
            Some(n) if all.len() < MAX_TABLES => start = Some(n),
            _ => return Ok(all),
        }
    }
}

/// Consumed capacity units over the window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Traffic {
    total: f64,
    /// Units per second, on average.
    mean: f64,
    /// Units per second in the busiest hour.
    peak: f64,
}

impl Traffic {
    fn of(hourly_sums: &Series, days: u32) -> Traffic {
        let total = total(hourly_sums);
        Traffic { total, mean: total / (f64::from(days.max(1)) * 86_400.0), peak: peak(hourly_sums).unwrap_or(0.0) / 3600.0 }
    }
}

/// Capacity to keep for a peak: half again, at least 1.
fn needed(peak: f64) -> i64 {
    ((peak * 1.5).ceil() as i64).max(1)
}

fn table_findings(t: &TableDescription, read: &Traffic, write: &Traffic, days: u32) -> Vec<Finding> {
    let name = t.table_name.as_str();
    let stored = t.table_size_bytes.unwrap_or(0) as f64;
    let capacity = t.provisioned_throughput.as_ref();
    let (rcu, wcu) = if t.on_demand() {
        (0, 0)
    } else {
        (capacity.and_then(|c| c.read_capacity_units).unwrap_or(0), capacity.and_then(|c| c.write_capacity_units).unwrap_or(0))
    };
    let unit_hours = |units: i64| units as f64 * MONTH / 3600.0;
    let new = |summary: String, recommendation: String, reduction: String, impact: Impact| {
        Finding::new(MODULE, name, name, summary, recommendation, reduction, impact)
    };
    let traffic = format!(
        "reads {}/s on average, {}/s in the busiest hour; writes {}/s and {}/s ({days} days)",
        rate(read.mean),
        rate(read.peak),
        rate(write.mean),
        rate(write.peak)
    );

    if read.total == 0.0 && write.total == 0.0 {
        if !t.on_demand() && rcu + wcu > 0 {
            let hours = unit_hours(rcu + wcu);
            return vec![
                new(
                    format!("no reads or writes in {days} days, {rcu} RCU / {wcu} WCU provisioned"),
                    "Delete it if nothing needs it (back it up first), or switch it to on demand".into(),
                    format!("−{rcu} RCU and −{wcu} WCU ({} capacity-unit hours a month)", qty(hours)),
                    Impact::of(hours, UNIT_HOURS_MEDIUM, UNIT_HOURS_HIGH),
                )
                .score(hours / UNIT_HOURS_HIGH)
                .evidence(format!("ConsumedRead/WriteCapacityUnits: none in {days} days"))
                .evidence(format!("{} stored", size(stored)))
                .caveat("Tables used rarely (a yearly job, a disaster-recovery copy) look unused in a short window."),
            ];
        }
        if stored >= UNUSED_SIZE {
            return vec![
                new(
                    format!("no reads or writes in {days} days, {} stored", size(stored)),
                    "Delete it if nothing needs it (back it up first)".into(),
                    format!("{} stored", size(stored)),
                    Impact::of(stored, IA_MEDIUM, IA_HIGH),
                )
                .score(stored / IA_HIGH)
                .evidence(format!("ConsumedRead/WriteCapacityUnits: none in {days} days"))
                .caveat("Tables used rarely (a yearly job, a disaster-recovery copy) look unused in a short window."),
            ];
        }
        return Vec::new();
    }

    let mut found = Vec::new();
    if !t.on_demand() && rcu + wcu > 0 {
        let lower = |provisioned: i64, peak: f64| {
            let keep = needed(peak);
            if keep as f64 <= provisioned as f64 * 0.5 { keep } else { provisioned }
        };
        let (new_rcu, new_wcu) = (lower(rcu, read.peak), lower(wcu, write.peak));
        let removed = (rcu - new_rcu) + (wcu - new_wcu);
        if removed > 0 {
            let hours = unit_hours(removed);
            found.push(
                new(
                    format!("{rcu} RCU / {wcu} WCU provisioned, the busiest hour used {} / {}", rate(read.peak), rate(write.peak)),
                    format!("Lower it to {new_rcu} RCU / {new_wcu} WCU, or switch to on demand"),
                    format!("−{} RCU and −{} WCU ({} capacity-unit hours a month)", rcu - new_rcu, wcu - new_wcu, qty(hours)),
                    Impact::of(hours, UNIT_HOURS_MEDIUM, UNIT_HOURS_HIGH),
                )
                .score(hours / UNIT_HOURS_HIGH)
                .evidence(traffic.clone())
                .evidence("the suggestion keeps half again the busiest hour's average")
                .caveat("Peaks within an hour are higher than its average: watch throttling after the change.")
                .caveat("If auto scaling manages the table, lower its minimum or raise its target utilization instead.")
                .caveat("After switching to on demand, the table can switch back to provisioned only once per 24 h."),
            );
        }
    }
    if t.on_demand() {
        let monthly = (read.total + write.total) * per_month(days);
        let (mean, peak) = (read.mean + write.mean, read.peak + write.peak);
        if monthly >= REQUESTS_LOW && peak > 0.0 && mean / peak >= STEADY {
            found.push(
                new(
                    format!("on demand with steady traffic: {}/s on average, {}/s in the busiest hour", rate(mean), rate(peak)),
                    format!("Consider provisioned capacity with auto scaling around {} units/s", needed(mean)),
                    format!("{} request units a month billed per request", qty(monthly)),
                    Impact::of(monthly, REQUESTS_MEDIUM, REQUESTS_HIGH),
                )
                .score(monthly / REQUESTS_HIGH)
                .evidence(traffic.clone())
                .caveat("On-demand requests and provisioned capacity are billed differently: compare both with your prices before switching.")
                .caveat("After switching to provisioned, the table can switch back to on demand only once per 24 h."),
            );
        }
    }
    if t.table_class() == "STANDARD" && stored >= IA_LOW && (read.mean + write.mean) / (stored / GB) <= IA_UNITS_PER_GB {
        found.push(
            new(
                format!("{} stored, {}/s of reads and writes: storage dominates", size(stored), rate(read.mean + write.mean)),
                "Consider the Standard-IA table class".into(),
                format!("{} stored in the Standard class", size(stored)),
                Impact::of(stored, IA_MEDIUM, IA_HIGH),
            )
            .score(stored / IA_HIGH)
            .evidence(traffic)
            .caveat("Standard-IA bills storage for less and reads and writes for more: it pays off only while storage dominates."),
        );
    }
    found
}

/// `0.4`, `12`, `1.5 k` (units per second).
fn rate(v: f64) -> String {
    if v > 0.0 && v < 0.1 { "<0.1".into() } else { qty(v) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::dynamodb::{BillingModeSummary, TableClassSummary, Throughput};

    fn table(name: &str, on_demand: bool, rcu: i64, wcu: i64, gb: f64) -> TableDescription {
        TableDescription {
            table_name: name.into(),
            billing_mode_summary: on_demand.then(|| BillingModeSummary { billing_mode: Some("PAY_PER_REQUEST".into()) }),
            provisioned_throughput: Some(Throughput { read_capacity_units: Some(rcu), write_capacity_units: Some(wcu) }),
            table_size_bytes: Some((gb * GB) as i64),
            ..Default::default()
        }
    }

    fn traffic(mean: f64, peak: f64, days: u32) -> Traffic {
        Traffic { total: mean * f64::from(days) * 86_400.0, mean, peak }
    }

    #[test]
    fn over_provisioned_tables() {
        let found = table_findings(&table("invoices", false, 500, 500, 1.0), &traffic(10.0, 20.0, 14), &traffic(12.0, 26.0, 14), 14);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].recommendation, "Lower it to 30 RCU / 39 WCU, or switch to on demand");
        // 931 units removed × 730.5 hours.
        assert_eq!(found[0].reduction, "−470 RCU and −461 WCU (680 k capacity-unit hours a month)");
        assert_eq!(found[0].impact, Impact::High);
        // Capacity used at more than half is left alone.
        assert!(table_findings(&table("busy", false, 30, 30, 1.0), &traffic(10.0, 15.0, 14), &traffic(10.0, 15.0, 14), 14).is_empty());
    }

    #[test]
    fn unused_tables() {
        let none = Traffic::default();
        let found = table_findings(&table("legacy", false, 100, 50, 0.1), &none, &none, 14);
        assert_eq!(found[0].reduction, "−100 RCU and −50 WCU (110 k capacity-unit hours a month)");
        assert_eq!(found[0].impact, Impact::Medium);
        let found = table_findings(&table("old", true, 0, 0, 30.0), &none, &none, 14);
        assert_eq!(found[0].summary, "no reads or writes in 14 days, 30.0 GB stored");
        assert_eq!(found[0].impact, Impact::Low);
        assert!(table_findings(&table("tiny", true, 0, 0, 0.01), &none, &none, 14).is_empty());
    }

    #[test]
    fn steady_on_demand_tables() {
        // 600 units/s on average, 900 at peak: ≈ 1.6 B request units a month.
        let found = table_findings(&table("orders", true, 0, 0, 5.0), &traffic(500.0, 700.0, 14), &traffic(100.0, 200.0, 14), 14);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].impact, Impact::Medium);
        assert_eq!(found[0].recommendation, "Consider provisioned capacity with auto scaling around 900 units/s");
        // Spiky traffic is what on demand is for.
        assert!(table_findings(&table("users", true, 0, 0, 5.0), &traffic(50.0, 400.0, 14), &traffic(0.0, 0.0, 14), 14).is_empty());
    }

    #[test]
    fn quiet_large_tables_for_standard_ia() {
        let found = table_findings(&table("archive", true, 0, 0, 380.0), &traffic(2.0, 9.0, 14), &traffic(1.0, 3.0, 14), 14);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].reduction, "380 GB stored in the Standard class");
        assert_eq!(found[0].impact, Impact::Medium);
        let mut ia = table("archive", true, 0, 0, 380.0);
        ia.table_class_summary = Some(TableClassSummary { table_class: Some("STANDARD_INFREQUENT_ACCESS".into()) });
        assert!(table_findings(&ia, &traffic(2.0, 9.0, 14), &traffic(1.0, 3.0, 14), 14).is_empty());
        // Busy for its size: storage does not dominate.
        assert!(table_findings(&table("hot", true, 0, 0, 60.0), &traffic(40.0, 200.0, 14), &traffic(0.0, 0.0, 14), 14).is_empty());
    }
}

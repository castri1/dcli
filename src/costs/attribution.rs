//! A usage type's cost split over the resources behind it, by what each one used (its own
//! CloudWatch metrics or configuration): the functions, log groups, tables… that cost most.
//! An allocation, not a line of the bill: the bill knows the usage type's total, and dcli
//! shares it out by usage. Reads CloudWatch and the services' configuration only, never
//! Cost Explorer.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Serialize;

use super::{Snapshot, UsageCost};
use crate::aws::apigateway::{self as apigw, Kind};
use crate::aws::lambda::FunctionConfiguration;
use crate::aws::metrics::Series;
use crate::aws::{self, Aws, dms, dynamodb, rds, redshift, s3, sfn, sqs};
use crate::savings::{self, GB, billing, metric_data, par_map, query, query_id, total};

/// Days of usage measured: the costs cover the last 30.
const DAYS: u32 = 30;
/// Hours in those days.
const HOURS: f64 = 720.0;
/// Usage types split at most, the most expensive first.
const MAX_SPLITS: usize = 12;

/// What one resource used of a usage type, and its part of the cost.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Share {
    /// The module that opens it, and what its `open` takes.
    pub module: &'static str,
    pub target: String,
    pub resource: String,
    /// What it used, in the usage type's unit (see [`Split::by`]).
    pub used: f64,
    pub share: f64,
    pub cost: f64,
}

/// A usage type of the bill split over its resources.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Split {
    /// Cost Explorer's names of the service and the usage type (without region).
    pub service: String,
    pub usage: String,
    /// Its cost over the last 30 days in the environment's region.
    pub cost: f64,
    pub currency: String,
    /// What it is split by.
    pub by: &'static str,
    pub shares: Vec<Share>,
    /// Why it could not be split.
    pub error: Option<String>,
}

/// What one resource used.
struct Used {
    module: &'static str,
    target: String,
    resource: String,
    used: f64,
}

impl Used {
    fn new(module: &'static str, target: impl Into<String>, resource: impl Into<String>, used: f64) -> Used {
        Used { module, target: target.into(), resource: resource.into(), used }
    }
}

/// Reads what each resource used of the usage type named (without region) as given.
type Measure = fn(&Aws, &str) -> aws::Result<Vec<Used>>;

/// (service, usage type or a prefix ending in `:`, what it is split by, how it is measured)
const SPLITS: &[(&str, &str, &str, Measure)] = &[
    ("AWS Lambda", billing::LAMBDA_GBS, "duration × memory, per x86_64 function", lambda_gbs),
    ("AWS Lambda", billing::LAMBDA_GBS_ARM, "duration × memory, per arm64 function", lambda_gbs),
    ("AWS Lambda", billing::LAMBDA_REQUESTS, "invocations, per x86_64 function", lambda_requests),
    ("AWS Lambda", billing::LAMBDA_REQUESTS_ARM, "invocations, per arm64 function", lambda_requests),
    ("AmazonCloudWatch", billing::LOGS_INGESTED, "bytes received, per log group (IncomingBytes)", logs_ingested),
    ("AmazonCloudWatch", billing::LOGS_STORED, "bytes stored, per log group", logs_stored),
    ("Amazon DynamoDB", billing::DDB_RCU_HOURS, "provisioned read capacity × hours, per table", ddb_capacity),
    ("Amazon DynamoDB", billing::DDB_WCU_HOURS, "provisioned write capacity × hours, per table", ddb_capacity),
    ("Amazon DynamoDB", billing::DDB_READ_UNITS, "read units consumed, per on-demand table", ddb_requests),
    ("Amazon DynamoDB", billing::DDB_WRITE_UNITS, "write units consumed, per on-demand table", ddb_requests),
    ("Amazon DynamoDB", billing::DDB_STORAGE, "bytes stored, per Standard table", ddb_storage),
    ("Amazon DynamoDB", billing::DDB_STORAGE_IA, "bytes stored, per Standard-IA table", ddb_storage),
    ("Amazon Simple Storage Service", billing::S3_STANDARD, "bytes in STANDARD, per bucket", s3_standard),
    ("Amazon Simple Queue Service", billing::SQS_REQUESTS, "messages sent, received, deleted and empty receives, per queue", sqs_requests),
    ("Amazon Simple Queue Service", billing::SQS_FIFO_REQUESTS, "messages sent, received, deleted and empty receives, per FIFO queue", sqs_requests),
    ("Amazon API Gateway", billing::APIGW_REST, "requests, per REST API", apigw_requests),
    ("Amazon API Gateway", billing::APIGW_HTTP, "requests, per HTTP API", apigw_requests),
    ("AWS Step Functions", billing::SFN_TRANSITIONS, "executions × states, per Standard state machine", sfn_transitions),
    ("Amazon Relational Database Service", "InstanceUsage:", "hours running, per Aurora instance of the class (stopped ones count none)", rds_instances),
    ("Amazon Relational Database Service", billing::AURORA_SERVERLESS, "capacity used, per Serverless v2 instance", rds_serverless),
    ("Amazon Redshift", "Node:", "nodes × hours, per cluster of the node type (paused ones count none)", redshift_nodes),
    ("AWS Database Migration Service", "InstanceUsg:", "hours, per replication instance of the class", dms_instances),
    ("AWS Database Migration Service", "Multi-AZUsg:", "hours, per Multi-AZ replication instance of the class", dms_instances),
];

fn splitter(service: &str, usage: &str) -> Option<(&'static str, Measure)> {
    SPLITS
        .iter()
        .find(|(s, u, _, _)| *s == service && if u.ends_with(':') { usage.starts_with(u) } else { usage == *u })
        .map(|(_, _, by, measure)| (*by, *measure))
}

/// The usage types of `s` billed in `region` that dcli can split, most expensive first:
/// (service, usage type without region, cost).
pub fn candidates(s: &Snapshot, region: &str) -> Vec<(String, String, f64)> {
    let mut by_usage: BTreeMap<(String, String), f64> = BTreeMap::new();
    for UsageCost { service, usage_type, cost, .. } in &s.usage {
        if !billing::in_region(usage_type, region) {
            continue;
        }
        let name = billing::split_region(usage_type).1;
        if splitter(service, name).is_some() {
            *by_usage.entry((service.clone(), name.to_string())).or_default() += cost;
        }
    }
    let mut list: Vec<(String, String, f64)> = by_usage.into_iter().filter(|(_, c)| *c > 0.0).map(|((s, u), c)| (s, u, c)).collect();
    list.sort_by(|a, b| b.2.total_cmp(&a.2));
    list.truncate(MAX_SPLITS);
    list
}

/// Splits the most expensive usage types of `s` in the region `aws` reads; `progress` hears
/// how many of how many are done.
pub fn split(aws: &Aws, s: &Snapshot, progress: &(dyn Fn(usize, usize) + Sync)) -> Vec<Split> {
    let list = candidates(s, &aws.region);
    let done = AtomicUsize::new(0);
    let currency = if s.currency.is_empty() { "USD".to_string() } else { s.currency.clone() };
    par_map(&list, |(service, usage, cost)| {
        let Some((by, measure)) = splitter(service, usage) else { unreachable!("candidates have a splitter") };
        let result = measure(aws, usage);
        progress(done.fetch_add(1, Ordering::SeqCst) + 1, list.len());
        let (shares, error) = match result {
            Ok(used) => (shares(used, *cost), None),
            Err(e) => (Vec::new(), Some(e.short())),
        };
        Split { service: service.clone(), usage: usage.clone(), cost: *cost, currency: currency.clone(), by, shares, error }
    })
}

/// Each resource's part of `cost`, by what it used, largest first.
fn shares(used: Vec<Used>, cost: f64) -> Vec<Share> {
    let sum: f64 = used.iter().map(|u| u.used.max(0.0)).sum();
    if sum <= 0.0 {
        return Vec::new();
    }
    let mut out: Vec<Share> = used
        .into_iter()
        .filter(|u| u.used > 0.0)
        .map(|u| Share { share: u.used / sum, cost: cost * u.used / sum, module: u.module, target: u.target, resource: u.resource, used: u.used })
        .collect();
    out.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    out
}

/// The series of `queries` (one per item) over the last 30 days, summed.
fn sums(aws: &Aws, queries: Vec<crate::aws::metrics::Query>) -> aws::Result<Vec<f64>> {
    Ok(metric_data(aws, &queries, DAYS)?.iter().map(total).collect())
}

fn last(s: &Series) -> f64 {
    s.points.last().map_or(0.0, |p| p.1)
}

// ---- measures -----------------------------------------------------------------------------

/// The functions whose usage `usage` bills: the arm64 ones for `…-ARM`, the others otherwise.
fn functions_on(aws: &Aws, usage: &str) -> aws::Result<Vec<FunctionConfiguration>> {
    let arm = usage.ends_with("-ARM");
    Ok(savings::lambda::all_functions(aws)?.into_iter().filter(|f| savings::lambda::on_x86(f) != arm).collect())
}

fn lambda_gbs(aws: &Aws, usage: &str) -> aws::Result<Vec<Used>> {
    let functions = functions_on(aws, usage)?;
    let queries = functions
        .iter()
        .enumerate()
        .map(|(i, f)| query(query_id("dur", i), "AWS/Lambda", "Duration", &[("FunctionName", &f.function_name)], "Sum", 86_400))
        .collect();
    let ms = sums(aws, queries)?;
    Ok(functions
        .iter()
        .zip(ms)
        .map(|(f, ms)| Used::new("Lambda", &f.function_name, &f.function_name, ms / 1000.0 * savings::lambda::memory_mb(f) as f64 / 1024.0))
        .collect())
}

fn lambda_requests(aws: &Aws, usage: &str) -> aws::Result<Vec<Used>> {
    let functions = functions_on(aws, usage)?;
    let queries = functions
        .iter()
        .enumerate()
        .map(|(i, f)| query(query_id("inv", i), "AWS/Lambda", "Invocations", &[("FunctionName", &f.function_name)], "Sum", 86_400))
        .collect();
    let calls = sums(aws, queries)?;
    Ok(functions.iter().zip(calls).map(|(f, n)| Used::new("Lambda", &f.function_name, &f.function_name, n)).collect())
}

fn logs_ingested(aws: &Aws, _usage: &str) -> aws::Result<Vec<Used>> {
    let groups = savings::logs::all_groups(aws)?;
    let queries = groups
        .iter()
        .enumerate()
        .map(|(i, g)| query(query_id("inb", i), "AWS/Logs", "IncomingBytes", &[("LogGroupName", &g.log_group_name)], "Sum", 86_400))
        .collect();
    let bytes = sums(aws, queries)?;
    Ok(groups.iter().zip(bytes).map(|(g, b)| Used::new("Logs", &g.log_group_name, &g.log_group_name, b / GB)).collect())
}

fn logs_stored(aws: &Aws, _usage: &str) -> aws::Result<Vec<Used>> {
    Ok(savings::logs::all_groups(aws)?
        .iter()
        .map(|g| Used::new("Logs", &g.log_group_name, &g.log_group_name, g.stored_bytes.unwrap_or(0) as f64 / GB))
        .collect())
}

fn tables(aws: &Aws) -> aws::Result<Vec<dynamodb::TableDescription>> {
    let names = savings::dynamodb::all_tables(aws)?;
    par_map(&names, |name| dynamodb::describe_table(aws, name)).into_iter().collect()
}

fn ddb_capacity(aws: &Aws, usage: &str) -> aws::Result<Vec<Used>> {
    let read = usage == billing::DDB_RCU_HOURS;
    Ok(tables(aws)?
        .iter()
        .filter(|t| !t.on_demand())
        .map(|t| {
            let c = t.provisioned_throughput.as_ref();
            let units = if read { c.and_then(|c| c.read_capacity_units) } else { c.and_then(|c| c.write_capacity_units) };
            Used::new("DynamoDB", &t.table_name, &t.table_name, units.unwrap_or(0) as f64 * HOURS)
        })
        .collect())
}

fn ddb_requests(aws: &Aws, usage: &str) -> aws::Result<Vec<Used>> {
    let metric = if usage == billing::DDB_READ_UNITS { "ConsumedReadCapacityUnits" } else { "ConsumedWriteCapacityUnits" };
    let list: Vec<_> = tables(aws)?.into_iter().filter(|t| t.on_demand()).collect();
    let queries =
        list.iter().enumerate().map(|(i, t)| query(query_id("cu", i), "AWS/DynamoDB", metric, &[("TableName", &t.table_name)], "Sum", 86_400)).collect();
    let units = sums(aws, queries)?;
    Ok(list.iter().zip(units).map(|(t, n)| Used::new("DynamoDB", &t.table_name, &t.table_name, n)).collect())
}

fn ddb_storage(aws: &Aws, usage: &str) -> aws::Result<Vec<Used>> {
    let ia = usage == billing::DDB_STORAGE_IA;
    Ok(tables(aws)?
        .iter()
        .filter(|t| (t.table_class() == "STANDARD_INFREQUENT_ACCESS") == ia)
        .map(|t| Used::new("DynamoDB", &t.table_name, &t.table_name, t.table_size_bytes.unwrap_or(0) as f64 / GB))
        .collect())
}

fn s3_standard(aws: &Aws, _usage: &str) -> aws::Result<Vec<Used>> {
    let mut buckets = s3::list_buckets(aws)?;
    buckets.truncate(1000);
    let regions = par_map(&buckets, |b| b.region.clone().map_or_else(|| s3::bucket_region(aws, &b.name).ok(), Some));
    let here: Vec<&s3::Bucket> = buckets.iter().zip(regions).filter(|(_, r)| r.as_deref() == Some(aws.region.as_str())).map(|(b, _)| b).collect();
    let queries = here
        .iter()
        .enumerate()
        .map(|(i, b)| query(query_id("b", i), "AWS/S3", "BucketSizeBytes", &[("BucketName", &b.name), ("StorageType", "StandardStorage")], "Average", 86_400))
        .collect::<Vec<_>>();
    // A daily metric that can lag a day or two: the latest value of the last three days.
    let series = metric_data(aws, &queries, 3)?;
    Ok(here.iter().zip(&series).map(|(b, s)| Used::new("S3", format!("s3://{}/", b.name), &b.name, last(s) / GB)).collect())
}

fn sqs_requests(aws: &Aws, usage: &str) -> aws::Result<Vec<Used>> {
    let fifo = usage == billing::SQS_FIFO_REQUESTS;
    let urls: Vec<String> = sqs::list_queue_urls(aws)?.into_iter().filter(|u| sqs::queue_name(u).ends_with(".fifo") == fifo).collect();
    const METRICS: [&str; 4] = ["NumberOfMessagesSent", "NumberOfMessagesReceived", "NumberOfMessagesDeleted", "NumberOfEmptyReceives"];
    let mut queries = Vec::new();
    for (i, url) in urls.iter().enumerate() {
        for (j, metric) in METRICS.iter().enumerate() {
            queries.push(query(query_id("q", i * METRICS.len() + j), "AWS/SQS", metric, &[("QueueName", sqs::queue_name(url))], "Sum", 86_400));
        }
    }
    let counts = sums(aws, queries)?;
    Ok(urls.iter().zip(counts.chunks(METRICS.len())).map(|(url, c)| Used::new("SQS", url, sqs::queue_name(url), c.iter().sum())).collect())
}

fn apigw_requests(aws: &Aws, usage: &str) -> aws::Result<Vec<Used>> {
    let rest = usage == billing::APIGW_REST;
    let apis: Vec<apigw::Api> = if rest {
        apigw::list_rest_apis(aws)?
    } else {
        apigw::list_v2_apis(aws)?.into_iter().filter(|a| a.kind == Kind::Http).collect()
    };
    let queries = apis
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let dims = if rest { [("ApiName", a.name.as_str())] } else { [("ApiId", a.id.as_str())] };
            query(query_id("api", i), "AWS/ApiGateway", "Count", &dims, "Sum", 86_400)
        })
        .collect();
    let counts = sums(aws, queries)?;
    Ok(apis.iter().zip(counts).map(|(a, n)| Used::new("API Gateway", &a.id, &a.name, n)).collect())
}

fn sfn_transitions(aws: &Aws, _usage: &str) -> aws::Result<Vec<Used>> {
    let machines: Vec<sfn::StateMachine> = sfn::all_state_machines(aws)?.into_iter().filter(|m| !m.express()).collect();
    let queries = machines
        .iter()
        .enumerate()
        .map(|(i, m)| query(query_id("ex", i), "AWS/States", "ExecutionsStarted", &[("StateMachineArn", &m.state_machine_arn)], "Sum", 86_400))
        .collect();
    let started = sums(aws, queries)?;
    let busy: Vec<(&sfn::StateMachine, f64)> = machines.iter().zip(started).filter(|(_, n)| *n > 0.0).take(200).collect();
    let states = par_map(&busy, |(m, _)| {
        sfn::describe_state_machine(aws, &m.state_machine_arn)
            .ok()
            .and_then(|d| d.definition)
            .and_then(|d| serde_json::from_str(&d).ok())
            .map_or(1, |v| savings::sfn::count_states(&v).max(1))
    });
    Ok(busy.iter().zip(states).map(|((m, n), states)| Used::new("Step Functions", &m.state_machine_arn, &m.name, n * states as f64)).collect())
}

/// `InstanceUsage:db.r6g.large` → `db.r6g.large`.
fn class_of(usage: &str) -> &str {
    usage.split_once(':').map_or(usage, |(_, c)| c)
}

fn rds_instances(aws: &Aws, usage: &str) -> aws::Result<Vec<Used>> {
    let class = class_of(usage);
    let inv = rds::inventory(aws, None)?;
    Ok(inv
        .instances
        .iter()
        .filter(|i| i.class == class)
        .map(|i| {
            let stopped = inv.clusters.iter().any(|c| c.id == i.cluster && c.status == "stopped") || i.status == "stopped";
            Used::new("Aurora", &i.cluster, format!("{} / {}", i.cluster, i.id), if stopped { 0.0 } else { HOURS })
        })
        .collect())
}

fn rds_serverless(aws: &Aws, _usage: &str) -> aws::Result<Vec<Used>> {
    let inv = rds::inventory(aws, None)?;
    let list: Vec<&rds::Instance> = inv.instances.iter().filter(|i| i.serverless()).collect();
    let queries = list
        .iter()
        .enumerate()
        .map(|(i, inst)| query(query_id("acu", i), "AWS/RDS", "ServerlessDatabaseCapacity", &[("DBInstanceIdentifier", &inst.id)], "Average", 86_400))
        .collect::<Vec<_>>();
    let series = metric_data(aws, &queries, DAYS)?;
    Ok(list
        .iter()
        .zip(&series)
        .map(|(i, s)| Used::new("Aurora", &i.cluster, format!("{} / {}", i.cluster, i.id), s.points.iter().map(|p| p.1 * 24.0).sum()))
        .collect())
}

fn redshift_nodes(aws: &Aws, usage: &str) -> aws::Result<Vec<Used>> {
    let node_type = class_of(usage);
    Ok(redshift::describe_clusters(aws, None)?
        .iter()
        .filter(|c| c.node_type == node_type)
        .map(|c| Used::new("Redshift", &c.id, &c.id, if c.paused() { 0.0 } else { f64::from(c.nodes_count) * HOURS }))
        .collect())
}

fn dms_instances(aws: &Aws, usage: &str) -> aws::Result<Vec<Used>> {
    let class = class_of(usage);
    let standby = usage.starts_with("Multi-AZUsg:");
    Ok(dms::list_instances(aws)?
        .iter()
        .filter(|i| i.class == class && (!standby || i.multi_az))
        .map(|i| Used::new("DMS", &i.id, &i.id, HOURS))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(service: &str, usage_type: &str, cost: f64) -> UsageCost {
        UsageCost { service: service.into(), usage_type: usage_type.into(), cost, quantity: 1.0, unit: String::new() }
    }

    #[test]
    fn candidates_are_the_region_s_splittable_usage_types() {
        let s = Snapshot {
            usage: vec![
                usage("AWS Lambda", "Lambda-GB-Second", 300.0),
                usage("AWS Lambda", "USE1-Lambda-GB-Second", 100.0),
                usage("AWS Lambda", "USW2-Lambda-GB-Second", 900.0),
                usage("AmazonCloudWatch", "DataProcessing-Bytes", 500.0),
                usage("Amazon Relational Database Service", "InstanceUsage:db.r6g.large", 200.0),
                usage("Amazon Relational Database Service", "Aurora:StorageUsage", 80.0),
                usage("AWS Lambda", "Lambda-Edge-Request", 50.0),
            ],
            ..Default::default()
        };
        let list = candidates(&s, "us-east-1");
        let names: Vec<(&str, f64)> = list.iter().map(|(_, u, c)| (u.as_str(), *c)).collect();
        assert_eq!(names, [("DataProcessing-Bytes", 500.0), ("Lambda-GB-Second", 400.0), ("InstanceUsage:db.r6g.large", 200.0)]);
        assert_eq!(candidates(&s, "us-west-2")[0], ("AWS Lambda".to_string(), "Lambda-GB-Second".to_string(), 900.0));
    }

    #[test]
    fn shares_split_the_cost_by_use() {
        let used = vec![
            Used::new("Lambda", "a", "a", 30.0),
            Used::new("Lambda", "b", "b", 0.0),
            Used::new("Lambda", "c", "c", 90.0),
        ];
        let s = shares(used, 240.0);
        assert_eq!(s.len(), 2, "what used nothing is left out");
        assert_eq!((s[0].resource.as_str(), s[0].share, s[0].cost), ("c", 0.75, 180.0));
        assert_eq!((s[1].resource.as_str(), s[1].cost), ("a", 60.0));
        assert!(shares(vec![Used::new("Lambda", "a", "a", 0.0)], 10.0).is_empty());
        assert_eq!(class_of("InstanceUsage:db.r6g.large"), "db.r6g.large");
        assert_eq!(splitter("Amazon Redshift", "Node:ra3.4xlarge").map(|s| s.0), Some("nodes × hours, per cluster of the node type (paused ones count none)"));
        assert!(splitter("Amazon Redshift", "RMS:ra3").is_none());
    }
}

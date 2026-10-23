//! The usage types findings remove, as Cost Explorer names them: one table, so the names are
//! easy to check and correct. With the account's costs read ([`crate::costs::Snapshot`]),
//! a finding's monthly quantity times the account's own rate for its usage type gives dollars.
//!
//! Cost Explorer prefixes usage types with their region (`USW2-Lambda-GB-Second`; us-east-1
//! often goes without): the names here have no prefix, and [`crate::costs::usage_is`] matches
//! any region. Each comment says the unit Cost Explorer reports, which is the unit the
//! findings measure in.

/// Lambda duration on x86_64 (GB-seconds; GB of 1,024 MB).
pub const LAMBDA_GBS: &str = "Lambda-GB-Second";
/// Lambda duration on arm64 (GB-seconds).
pub const LAMBDA_GBS_ARM: &str = "Lambda-GB-Second-ARM";
/// Lambda requests (requests).
pub const LAMBDA_REQUESTS: &str = "Request";
pub const LAMBDA_REQUESTS_ARM: &str = "Request-ARM";
/// Provisioned concurrency kept configured (GB-seconds), x86_64 and arm64.
pub const LAMBDA_PROVISIONED: &str = "Lambda-Provisioned-Concurrency";
pub const LAMBDA_PROVISIONED_ARM: &str = "Lambda-Provisioned-Concurrency-ARM";
/// CloudWatch Logs ingestion (GB).
pub const LOGS_INGESTED: &str = "DataProcessing-Bytes";
/// CloudWatch Logs storage (GB-Mo).
pub const LOGS_STORED: &str = "TimedStorage-ByteHrs";
/// CloudWatch alarms (alarm-months).
pub const ALARMS: &str = "CW:AlarmMonitorUsage";
/// SQS requests, standard and FIFO queues (requests).
pub const SQS_REQUESTS: &str = "Requests-Tier1";
pub const SQS_FIFO_REQUESTS: &str = "Requests-FIFO-Tier1";
/// DynamoDB provisioned capacity (capacity-unit hours).
pub const DDB_RCU_HOURS: &str = "ReadCapacityUnit-Hrs";
pub const DDB_WCU_HOURS: &str = "WriteCapacityUnit-Hrs";
/// DynamoDB on-demand requests (request units).
pub const DDB_READ_UNITS: &str = "ReadRequestUnits";
pub const DDB_WRITE_UNITS: &str = "WriteRequestUnits";
/// DynamoDB storage, Standard and Standard-IA table classes (GB-Mo).
pub const DDB_STORAGE: &str = "TimedStorage-ByteHrs";
pub const DDB_STORAGE_IA: &str = "IA-TimedStorage-ByteHrs";
/// S3 Standard storage (GB-Mo) and Standard-IA.
pub const S3_STANDARD: &str = "TimedStorage-ByteHrs";
pub const S3_STANDARD_IA: &str = "TimedStorage-SIA-ByteHrs";
/// Step Functions Standard workflows (state transitions).
pub const SFN_TRANSITIONS: &str = "StateTransition";
/// Secrets Manager (secret-months).
pub const SECRETS: &str = "AWSSecretsManager-Secrets";
/// API Gateway requests to REST and to HTTP APIs (requests).
pub const APIGW_REST: &str = "ApiGatewayRequest";
pub const APIGW_HTTP: &str = "ApiGatewayHttpRequest";
/// Aurora Serverless v2 capacity (ACU-hours), I/O on Standard storage (requests), backups
/// and manual snapshots beyond the free share (GB-Mo).
pub const AURORA_SERVERLESS: &str = "Aurora:ServerlessV2Usage";
pub const AURORA_IO: &str = "Aurora:StorageIOUsage";
pub const AURORA_BACKUP: &str = "Aurora:BackupUsage";

/// An API Gateway stage cache of `size` GB, by the hour (`0.5`, `6.1`…).
pub fn apigw_cache(size: &str) -> String {
    format!("ApiGatewayCacheUsage:{size}")
}

/// An RDS or Aurora instance of `class` (`db.r6g.large`), by the hour.
pub fn rds_instance(class: &str) -> String {
    format!("InstanceUsage:{class}")
}

/// A Redshift node of `node_type` (`ra3.4xlarge`), by the hour.
pub fn redshift_node(node_type: &str) -> String {
    format!("Node:{node_type}")
}

/// A DMS replication instance of `class` (`dms.r5.large`), by the hour, and its Multi-AZ standby.
pub fn dms_instance(class: &str) -> String {
    format!("InstanceUsg:{class}")
}

pub fn dms_standby(class: &str) -> String {
    format!("Multi-AZUsg:{class}")
}

/// Usage types whose price falls in tiers as usage grows: an average rate prices the last
/// units a little differently.
pub fn tiered(usage: &str) -> bool {
    [LOGS_INGESTED, LOGS_STORED, S3_STANDARD, S3_STANDARD_IA, SQS_REQUESTS, APIGW_REST, APIGW_HTTP].contains(&usage)
}

/// The prefix Cost Explorer gives a region's usage types (`USW2`), for those dcli knows;
/// us-east-1's often have none.
pub fn region_prefix(region: &str) -> Option<&'static str> {
    Some(match region {
        "us-east-1" => "USE1",
        "us-east-2" => "USE2",
        "us-west-1" => "USW1",
        "us-west-2" => "USW2",
        "ca-central-1" => "CAN1",
        "sa-east-1" => "SAE1",
        "eu-west-1" => "EU",
        "eu-west-2" => "EUW2",
        "eu-west-3" => "EUW3",
        "eu-central-1" => "EUC1",
        "eu-north-1" => "EUN1",
        "eu-south-1" => "EUS1",
        "ap-northeast-1" => "APN1",
        "ap-northeast-2" => "APN2",
        "ap-northeast-3" => "APN3",
        "ap-southeast-1" => "APS1",
        "ap-southeast-2" => "APS2",
        "ap-south-1" => "APS3",
        "ap-east-1" => "APE1",
        "me-south-1" => "MES1",
        "af-south-1" => "AFS1",
        _ => return None,
    })
}

/// A usage type without its region prefix: `USW2-Lambda-GB-Second` → (`USW2`, `Lambda-GB-Second`).
/// Region prefixes are capitals with a digit (`USE1`, `APN1`), or `EU`: `IA-TimedStorage…`
/// keeps its `IA`.
pub fn split_region(usage_type: &str) -> (Option<&str>, &str) {
    match usage_type.split_once('-') {
        Some((prefix, rest))
            if prefix == "EU"
                || ((3..=5).contains(&prefix.len())
                    && prefix.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                    && prefix.chars().any(|c| c.is_ascii_digit())
                    && prefix.chars().any(|c| c.is_ascii_uppercase())) =>
        {
            (Some(prefix), rest)
        }
        _ => (None, usage_type),
    }
}

/// Whether `usage_type` (as Cost Explorer has it) is billed in `region`: its prefix is the
/// region's, or it has none and the region is us-east-1. Unknown regions accept any.
pub fn in_region(usage_type: &str, region: &str) -> bool {
    match (split_region(usage_type).0, region_prefix(region)) {
        (_, None) => true,
        (Some(prefix), Some(want)) => prefix == want,
        (None, Some(_)) => region == "us-east-1",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_of_usage_types() {
        assert_eq!(split_region("USW2-Lambda-GB-Second"), (Some("USW2"), "Lambda-GB-Second"));
        assert_eq!(split_region("EU-DataProcessing-Bytes"), (Some("EU"), "DataProcessing-Bytes"));
        assert_eq!(split_region("Lambda-GB-Second"), (None, "Lambda-GB-Second"));
        assert_eq!(split_region("CW:AlarmMonitorUsage"), (None, "CW:AlarmMonitorUsage"));
        assert_eq!(split_region("TimedStorage-ByteHrs"), (None, "TimedStorage-ByteHrs"));
        assert_eq!(split_region("IA-TimedStorage-ByteHrs"), (None, "IA-TimedStorage-ByteHrs"));
        assert_eq!(split_region("USE1-IA-TimedStorage-ByteHrs"), (Some("USE1"), "IA-TimedStorage-ByteHrs"));
        assert!(in_region("Lambda-GB-Second", "us-east-1"));
        assert!(in_region("USE1-Lambda-GB-Second", "us-east-1"));
        assert!(!in_region("USW2-Lambda-GB-Second", "us-east-1"));
        assert!(in_region("USW2-Lambda-GB-Second", "us-west-2"));
        assert!(!in_region("Lambda-GB-Second", "us-west-2"));
        assert!(in_region("XYZ9-Thing", "mars-1"));
        assert_eq!(rds_instance("db.r6g.large"), "InstanceUsage:db.r6g.large");
        assert!(tiered(LOGS_INGESTED) && !tiered(LAMBDA_GBS));
    }
}

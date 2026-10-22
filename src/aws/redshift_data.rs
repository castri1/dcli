//! Redshift Data API (awsJson1_1, `X-Amz-Target: RedshiftData.<Operation>`): SQL on a cluster
//! without a driver (ExecuteStatement, DescribeStatement, GetStatementResult), used to read
//! system tables.

use super::Service;

#[allow(dead_code)] // used by the Redshift module
pub const SERVICE: Service = Service { endpoint: "redshift-data", signing_name: "redshift-data", target_prefix: "RedshiftData" };

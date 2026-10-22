//! Redshift (awsQuery protocol, API version 2012-12-01): provisioned clusters, their nodes,
//! snapshots and events; pause, resume, resize.

use super::Service;

#[allow(dead_code)] // used by the Redshift module
pub const SERVICE: Service = Service { endpoint: "redshift", signing_name: "redshift", target_prefix: "" };
#[allow(dead_code)]
pub const VERSION: &str = "2012-12-01";

//! RDS (awsQuery protocol, API version 2014-10-31): Aurora clusters, their instances,
//! endpoints and events.

use super::Service;

#[allow(dead_code)] // used by the Aurora module
pub const SERVICE: Service = Service { endpoint: "rds", signing_name: "rds", target_prefix: "" };
#[allow(dead_code)]
pub const VERSION: &str = "2014-10-31";

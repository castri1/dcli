//! STS (awsQuery protocol, API version 2011-06-15): the account a profile signs in to
//! (GetCallerIdentity), which says whose costs are an environment's.

use super::Service;

#[allow(dead_code)] // used by the costs work
pub const SERVICE: Service = Service { endpoint: "sts", signing_name: "sts", target_prefix: "" };
#[allow(dead_code)]
pub const VERSION: &str = "2011-06-15";

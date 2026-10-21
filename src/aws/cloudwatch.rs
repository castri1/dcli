//! CloudWatch (awsQuery protocol, API version 2010-08-01): the service shared by
//! [`super::alarms`] (DescribeAlarms) and [`super::metrics`] (GetMetricData).

use super::Service;

pub const SERVICE: Service = Service { endpoint: "monitoring", signing_name: "monitoring", target_prefix: "" };
pub const VERSION: &str = "2010-08-01";

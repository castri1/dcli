//! Performance Insights (awsJson1_1, `X-Amz-Target: PerformanceInsightsv20180227.<Operation>`):
//! database load, top SQL and wait events of an Aurora instance.

use super::Service;

#[allow(dead_code)] // used by the Aurora module
pub const SERVICE: Service = Service { endpoint: "pi", signing_name: "pi", target_prefix: "PerformanceInsightsv20180227" };

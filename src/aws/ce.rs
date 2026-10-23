//! Cost Explorer (awsJson1_1, `X-Amz-Target: AWSInsightsIndexService.<Operation>`), served
//! from us-east-1. AWS bills every request: see `crate::costs`.

use super::Service;

#[allow(dead_code)] // used by the costs work
pub const SERVICE: Service = Service { endpoint: "ce", signing_name: "ce", target_prefix: "AWSInsightsIndexService" };

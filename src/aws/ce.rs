//! Cost Explorer (awsJson1_1, `X-Amz-Target: AWSInsightsIndexService.<Operation>`), served
//! from us-east-1. AWS bills every request: the calls go through `crate::costs::explorer`,
//! which counts them.

use super::Service;

pub const SERVICE: Service = Service { endpoint: "ce", signing_name: "ce", target_prefix: "AWSInsightsIndexService" };

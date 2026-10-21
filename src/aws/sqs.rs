//! SQS (awsJson1_0, `X-Amz-Target: AmazonSQS.<Operation>`).

use super::Service;

#[allow(dead_code)] // used by the SQS module
pub const SERVICE: Service = Service { endpoint: "sqs", signing_name: "sqs", target_prefix: "AmazonSQS" };

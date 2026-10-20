//! EventBridge (awsJson1_1): buses, rules, targets, TestEventPattern, PutEvents.

use super::Service;

pub const SERVICE: Service = Service { endpoint: "events", signing_name: "events", target_prefix: "AWSEvents" };

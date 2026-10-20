//! DynamoDB (awsJson1_0): tables, Query, Scan, GetItem; DynamoDB JSON ↔ plain JSON.

use super::Service;

pub const SERVICE: Service = Service { endpoint: "dynamodb", signing_name: "dynamodb", target_prefix: "DynamoDB_20120810" };

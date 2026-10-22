//! API Gateway (restJson1). REST APIs live under `/restapis`, HTTP and WebSocket APIs
//! under `/v2/apis`; both on the same endpoint and signing name.

use super::Service;

#[allow(dead_code)] // used by the API Gateway module
pub const SERVICE: Service = Service { endpoint: "apigateway", signing_name: "apigateway", target_prefix: "" };

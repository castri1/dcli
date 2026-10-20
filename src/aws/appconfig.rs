//! AppConfig (restJson1): applications, environments, profiles, hosted versions, deployments.

use super::Service;

pub const SERVICE: Service = Service { endpoint: "appconfig", signing_name: "appconfig", target_prefix: "" };

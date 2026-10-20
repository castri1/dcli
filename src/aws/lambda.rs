use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service { endpoint: "lambda", signing_name: "lambda", target_prefix: "" };

const API: &str = "2015-03-31";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct FunctionConfiguration {
    pub function_name: String,
    pub function_arn: String,
    pub runtime: Option<String>,
    pub role: Option<String>,
    pub handler: Option<String>,
    pub code_size: i64,
    pub description: Option<String>,
    pub timeout: Option<i64>,
    pub memory_size: Option<i64>,
    /// e.g. `2024-01-31T10:00:00.000+0000`
    pub last_modified: Option<String>,
    pub version: Option<String>,
    pub environment: Option<EnvironmentVars>,
    pub tracing_config: Option<TracingConfig>,
    pub layers: Vec<Layer>,
    pub state: Option<String>,
    pub state_reason: Option<String>,
    pub last_update_status: Option<String>,
    pub package_type: Option<String>,
    pub architectures: Vec<String>,
    pub ephemeral_storage: Option<EphemeralStorage>,
    pub vpc_config: Option<VpcConfig>,
    pub logging_config: Option<LoggingConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct EnvironmentVars {
    pub variables: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TracingConfig {
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Layer {
    pub arn: String,
    pub code_size: i64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct EphemeralStorage {
    pub size: i64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct VpcConfig {
    pub subnet_ids: Vec<String>,
    pub security_group_ids: Vec<String>,
    pub vpc_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LoggingConfig {
    pub log_format: Option<String>,
    pub log_group: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct FunctionDetail {
    pub configuration: FunctionConfiguration,
    pub tags: BTreeMap<String, String>,
    pub concurrency: Option<Concurrency>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Concurrency {
    pub reserved_concurrent_executions: Option<i64>,
}

/// One page of functions. Returns the marker for the next page, if any.
pub fn list_functions(aws: &Aws, marker: Option<&str>) -> Result<(Vec<FunctionConfiguration>, Option<String>)> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        functions: Vec<FunctionConfiguration>,
        next_marker: Option<String>,
    }
    let mut query = vec![("MaxItems".to_string(), "50".to_string())];
    if let Some(m) = marker {
        query.push(("Marker".to_string(), m.to_string()));
    }
    let out: Out = aws.rest(&SERVICE, "GET", &[API, "functions", ""], &query, None)?;
    Ok((out.functions, out.next_marker.filter(|m| !m.is_empty())))
}

pub fn get_function(aws: &Aws, name: &str) -> Result<FunctionDetail> {
    aws.rest(&SERVICE, "GET", &[API, "functions", name], &[], None)
}

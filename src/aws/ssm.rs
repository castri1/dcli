use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service {
    endpoint: "ssm",
    signing_name: "ssm",
    target_prefix: "AmazonSSM",
};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ParameterMetadata {
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub last_modified_date: Option<f64>,
    pub last_modified_user: Option<String>,
    pub version: Option<i64>,
    pub tier: Option<String>,
    pub description: Option<String>,
    pub data_type: Option<String>,
    pub key_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Parameter {
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub value: String,
    pub version: Option<i64>,
    pub last_modified_date: Option<f64>,
    #[serde(rename = "ARN")]
    pub arn: Option<String>,
}

/// One page of parameter metadata (no values). Returns the next token, if any.
pub fn describe_parameters(aws: &Aws, token: Option<&str>) -> Result<(Vec<ParameterMetadata>, Option<String>)> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        parameters: Vec<ParameterMetadata>,
        next_token: Option<String>,
    }
    let mut input = json!({ "MaxResults": 50 });
    if let Some(t) = token {
        input["NextToken"] = json!(t);
    }
    let out: Out = aws.call(&SERVICE, "DescribeParameters", &input)?;
    Ok((out.parameters, out.next_token.filter(|t| !t.is_empty())))
}

pub fn get_parameter(aws: &Aws, name: &str, decrypt: bool) -> Result<Parameter> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        parameter: Parameter,
    }
    let out: Out = aws.call(&SERVICE, "GetParameter", &json!({ "Name": name, "WithDecryption": decrypt }))?;
    Ok(out.parameter)
}

/// Metadata of one parameter (type, KMS key, description), or `None` when it does not exist.
pub fn describe_parameter(aws: &Aws, name: &str) -> Result<Option<ParameterMetadata>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        parameters: Vec<ParameterMetadata>,
    }
    let input = json!({ "ParameterFilters": [{ "Key": "Name", "Option": "Equals", "Values": [name] }] });
    let out: Out = aws.call(&SERVICE, "DescribeParameters", &input)?;
    Ok(out.parameters.into_iter().find(|p| p.name == name))
}

/// Creates a parameter, or with `overwrite` replaces its value (a new version). `key_id`
/// keeps a SecureString on its KMS key. Returns the new version.
pub fn put_parameter(
    aws: &Aws,
    name: &str,
    value: &str,
    kind: &str,
    description: Option<&str>,
    key_id: Option<&str>,
    overwrite: bool,
) -> Result<i64> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        version: i64,
    }
    let mut input = json!({ "Name": name, "Value": value, "Type": kind, "Overwrite": overwrite });
    if let Some(d) = description.filter(|d| !d.is_empty()) {
        input["Description"] = json!(d);
    }
    if let Some(k) = key_id.filter(|_| kind == "SecureString") {
        input["KeyId"] = json!(k);
    }
    let out: Out = aws.call(&SERVICE, "PutParameter", &input)?;
    Ok(out.version)
}

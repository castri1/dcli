//! Secrets Manager: list, describe, read, create and update secrets.

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service {
    endpoint: "secretsmanager",
    signing_name: "secretsmanager",
    target_prefix: "secretsmanager",
};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Secret {
    #[serde(rename = "ARN")]
    pub arn: String,
    pub name: String,
    pub description: Option<String>,
    pub kms_key_id: Option<String>,
    pub rotation_enabled: Option<bool>,
    pub created_date: Option<f64>,
    pub last_changed_date: Option<f64>,
    pub last_accessed_date: Option<f64>,
    pub last_rotated_date: Option<f64>,
    pub owning_service: Option<String>,
    pub tags: Vec<Tag>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Tag {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SecretValue {
    #[serde(rename = "ARN")]
    pub arn: String,
    pub name: String,
    pub version_id: Option<String>,
    pub secret_string: Option<String>,
    /// Base64, for binary secrets.
    pub secret_binary: Option<String>,
    pub version_stages: Vec<String>,
    pub created_date: Option<f64>,
}

/// One page of secrets (metadata only). Returns the next token, if any.
pub fn list_secrets(aws: &Aws, token: Option<&str>) -> Result<(Vec<Secret>, Option<String>)> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        secret_list: Vec<Secret>,
        next_token: Option<String>,
    }
    let mut input = json!({ "MaxResults": 100 });
    if let Some(t) = token {
        input["NextToken"] = json!(t);
    }
    let out: Out = aws.call(&SERVICE, "ListSecrets", &input)?;
    Ok((out.secret_list, out.next_token.filter(|t| !t.is_empty())))
}

pub fn describe_secret(aws: &Aws, id: &str) -> Result<Secret> {
    aws.call(&SERVICE, "DescribeSecret", &json!({ "SecretId": id }))
}

pub fn get_secret_value(aws: &Aws, id: &str) -> Result<SecretValue> {
    aws.call(&SERVICE, "GetSecretValue", &json!({ "SecretId": id }))
}

/// Creates a secret with a string value; returns its ARN.
pub fn create_secret(aws: &Aws, name: &str, value: &str, description: Option<&str>) -> Result<String> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        #[serde(rename = "ARN")]
        arn: String,
    }
    let mut input = json!({
        "Name": name,
        "SecretString": value,
        // Raw API calls must send their own idempotency token (the SDKs generate one).
        "ClientRequestToken": crate::util::request_token(),
    });
    if let Some(d) = description.filter(|d| !d.is_empty()) {
        input["Description"] = json!(d);
    }
    let out: Out = aws.call(&SERVICE, "CreateSecret", &input)?;
    Ok(out.arn)
}

/// Stores a new current version of a secret; returns the version id.
pub fn put_secret_value(aws: &Aws, id: &str, value: &str) -> Result<String> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        version_id: String,
    }
    let input = json!({ "SecretId": id, "SecretString": value, "ClientRequestToken": crate::util::request_token() });
    let out: Out = aws.call(&SERVICE, "PutSecretValue", &input)?;
    Ok(out.version_id)
}

/// Console page of a secret.
pub fn console_url(region: &str, name: &str) -> String {
    super::console_url(
        region,
        &format!("secretsmanager/secret?name={}&region={region}", crate::util::uri_encode(name)),
    )
}

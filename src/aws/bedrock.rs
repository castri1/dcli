//! Amazon Bedrock (restJson1 control plane), read only: where model invocations are logged and
//! the provisioned throughput bought; and, through CloudWatch, which models were invoked
//! (`AWS/Bedrock` metrics by `ModelId`).

use serde::Deserialize;

use super::xml::{El, result_of};
use super::{Aws, Result, Service, cloudwatch};

pub const SERVICE: Service = Service { endpoint: "bedrock", signing_name: "bedrock", target_prefix: "" };

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LoggingConfig {
    pub cloud_watch_config: Option<CloudWatchConfig>,
    pub s3_config: Option<S3Config>,
    /// Prompts and completions are written whole.
    pub text_data_delivery_enabled: bool,
    pub image_data_delivery_enabled: bool,
    pub embedding_data_delivery_enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CloudWatchConfig {
    pub log_group_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct S3Config {
    pub bucket_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProvisionedThroughput {
    pub provisioned_model_name: String,
    pub provisioned_model_arn: String,
    pub model_arn: String,
    pub foundation_model_arn: String,
    pub model_units: i64,
    /// `InService`, `Creating`, `Updating`, `Failed`.
    pub status: String,
    /// `OneMonth`, `SixMonths`, or empty for no commitment.
    pub commitment_duration: String,
    pub commitment_expiration_time: Option<String>,
}

/// How model invocations are logged (`None`: they are not).
pub fn logging(aws: &Aws) -> Result<Option<LoggingConfig>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        logging_config: Option<LoggingConfig>,
    }
    let out: Out = aws.rest(&SERVICE, "GET", &["logging", "modelinvocations"], &[], None)?;
    Ok(out.logging_config)
}

pub fn provisioned(aws: &Aws) -> Result<Vec<ProvisionedThroughput>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        provisioned_model_summaries: Vec<ProvisionedThroughput>,
        next_token: Option<String>,
    }
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..20 {
        let query: Vec<(String, String)> = token.iter().map(|t| ("nextToken".to_string(), t.clone())).collect();
        let out: Out = aws.rest(&SERVICE, "GET", &["provisioned-model-throughputs"], &query, None)?;
        all.extend(out.provisioned_model_summaries);
        match out.next_token.filter(|t| !t.is_empty()) {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    Ok(all)
}

/// The model ids with `AWS/Bedrock` metrics of the last two weeks (CloudWatch ListMetrics):
/// the models this account invoked, foundation models and inference profiles alike.
pub fn invoked_models(aws: &Aws) -> Result<Vec<String>> {
    let mut models: Vec<String> = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..20 {
        let mut params = vec![("Namespace", "AWS/Bedrock"), ("MetricName", "InputTokenCount")];
        if let Some(t) = &token {
            params.push(("NextToken", t));
        }
        let xml = aws.query(&cloudwatch::SERVICE, "ListMetrics", cloudwatch::VERSION, &params)?;
        let result = result_of(&xml, "ListMetrics");
        for metric in result.list("Metrics") {
            let dims: Vec<El> = metric.list("Dimensions");
            // Only the series by model alone: others add a dimension.
            if let [d] = dims.as_slice()
                && d.string("Name") == "ModelId"
            {
                let id = d.string("Value");
                if !id.is_empty() && !models.contains(&id) {
                    models.push(id);
                }
            }
        }
        token = result.text("NextToken");
        if token.is_none() {
            break;
        }
    }
    models.sort();
    Ok(models)
}

use serde::Deserialize;
use serde_json::json;

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service {
    endpoint: "codebuild",
    signing_name: "codebuild",
    target_prefix: "CodeBuild_20161006",
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Build {
    pub id: String,
    pub build_status: String,
    pub current_phase: Option<String>,
    pub logs: Option<LogsLocation>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogsLocation {
    pub group_name: Option<String>,
    pub stream_name: Option<String>,
}

pub fn get_build(aws: &Aws, id: &str) -> Result<Option<Build>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        builds: Vec<Build>,
    }
    let out: Out = aws.call(&SERVICE, "BatchGetBuilds", &json!({ "ids": [id] }))?;
    Ok(out.builds.into_iter().next())
}

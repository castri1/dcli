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
    pub phases: Vec<BuildPhase>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogsLocation {
    pub group_name: Option<String>,
    pub stream_name: Option<String>,
    /// Console link to the log stream.
    pub deep_link: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BuildPhase {
    /// `INSTALL`, `BUILD`, `POST_BUILD`, …
    pub phase_type: String,
    pub phase_status: Option<String>,
    pub duration_in_seconds: Option<i64>,
    pub contexts: Vec<PhaseContext>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PhaseContext {
    pub status_code: Option<String>,
    pub message: Option<String>,
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

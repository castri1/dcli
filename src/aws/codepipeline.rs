use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service {
    endpoint: "codepipeline",
    signing_name: "codepipeline",
    target_prefix: "CodePipeline_20150709",
};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PipelineState {
    pub pipeline_name: String,
    pub stage_states: Vec<StageState>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StageState {
    pub stage_name: String,
    pub action_states: Vec<ActionState>,
    pub latest_execution: Option<StageExecution>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StageExecution {
    pub pipeline_execution_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActionState {
    pub action_name: String,
    pub latest_execution: Option<ActionExecution>,
    pub revision_url: Option<String>,
    pub entity_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActionExecution {
    pub action_execution_id: Option<String>,
    pub status: String,
    pub summary: Option<String>,
    pub last_status_change: Option<f64>,
    pub token: Option<String>,
    pub external_execution_id: Option<String>,
    pub external_execution_url: Option<String>,
    pub percent_complete: Option<u32>,
    pub error_details: Option<ErrorDetails>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ErrorDetails {
    pub code: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExecutionSummary {
    pub pipeline_execution_id: String,
    pub status: String,
    pub status_summary: Option<String>,
    pub start_time: Option<f64>,
    pub last_update_time: Option<f64>,
    pub source_revisions: Vec<SourceRevision>,
    pub trigger: Option<Trigger>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SourceRevision {
    pub action_name: String,
    pub revision_id: Option<String>,
    pub revision_summary: Option<String>,
    pub revision_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Trigger {
    pub trigger_type: Option<String>,
    pub trigger_detail: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActionExecutionDetail {
    pub pipeline_execution_id: String,
    pub stage_name: String,
    pub action_name: String,
    pub start_time: Option<f64>,
    pub last_update_time: Option<f64>,
    pub status: String,
    pub input: Option<ActionInput>,
    pub output: Option<ActionOutput>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActionInput {
    pub action_type_id: Option<ActionTypeId>,
    pub configuration: HashMap<String, String>,
    /// Set when the action runs in another region than the pipeline.
    pub region: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActionTypeId {
    pub category: String,
    pub owner: String,
    pub provider: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActionOutput {
    pub execution_result: Option<ExecutionResult>,
    pub output_variables: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExecutionResult {
    pub external_execution_id: Option<String>,
    pub external_execution_summary: Option<String>,
    pub external_execution_url: Option<String>,
    pub error_details: Option<ErrorDetails>,
}

pub fn get_pipeline_state(aws: &Aws, name: &str) -> Result<PipelineState> {
    aws.call(&SERVICE, "GetPipelineState", &json!({ "name": name }))
}

pub fn list_executions(aws: &Aws, name: &str, max: u32) -> Result<Vec<ExecutionSummary>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        pipeline_execution_summaries: Vec<ExecutionSummary>,
    }
    let out: Out = aws.call(
        &SERVICE,
        "ListPipelineExecutions",
        &json!({ "pipelineName": name, "maxResults": max }),
    )?;
    Ok(out.pipeline_execution_summaries)
}

pub fn get_execution(aws: &Aws, name: &str, execution_id: &str) -> Result<ExecutionSummary> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        pipeline_execution: ExecutionSummary,
    }
    let out: Out = aws.call(
        &SERVICE,
        "GetPipelineExecution",
        &json!({ "pipelineName": name, "pipelineExecutionId": execution_id }),
    )?;
    Ok(out.pipeline_execution)
}

/// Action executions, newest first. With `execution_id`, only that execution's.
pub fn list_action_executions(
    aws: &Aws,
    name: &str,
    execution_id: Option<&str>,
    max: u32,
) -> Result<Vec<ActionExecutionDetail>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        action_execution_details: Vec<ActionExecutionDetail>,
    }
    let mut input = json!({ "pipelineName": name, "maxResults": max });
    if let Some(id) = execution_id {
        input["filter"] = json!({ "pipelineExecutionId": id });
    }
    let out: Out = aws.call(&SERVICE, "ListActionExecutions", &input)?;
    Ok(out.action_execution_details)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PipelineSummary {
    pub name: String,
    /// Unix seconds.
    pub updated: Option<f64>,
}

/// Every pipeline in the account and region, sorted by name.
pub fn list_pipelines(aws: &Aws) -> Result<Vec<PipelineSummary>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        pipelines: Vec<PipelineSummary>,
        next_token: Option<String>,
    }
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut input = json!({ "maxResults": 100 });
        if let Some(t) = &token {
            input["nextToken"] = json!(t);
        }
        let out: Out = aws.call(&SERVICE, "ListPipelines", &input)?;
        all.extend(out.pipelines);
        match out.next_token {
            Some(t) if !t.is_empty() => token = Some(t),
            _ => break,
        }
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(all)
}

pub fn start(aws: &Aws, name: &str) -> Result<String> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        pipeline_execution_id: String,
    }
    let out: Out = aws.call(&SERVICE, "StartPipelineExecution", &json!({ "name": name }))?;
    Ok(out.pipeline_execution_id)
}

pub fn stop(aws: &Aws, name: &str, execution_id: &str, abandon: bool, reason: &str) -> Result<()> {
    let _: serde_json::Value = aws.call(
        &SERVICE,
        "StopPipelineExecution",
        &json!({
            "pipelineName": name,
            "pipelineExecutionId": execution_id,
            "abandon": abandon,
            "reason": reason,
        }),
    )?;
    Ok(())
}

pub fn put_approval(
    aws: &Aws,
    name: &str,
    stage: &str,
    action: &str,
    token: &str,
    approved: bool,
    summary: &str,
) -> Result<()> {
    let _: serde_json::Value = aws.call(
        &SERVICE,
        "PutApprovalResult",
        &json!({
            "pipelineName": name,
            "stageName": stage,
            "actionName": action,
            "token": token,
            "result": {
                "summary": summary,
                "status": if approved { "Approved" } else { "Rejected" },
            },
        }),
    )?;
    Ok(())
}

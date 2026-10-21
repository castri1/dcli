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
    /// `STANDARD`, or `ROLLBACK` for a stage rollback (V2 pipelines).
    pub execution_type: Option<String>,
    pub rollback_metadata: Option<RollbackMetadata>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RollbackMetadata {
    pub rollback_target_pipeline_execution_id: Option<String>,
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

/// A pipeline's definition (GetPipeline): its type and its stages' actions.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Declaration {
    pub name: String,
    /// `V1` or `V2`; older pipelines leave it out (V1).
    pub pipeline_type: Option<String>,
    pub stages: Vec<StageDeclaration>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StageDeclaration {
    pub name: String,
    pub actions: Vec<ActionDeclaration>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActionDeclaration {
    pub name: String,
    pub action_type_id: ActionTypeId,
    pub configuration: HashMap<String, String>,
}

impl Declaration {
    /// Only V2 pipelines can roll a stage back.
    pub fn is_v2(&self) -> bool {
        self.pipeline_type.as_deref() == Some("V2")
    }

    pub fn source_actions(&self) -> Vec<&ActionDeclaration> {
        self.stages.iter().flat_map(|s| &s.actions).filter(|a| a.action_type_id.category == "Source").collect()
    }

    /// Stages that can be rolled back: every stage but the source ones.
    pub fn rollback_stages(&self) -> Vec<&str> {
        self.stages
            .iter()
            .filter(|s| !s.actions.iter().all(|a| a.action_type_id.category == "Source"))
            .map(|s| s.name.as_str())
            .collect()
    }

    /// The stage a rollback usually means: the last one that deploys.
    pub fn deploy_stage(&self) -> Option<&str> {
        let deploys = |s: &&StageDeclaration| s.actions.iter().any(|a| a.action_type_id.category == "Deploy");
        self.stages.iter().rev().find(deploys).map(|s| s.name.as_str()).or_else(|| self.rollback_stages().last().copied())
    }

    pub fn action(&self, stage: &str, action: &str) -> Option<&ActionDeclaration> {
        self.stages.iter().find(|s| s.name == stage)?.actions.iter().find(|a| a.name == action)
    }
}

pub fn get_pipeline(aws: &Aws, name: &str) -> Result<Declaration> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Out {
        pipeline: Declaration,
    }
    let out: Out = aws.call(&SERVICE, "GetPipeline", &json!({ "name": name }))?;
    Ok(out.pipeline)
}

/// Runs a pipeline on a given revision of one source action instead of its latest.
#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    pub action: String,
    /// `COMMIT_ID`, `S3_OBJECT_VERSION_ID` or `IMAGE_DIGEST`, after the action's provider.
    pub revision_type: &'static str,
    pub revision: String,
}

impl Pin {
    pub fn new(action: &ActionDeclaration, revision: &str) -> Pin {
        let revision_type = match action.action_type_id.provider.as_str() {
            "S3" => "S3_OBJECT_VERSION_ID",
            "ECR" => "IMAGE_DIGEST",
            _ => "COMMIT_ID",
        };
        Pin { action: action.name.clone(), revision_type, revision: revision.to_string() }
    }
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

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Started {
    pipeline_execution_id: String,
}

/// Starts a pipeline, on `pin`'s revision when given (promote, rollback by re-running).
pub fn start_pinned(aws: &Aws, name: &str, pin: Option<&Pin>) -> Result<String> {
    let mut input = json!({ "name": name });
    if let Some(p) = pin {
        input["sourceRevisions"] = json!([{ "actionName": p.action, "revisionType": p.revision_type, "revisionValue": p.revision }]);
    }
    let out: Started = aws.call(&SERVICE, "StartPipelineExecution", &input)?;
    Ok(out.pipeline_execution_id)
}

/// Rolls `stage` back to what `target_execution` deployed there (V2 pipelines): nothing
/// is rebuilt. Returns the rollback execution.
pub fn rollback_stage(aws: &Aws, name: &str, stage: &str, target_execution: &str) -> Result<String> {
    let input = json!({ "pipelineName": name, "stageName": stage, "targetPipelineExecutionId": target_execution });
    let out: Started = aws.call(&SERVICE, "RollbackStage", &input)?;
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

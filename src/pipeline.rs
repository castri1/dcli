//! Pipeline domain model shared by the CLI and the UI: turns raw CodePipeline
//! responses into stage/action views for one execution.

use std::collections::HashMap;


use crate::aws::codepipeline::{self as cp, ActionDeclaration, ActionExecutionDetail, ExecutionSummary, PipelineState};
use crate::aws::{self, Aws};
use crate::util::{ellipsize, first_line};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Succeeded,
    Failed,
    InProgress,
    Stopping,
    Stopped,
    Superseded,
    Cancelled,
    Abandoned,
    /// Waiting on a manual approval.
    Approval,
    /// Not reached yet by a running execution.
    Pending,
    /// Not reached by a finished execution.
    NotRun,
    Unknown,
}

impl Status {
    pub fn parse(s: &str) -> Status {
        match s {
            "Succeeded" => Status::Succeeded,
            "Failed" => Status::Failed,
            "InProgress" => Status::InProgress,
            "Stopping" => Status::Stopping,
            "Stopped" => Status::Stopped,
            "Superseded" => Status::Superseded,
            "Cancelled" => Status::Cancelled,
            "Abandoned" => Status::Abandoned,
            "" => Status::Unknown,
            _ => Status::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Succeeded => "Succeeded",
            Status::Failed => "Failed",
            Status::InProgress => "In progress",
            Status::Stopping => "Stopping",
            Status::Stopped => "Stopped",
            Status::Superseded => "Superseded",
            Status::Cancelled => "Cancelled",
            Status::Abandoned => "Abandoned",
            Status::Approval => "Needs approval",
            Status::Pending => "Pending",
            Status::NotRun => "Not run",
            Status::Unknown => "Unknown",
        }
    }

    /// Plain-text marker for CLI output.
    pub fn symbol(self) -> &'static str {
        match self {
            Status::Succeeded => "✓",
            Status::Failed => "✗",
            Status::InProgress => "⟳",
            Status::Stopping | Status::Stopped => "■",
            Status::Superseded => "↷",
            Status::Cancelled | Status::Abandoned => "⊘",
            Status::Approval => "◆",
            Status::Pending | Status::NotRun | Status::Unknown => "○",
        }
    }

    pub fn is_running(self) -> bool {
        matches!(self, Status::InProgress | Status::Stopping | Status::Approval)
    }
}

#[derive(Debug, Clone)]
pub struct ActionView {
    pub name: String,
    pub status: Status,
    pub provider: Option<String>,
    pub build_id: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub started: Option<f64>,
    pub ended: Option<f64>,
    pub url: Option<String>,
    pub approval_token: Option<String>,
    pub percent: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct StageView {
    pub name: String,
    pub status: Status,
    pub actions: Vec<ActionView>,
    pub started: Option<f64>,
    pub ended: Option<f64>,
}

impl StageView {
    pub fn error(&self) -> Option<&str> {
        self.actions.iter().find_map(|a| a.error.as_deref())
    }

    /// The CodeBuild action whose logs are most interesting: running, then failed, then any.
    pub fn build_action(&self) -> Option<&ActionView> {
        let builds = || self.actions.iter().filter(|a| a.build_id.is_some());
        builds()
            .find(|a| a.status == Status::InProgress)
            .or_else(|| builds().find(|a| a.status == Status::Failed))
            .or_else(|| builds().next())
    }

    pub fn elapsed(&self, now: f64) -> Option<f64> {
        let start = self.started?;
        Some(self.ended.unwrap_or(now) - start)
    }
}

#[derive(Debug, Clone)]
pub struct Approval {
    pub stage: String,
    pub action: String,
    pub token: String,
    pub summary: Option<String>,
    pub url: Option<String>,
}

pub fn approvals(stages: &[StageView]) -> Vec<Approval> {
    stages
        .iter()
        .flat_map(|s| {
            s.actions.iter().filter_map(|a| {
                Some(Approval {
                    stage: s.name.clone(),
                    action: a.name.clone(),
                    token: a.approval_token.clone()?,
                    summary: a.summary.clone(),
                    url: a.url.clone(),
                })
            })
        })
        .collect()
}

/// Stage views for one execution. `details` (ListActionExecutions for that execution)
/// is optional; it adds exact start times, providers and build IDs.
pub fn stages(
    state: &PipelineState,
    execution: Option<&ExecutionSummary>,
    details: &[ActionExecutionDetail],
) -> Vec<StageView> {
    let exec_id = execution.map(|e| e.pipeline_execution_id.as_str());
    let exec_running = execution.is_some_and(|e| Status::parse(&e.status).is_running());

    state
        .stage_states
        .iter()
        .map(|stage| {
            let current = stage
                .latest_execution
                .as_ref()
                .filter(|le| exec_id.is_none_or(|id| le.pipeline_execution_id == id));

            let actions: Vec<ActionView> = stage
                .action_states
                .iter()
                .map(|a| {
                    let detail = details
                        .iter()
                        .filter(|d| {
                            d.stage_name == stage.stage_name
                                && d.action_name == a.action_name
                                && exec_id.is_none_or(|id| d.pipeline_execution_id == id)
                        })
                        .max_by(|x, y| x.last_update_time.partial_cmp(&y.last_update_time).unwrap());
                    let live = a.latest_execution.as_ref().filter(|_| current.is_some());

                    let status = match (detail, live) {
                        (Some(d), _) => Status::parse(&d.status),
                        (None, Some(l)) => Status::parse(&l.status),
                        (None, None) if exec_running => Status::Pending,
                        (None, None) => Status::NotRun,
                    };
                    let provider = detail
                        .and_then(|d| d.input.as_ref())
                        .and_then(|i| i.action_type_id.as_ref())
                        .map(|t| t.provider.clone());
                    let result = detail.and_then(|d| d.output.as_ref()).and_then(|o| o.execution_result.as_ref());
                    let url = live
                        .and_then(|l| l.external_execution_url.clone())
                        .or_else(|| result.and_then(|r| r.external_execution_url.clone()));
                    let is_codebuild = provider.as_deref() == Some("CodeBuild")
                        || url.as_deref().is_some_and(|u| u.contains("codebuild"));
                    let build_id = is_codebuild
                        .then(|| {
                            result
                                .and_then(|r| r.external_execution_id.clone())
                                .or_else(|| live.and_then(|l| l.external_execution_id.clone()))
                        })
                        .flatten();
                    let error = (status == Status::Failed)
                        .then(|| {
                            live.and_then(|l| l.error_details.as_ref())
                                .and_then(|e| e.message.clone())
                                .or_else(|| result.and_then(|r| r.error_details.as_ref()).and_then(|e| e.message.clone()))
                                .or_else(|| result.and_then(|r| r.external_execution_summary.clone()))
                        })
                        .flatten();
                    let approval_token = live
                        .filter(|l| l.status == "InProgress")
                        .and_then(|l| l.token.clone());
                    let started = detail
                        .and_then(|d| d.start_time)
                        .or_else(|| live.filter(|_| status.is_running()).and_then(|l| l.last_status_change));
                    let ended = if status.is_running() || matches!(status, Status::Pending | Status::NotRun) {
                        None
                    } else {
                        detail.and_then(|d| d.last_update_time).or_else(|| live.and_then(|l| l.last_status_change))
                    };

                    ActionView {
                        name: a.action_name.clone(),
                        status: if approval_token.is_some() { Status::Approval } else { status },
                        provider,
                        build_id,
                        summary: live.and_then(|l| l.summary.clone()),
                        error,
                        started,
                        ended,
                        url,
                        approval_token,
                        percent: live.and_then(|l| l.percent_complete),
                    }
                })
                .collect();

            let mut status = match current {
                Some(c) => Status::parse(&c.status),
                None if exec_running => Status::Pending,
                None => Status::NotRun,
            };
            if actions.iter().any(|a| a.status == Status::Approval) {
                status = Status::Approval;
            }
            let started = actions.iter().filter_map(|a| a.started).reduce(f64::min);
            let ended = if status.is_running() {
                None
            } else {
                actions.iter().filter_map(|a| a.ended).reduce(f64::max)
            };
            StageView { name: stage.stage_name.clone(), status, actions, started, ended }
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct Revision {
    pub commit: Option<String>,
    pub message: Option<String>,
    pub branch: Option<String>,
    pub repo: Option<String>,
}

impl Revision {
    pub fn one_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(c) = &self.commit {
            parts.push(c.clone());
        }
        if let Some(m) = &self.message {
            parts.push(format!("\"{}\"", ellipsize(m, 72)));
        }
        if let Some(b) = &self.branch {
            parts.push(format!("({b})"));
        }
        parts.join(" ")
    }
}

fn short_revision(id: &str) -> String {
    if id.len() >= 7 && id.chars().all(|c| c.is_ascii_hexdigit()) {
        id[..7].to_string()
    } else {
        ellipsize(id, 14)
    }
}

/// Commit info from the execution summary, completed with the Source action details.
pub fn revision(summary: &ExecutionSummary, details: &[ActionExecutionDetail]) -> Revision {
    let mut rev = Revision::default();
    if let Some(src) = summary.source_revisions.first() {
        rev.commit = src.revision_id.as_deref().map(short_revision);
        rev.message = src.revision_summary.as_deref().map(|s| {
            // GitHub/CodeStar connections send a JSON blob here.
            let text = serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .and_then(|v| v.get("CommitMessage").and_then(|m| m.as_str()).map(str::to_string))
                .unwrap_or_else(|| s.to_string());
            first_line(&text).to_string()
        });
    }
    let source = details.iter().find(|d| {
        d.pipeline_execution_id == summary.pipeline_execution_id
            && (d.stage_name == "Source"
                || d.input.as_ref().and_then(|i| i.action_type_id.as_ref()).is_some_and(|t| t.category == "Source"))
    });
    if let Some(src) = source {
        let vars = src.output.as_ref().map(|o| &o.output_variables);
        let config = src.input.as_ref().map(|i| &i.configuration);
        let pick = |map: Option<&HashMap<String, String>>, keys: &[&str]| {
            map.and_then(|m| keys.iter().find_map(|k| m.get(*k).cloned()))
        };
        rev.branch = pick(vars, &["BranchName", "branchName"]).or_else(|| pick(config, &["BranchName", "Branch"]));
        rev.repo = pick(config, &["FullRepositoryId", "RepositoryName", "Repo"]);
        if rev.commit.is_none() {
            rev.commit = pick(vars, &["CommitId", "commitId"]).as_deref().map(short_revision);
        }
        if rev.message.is_none() {
            rev.message = pick(vars, &["CommitMessage", "commitMessage"]).map(|m| first_line(&m).to_string());
        }
    }
    rev
}

pub fn trigger_label(summary: &ExecutionSummary) -> &'static str {
    match summary.trigger.as_ref().and_then(|t| t.trigger_type.as_deref()) {
        Some("StartPipelineExecution" | "CreatePipeline" | "PutActionRevision") => "manual",
        Some("CloudWatchEvent") => "event",
        Some("PollForSourceChanges" | "Webhook" | "WebhookV2") => "push",
        Some("AutomatedTrigger" | "ScheduledTrigger") => "schedule",
        Some("ManualRollback" | "AutomatedRollback") => "rollback",
        Some(_) => "other",
        None => "—",
    }
}

/// Index of the newest successful execution in `history` (newest first) older than
/// `history[from]`. The executions before it, from `from`, are what changed since.
pub fn last_success(history: &[ExecutionSummary], from: usize) -> Option<usize> {
    (from + 1..history.len()).find(|&i| Status::parse(&history[i].status) == Status::Succeeded)
}

/// Web page comparing the commits of two executions, for GitHub (also Enterprise), GitLab
/// and Bitbucket sources. `repo` is the source's `FullRepositoryId`, used when the
/// revision has no URL.
pub fn compare_url(older: &ExecutionSummary, newer: &ExecutionSummary, repo: Option<&str>) -> Option<String> {
    let old = older.source_revisions.first()?.revision_id.as_deref()?;
    let src = newer.source_revisions.first()?;
    let new = src.revision_id.as_deref()?;
    if old == new {
        return None;
    }
    let base = match src.revision_url.as_deref() {
        Some(url) if url.contains("/commit/") || url.contains("/commits/") => {
            url.split("/-/commit/").next()?.split("/commits/").next()?.split("/commit/").next()?.to_string()
        }
        _ => {
            let github = src.revision_summary.as_deref().is_some_and(|s| s.contains("\"GitHub"));
            format!("https://github.com/{}", repo.filter(|_| github)?)
        }
    };
    Some(if base.contains("gitlab") {
        format!("{base}/-/compare/{old}...{new}")
    } else if base.contains("bitbucket.org") {
        format!("{base}/branches/compare/{new}%0D{old}")
    } else {
        format!("{base}/compare/{old}...{new}")
    })
}

/// Average duration per stage over recent fully-successful executions, from an
/// unfiltered ListActionExecutions (newest first). Used to estimate progress.
pub fn stage_averages(details: &[ActionExecutionDetail]) -> HashMap<String, f64> {
    struct Span<'a> {
        execution: &'a str,
        stage: &'a str,
        start: f64,
        end: f64,
        succeeded: bool,
    }
    // One span per (execution, stage), in first-seen (newest-first) order.
    let mut spans: Vec<Span> = Vec::new();
    for d in details {
        let (Some(start), Some(end)) = (d.start_time, d.last_update_time) else { continue };
        let ok = d.status == "Succeeded";
        match spans.iter_mut().find(|s| s.execution == d.pipeline_execution_id && s.stage == d.stage_name) {
            Some(s) => {
                s.start = s.start.min(start);
                s.end = s.end.max(end);
                s.succeeded &= ok;
            }
            None => spans.push(Span {
                execution: &d.pipeline_execution_id,
                stage: &d.stage_name,
                start,
                end,
                succeeded: ok,
            }),
        }
    }
    let mut samples: HashMap<String, Vec<f64>> = HashMap::new();
    for s in spans.iter().filter(|s| s.succeeded) {
        let list = samples.entry(s.stage.to_string()).or_default();
        if list.len() < 5 {
            list.push(s.end - s.start);
        }
    }
    samples
        .into_iter()
        .map(|(k, v)| (k, v.iter().sum::<f64>() / v.len() as f64))
        .collect()
}

// ---- promote, rollback, approvals waiting ---------------------------------------------

/// What another module (the matrix) asks Pipelines to do with a pipeline it opens, handed over
/// with `Ctx::jump_intent`: never read from a jump's text, which can come from anywhere (a
/// recommendation Claude wrote, a resource's name).
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Deploy `revision` of `source_action`, which `from_env` runs (`summary` describes it),
    /// to the pipeline; `behind`: how many deploys that moves it forward, when known.
    Promote { pipeline: String, revision: String, source_action: String, from_env: String, summary: String, behind: Option<usize> },
    /// Go back to the last successful deploy before the current one.
    Rollback { pipeline: String },
}

impl Intent {
    pub fn pipeline(&self) -> &str {
        match self {
            Intent::Promote { pipeline, .. } | Intent::Rollback { pipeline } => pipeline,
        }
    }
}

/// Which of a pipeline's source actions to pin: the only one, or the one named `wanted`
/// (the source action the revision comes from). `None` when the user has to choose.
pub fn pick_source(actions: &[&ActionDeclaration], wanted: &str) -> Option<usize> {
    if actions.len() == 1 {
        return Some(0);
    }
    actions.iter().position(|a| a.name == wanted)
}

/// The revision `execution` ran for source action `action` (its first source if no match).
pub fn revision_of<'a>(execution: &'a ExecutionSummary, action: &str) -> Option<&'a str> {
    let revisions = &execution.source_revisions;
    revisions.iter().find(|r| r.action_name == action).or(revisions.first())?.revision_id.as_deref()
}

/// Plain words to add when AWS refuses to start a pipeline on a given revision.
pub fn pin_hint(error: &str) -> &'static str {
    let e = error.to_lowercase();
    if e.contains("validation") || e.contains("revision") || e.contains("not supported") {
        ". CodePipeline runs a given revision only for CodeCommit and CodeStar connection sources \
         (GitHub, GitLab, Bitbucket: a commit), S3 (an object version) and ECR (an image digest)"
    } else {
        ""
    }
}

/// Where a rollback can go: the deployed execution (the latest success) and the earlier
/// successful ones that ran another revision, newest first, one per revision.
#[derive(Debug, Clone)]
pub struct RollbackPlan {
    pub current: ExecutionSummary,
    pub targets: Vec<ExecutionSummary>,
}

/// `history`: newest first. `None` when nothing succeeded before with another revision.
pub fn rollback_plan(history: &[ExecutionSummary]) -> Option<RollbackPlan> {
    let succeeded = |e: &&ExecutionSummary| Status::parse(&e.status) == Status::Succeeded;
    let mut successes = history.iter().filter(succeeded);
    let current = successes.next()?.clone();
    let revision = |e: &ExecutionSummary| e.source_revisions.first().and_then(|r| r.revision_id.clone());
    let mut seen = vec![revision(&current)];
    let mut targets = Vec::new();
    for e in successes {
        let r = revision(e);
        // Without revision ids every earlier success is a different deploy.
        if r.is_some() && seen.contains(&r) {
            continue;
        }
        seen.push(r);
        targets.push(e.clone());
        if targets.len() == 5 {
            break;
        }
    }
    (!targets.is_empty()).then_some(RollbackPlan { current, targets })
}

/// A manual approval waiting in a pipeline, with what it approves.
#[derive(Debug, Clone)]
pub struct Waiting {
    pub pipeline: String,
    pub stage: String,
    pub action: String,
    pub token: String,
    /// When it started waiting (Unix seconds).
    pub since: Option<f64>,
    /// The execution waiting for it.
    pub execution: Option<ExecutionSummary>,
    /// The approval's CustomData and ExternalEntityLink, from the pipeline's definition.
    pub custom_data: Option<String>,
    pub link: Option<String>,
}

/// The manual approvals waiting in `pipeline`. Reading what they approve is best effort.
pub fn waiting_approvals(aws: &Aws, pipeline: &str) -> aws::Result<Vec<Waiting>> {
    let state = cp::get_pipeline_state(aws, pipeline)?;
    let mut declaration = None;
    let mut out = Vec::new();
    for stage in &state.stage_states {
        for a in &stage.action_states {
            let Some(live) = a.latest_execution.as_ref().filter(|l| l.status == "InProgress") else { continue };
            let Some(token) = live.token.clone() else { continue };
            let declaration = declaration.get_or_insert_with(|| cp::get_pipeline(aws, pipeline).ok());
            let config = declaration.as_ref().and_then(|d| d.action(&stage.stage_name, &a.action_name)).map(|d| &d.configuration);
            let field = |key: &str| config.and_then(|c| c.get(key)).filter(|v| !v.trim().is_empty()).cloned();
            let execution = stage.latest_execution.as_ref().and_then(|e| cp::get_execution(aws, pipeline, &e.pipeline_execution_id).ok());
            out.push(Waiting {
                pipeline: pipeline.to_string(),
                stage: stage.stage_name.clone(),
                action: a.action_name.clone(),
                token,
                since: live.last_status_change,
                execution,
                custom_data: field("CustomData"),
                link: field("ExternalEntityLink"),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod changes_tests {
    use super::*;
    use crate::aws::codepipeline::SourceRevision;

    pub(super) fn exec(status: &str, sha: &str, url: Option<&str>) -> ExecutionSummary {
        ExecutionSummary {
            status: status.into(),
            source_revisions: vec![SourceRevision {
                revision_id: Some(sha.into()),
                revision_url: url.map(str::to_string),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn finds_the_last_success_and_compare_links() {
        let history = [
            exec("Failed", "ccc", Some("https://github.com/acme/app/commit/ccc")),
            exec("Superseded", "bbb", None),
            exec("Succeeded", "aaa", None),
        ];
        assert_eq!(last_success(&history, 0), Some(2));
        assert_eq!(last_success(&history, 2), None);
        assert_eq!(
            compare_url(&history[2], &history[0], None).as_deref(),
            Some("https://github.com/acme/app/compare/aaa...ccc")
        );
        let gitlab = exec("Failed", "ccc", Some("https://gitlab.com/g/app/-/commit/ccc"));
        assert_eq!(
            compare_url(&history[2], &gitlab, None).as_deref(),
            Some("https://gitlab.com/g/app/-/compare/aaa...ccc")
        );
        let mut no_url = exec("Failed", "ccc", None);
        no_url.source_revisions[0].revision_summary = Some(r#"{"ProviderType":"GitHub","CommitMessage":"x"}"#.into());
        assert_eq!(
            compare_url(&history[2], &no_url, Some("acme/app")).as_deref(),
            Some("https://github.com/acme/app/compare/aaa...ccc")
        );
        assert_eq!(compare_url(&history[2], &history[2], None), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::codepipeline::*;

    fn state() -> PipelineState {
        serde_json::from_value(serde_json::json!({
            "pipelineName": "app",
            "stageStates": [
                { "stageName": "Source",
                  "latestExecution": { "pipelineExecutionId": "e2", "status": "Succeeded" },
                  "actionStates": [{ "actionName": "Src", "latestExecution": { "status": "Succeeded", "lastStatusChange": 100.0 } }] },
                { "stageName": "Approve",
                  "latestExecution": { "pipelineExecutionId": "e2", "status": "InProgress" },
                  "actionStates": [{ "actionName": "Gate", "latestExecution": { "status": "InProgress", "token": "tok", "lastStatusChange": 120.0 } }] },
                { "stageName": "Deploy",
                  "latestExecution": { "pipelineExecutionId": "e1", "status": "Succeeded" },
                  "actionStates": [{ "actionName": "Push", "latestExecution": { "status": "Succeeded" } }] }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn stages_follow_the_current_execution() {
        let exec: ExecutionSummary =
            serde_json::from_value(serde_json::json!({ "pipelineExecutionId": "e2", "status": "InProgress" })).unwrap();
        let s = stages(&state(), Some(&exec), &[]);
        assert_eq!(s[0].status, Status::Succeeded);
        assert_eq!(s[1].status, Status::Approval);
        assert_eq!(approvals(&s)[0].token, "tok");
        // Deploy's last run belongs to an older execution: pending for this one.
        assert_eq!(s[2].status, Status::Pending);
        assert_eq!(approvals(&s).len(), 1);
    }

    #[test]
    fn revision_reads_codestar_json_summary() {
        let exec: ExecutionSummary = serde_json::from_value(serde_json::json!({
            "pipelineExecutionId": "e2", "status": "Succeeded",
            "sourceRevisions": [{ "actionName": "Src",
                "revisionId": "3f9e1a2b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f",
                "revisionSummary": "{\"ProviderType\":\"GitHub\",\"CommitMessage\":\"fix: retry\\nbody\"}" }]
        }))
        .unwrap();
        let r = revision(&exec, &[]);
        assert_eq!(r.commit.as_deref(), Some("3f9e1a2"));
        assert_eq!(r.message.as_deref(), Some("fix: retry"));
    }

    #[test]
    fn plans_rollbacks_and_intents() {
        use super::changes_tests::exec;
        let history = [
            exec("InProgress", "ddd", None),
            exec("Succeeded", "ccc", None),
            exec("Failed", "bbb", None),
            exec("Succeeded", "ccc", None),
            exec("Succeeded", "aaa", None),
            exec("Succeeded", "999", None),
        ];
        let plan = rollback_plan(&history).unwrap();
        assert_eq!(revision_of(&plan.current, "Source"), Some("ccc"));
        let targets: Vec<_> = plan.targets.iter().map(|t| revision_of(t, "Source").unwrap()).collect();
        assert_eq!(targets, ["aaa", "999"]);
        assert!(rollback_plan(&history[..2]).is_none());
    }

    #[test]
    fn picks_the_source_action_to_pin() {
        let action = |name: &str| ActionDeclaration { name: name.into(), ..Default::default() };
        let (app, infra) = (action("App"), action("Infra"));
        assert_eq!(pick_source(&[&app], "Source"), Some(0));
        assert_eq!(pick_source(&[&app, &infra], "Infra"), Some(1));
        assert_eq!(pick_source(&[&app, &infra], "Source"), None);
    }

    #[test]
    fn averages_only_count_successful_stages() {
        let d = |exec: &str, stage: &str, start: f64, end: f64, status: &str| ActionExecutionDetail {
            pipeline_execution_id: exec.into(),
            stage_name: stage.into(),
            start_time: Some(start),
            last_update_time: Some(end),
            status: status.into(),
            ..Default::default()
        };
        let avg = stage_averages(&[
            d("e3", "Build", 0.0, 500.0, "Failed"),
            d("e2", "Build", 0.0, 100.0, "Succeeded"),
            d("e1", "Build", 0.0, 300.0, "Succeeded"),
        ]);
        assert_eq!(avg["Build"], 200.0);
    }
}

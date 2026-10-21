//! Step Functions (awsJson1_0): state machines, executions, history, redrive/start/stop,
//! and the domain logic shared by the module and `dcli sfn`: the steps of an execution
//! (from its history), where it failed, and a Markdown failure report.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Aws, Result, Service};
use crate::pipeline::Status;
use crate::util::{fmt_ago, fmt_duration, fmt_utc, pretty_json};

pub const SERVICE: Service = Service { endpoint: "states", signing_name: "states", target_prefix: "AWSStepFunctions" };

/// History events read at most per execution.
pub const MAX_EVENTS: usize = 1000;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StateMachine {
    pub state_machine_arn: String,
    pub name: String,
    /// `STANDARD` or `EXPRESS`.
    #[serde(rename = "type")]
    pub kind: String,
    pub creation_date: Option<f64>,
}

impl StateMachine {
    pub fn express(&self) -> bool {
        self.kind == "EXPRESS"
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MachineDetail {
    pub state_machine_arn: String,
    pub name: String,
    pub status: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub role_arn: Option<String>,
    pub description: Option<String>,
    pub creation_date: Option<f64>,
    pub definition: Option<String>,
    pub logging_configuration: Option<LoggingConfiguration>,
}

impl MachineDetail {
    /// Name of the CloudWatch log group the machine logs to, if logging is on.
    pub fn log_group(&self) -> Option<String> {
        let config = self.logging_configuration.as_ref()?;
        if config.level.as_deref() == Some("OFF") {
            return None;
        }
        let arn = config.destinations.iter().find_map(|d| d.cloud_watch_logs_log_group.as_ref()?.log_group_arn.clone())?;
        log_group_from_arn(&arn)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LoggingConfiguration {
    pub level: Option<String>,
    pub include_execution_data: Option<bool>,
    pub destinations: Vec<LogDestination>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogDestination {
    pub cloud_watch_logs_log_group: Option<LogGroupRef>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogGroupRef {
    pub log_group_arn: Option<String>,
}

/// `arn:aws:logs:us-east-1:123:log-group:/aws/vendedlogs/states/x:*` → `/aws/vendedlogs/states/x`.
pub fn log_group_from_arn(arn: &str) -> Option<String> {
    let rest = arn.split_once(":log-group:")?.1;
    Some(rest.strip_suffix(":*").unwrap_or(rest).to_string())
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Execution {
    pub execution_arn: String,
    pub state_machine_arn: String,
    pub name: String,
    pub status: String,
    pub start_date: Option<f64>,
    pub stop_date: Option<f64>,
    pub redrive_count: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExecutionDetail {
    pub execution_arn: String,
    pub state_machine_arn: String,
    pub name: String,
    pub status: String,
    pub start_date: Option<f64>,
    pub stop_date: Option<f64>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub cause: Option<String>,
    pub redrive_count: Option<i64>,
    pub redrive_date: Option<f64>,
    /// `REDRIVABLE`, `NOT_REDRIVABLE`, `REDRIVABLE_BY_MAP_RUN`.
    pub redrive_status: Option<String>,
    pub redrive_status_reason: Option<String>,
}

impl ExecutionDetail {
    /// Seconds it ran (or has been running).
    pub fn duration(&self, now: f64) -> Option<f64> {
        Some(self.stop_date.unwrap_or(now) - self.start_date?)
    }
}

/// One event of an execution's history. Only the details dcli reads are typed.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistoryEvent {
    pub timestamp: Option<f64>,
    #[serde(rename = "type")]
    pub kind: String,
    pub id: i64,
    pub previous_event_id: i64,
    pub execution_started_event_details: Option<IoDetails>,
    pub state_entered_event_details: Option<StateDetails>,
    pub state_exited_event_details: Option<StateDetails>,
    pub task_scheduled_event_details: Option<TaskScheduled>,
    pub lambda_function_scheduled_event_details: Option<LambdaScheduled>,
    pub task_failed_event_details: Option<FailureDetails>,
    pub task_timed_out_event_details: Option<FailureDetails>,
    pub task_start_failed_event_details: Option<FailureDetails>,
    pub task_submit_failed_event_details: Option<FailureDetails>,
    pub lambda_function_failed_event_details: Option<FailureDetails>,
    pub lambda_function_timed_out_event_details: Option<FailureDetails>,
    pub lambda_function_start_failed_event_details: Option<FailureDetails>,
    pub lambda_function_schedule_failed_event_details: Option<FailureDetails>,
    pub activity_failed_event_details: Option<FailureDetails>,
    pub activity_timed_out_event_details: Option<FailureDetails>,
    pub map_run_failed_event_details: Option<FailureDetails>,
    pub execution_failed_event_details: Option<FailureDetails>,
    pub execution_timed_out_event_details: Option<FailureDetails>,
    pub execution_aborted_event_details: Option<FailureDetails>,
}

impl HistoryEvent {
    /// The failure details of any `*Failed` / `*TimedOut` / `*Aborted` event.
    fn failure(&self) -> Option<&FailureDetails> {
        [
            &self.task_failed_event_details,
            &self.task_timed_out_event_details,
            &self.task_start_failed_event_details,
            &self.task_submit_failed_event_details,
            &self.lambda_function_failed_event_details,
            &self.lambda_function_timed_out_event_details,
            &self.lambda_function_start_failed_event_details,
            &self.lambda_function_schedule_failed_event_details,
            &self.activity_failed_event_details,
            &self.activity_timed_out_event_details,
            &self.map_run_failed_event_details,
        ]
        .into_iter()
        .find_map(Option::as_ref)
    }

    fn execution_failure(&self) -> Option<&FailureDetails> {
        [&self.execution_failed_event_details, &self.execution_timed_out_event_details, &self.execution_aborted_event_details]
            .into_iter()
            .find_map(Option::as_ref)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IoDetails {
    pub input: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StateDetails {
    pub name: String,
    pub input: Option<String>,
    pub output: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskScheduled {
    /// `lambda`, `sqs`, `states`…
    pub resource_type: Option<String>,
    /// `invoke`, `sendMessage`… (the integration's action).
    pub resource: Option<String>,
    pub region: Option<String>,
    /// The task's parameters as a JSON string (`FunctionName` for Lambda).
    pub parameters: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LambdaScheduled {
    /// The function ARN.
    pub resource: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FailureDetails {
    pub error: Option<String>,
    pub cause: Option<String>,
}

// ---- calls ---------------------------------------------------------------------------

/// One page of state machines. Returns the next token, if any.
pub fn list_state_machines(aws: &Aws, token: Option<&str>) -> Result<(Vec<StateMachine>, Option<String>)> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        state_machines: Vec<StateMachine>,
        next_token: Option<String>,
    }
    let mut input = json!({ "maxResults": 100 });
    if let Some(t) = token {
        input["nextToken"] = json!(t);
    }
    let out: Out = aws.call_json10(&SERVICE, "ListStateMachines", &input)?;
    Ok((out.state_machines, out.next_token.filter(|t| !t.is_empty())))
}

/// Every state machine (all pages).
pub fn all_state_machines(aws: &Aws) -> Result<Vec<StateMachine>> {
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let (page, next) = list_state_machines(aws, token.as_deref())?;
        all.extend(page);
        match next {
            Some(t) => token = Some(t),
            None => return Ok(all),
        }
    }
}

pub fn describe_state_machine(aws: &Aws, arn: &str) -> Result<MachineDetail> {
    aws.call_json10(&SERVICE, "DescribeStateMachine", &json!({ "stateMachineArn": arn }))
}

/// Executions of a machine, newest first, optionally only with `status` (`FAILED`…),
/// reading at most `max`.
pub fn list_executions(aws: &Aws, machine_arn: &str, status: Option<&str>, max: usize) -> Result<Vec<Execution>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        executions: Vec<Execution>,
        next_token: Option<String>,
    }
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut input = json!({ "stateMachineArn": machine_arn, "maxResults": (max - all.len()).min(100) });
        if let Some(s) = status {
            input["statusFilter"] = json!(s);
        }
        if let Some(t) = &token {
            input["nextToken"] = json!(t);
        }
        let out: Out = aws.call_json10(&SERVICE, "ListExecutions", &input)?;
        all.extend(out.executions);
        match out.next_token.filter(|t| !t.is_empty()) {
            Some(t) if all.len() < max => token = Some(t),
            _ => return Ok(all),
        }
    }
}

/// The definition an execution ran with (for a versioned machine, the version it started on).
pub fn execution_definition(aws: &Aws, execution_arn: &str) -> Result<String> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Out {
        definition: String,
    }
    let out: Out = aws.call_json10(&SERVICE, "DescribeStateMachineForExecution", &json!({ "executionArn": execution_arn }))?;
    Ok(out.definition)
}

pub fn describe_execution(aws: &Aws, arn: &str) -> Result<ExecutionDetail> {
    aws.call_json10(&SERVICE, "DescribeExecution", &json!({ "executionArn": arn }))
}

/// The execution's events, oldest first (at most [`MAX_EVENTS`]).
pub fn execution_history(aws: &Aws, arn: &str) -> Result<Vec<HistoryEvent>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        events: Vec<HistoryEvent>,
        next_token: Option<String>,
    }
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut input = json!({ "executionArn": arn, "maxResults": 1000, "reverseOrder": false, "includeExecutionData": true });
        if let Some(t) = &token {
            input["nextToken"] = json!(t);
        }
        let out: Out = aws.call_json10(&SERVICE, "GetExecutionHistory", &input)?;
        all.extend(out.events);
        match out.next_token.filter(|t| !t.is_empty()) {
            Some(t) if all.len() < MAX_EVENTS => token = Some(t),
            _ => return Ok(all),
        }
    }
}

/// Restarts a failed (Standard) execution from the step that failed.
pub fn redrive_execution(aws: &Aws, arn: &str) -> Result<()> {
    let _: serde_json::Value = aws.call_json10(
        &SERVICE,
        "RedriveExecution",
        &json!({ "executionArn": arn, "clientToken": crate::util::request_token() }),
    )?;
    Ok(())
}

/// Starts an execution; returns its ARN.
pub fn start_execution(aws: &Aws, machine_arn: &str, input: &str, name: Option<&str>) -> Result<String> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        execution_arn: String,
    }
    let mut body = json!({ "stateMachineArn": machine_arn, "input": input });
    if let Some(n) = name.filter(|n| !n.is_empty()) {
        body["name"] = json!(n);
    }
    let out: Out = aws.call_json10(&SERVICE, "StartExecution", &body)?;
    Ok(out.execution_arn)
}

pub fn stop_execution(aws: &Aws, arn: &str, error: &str, cause: &str) -> Result<()> {
    let mut body = json!({ "executionArn": arn });
    if !error.is_empty() {
        body["error"] = json!(error);
    }
    if !cause.is_empty() {
        body["cause"] = json!(cause);
    }
    let _: serde_json::Value = aws.call_json10(&SERVICE, "StopExecution", &body)?;
    Ok(())
}

/// Console page of a state machine or an execution.
pub fn console_url(region: &str, arn: &str) -> String {
    let page = if arn.contains(":execution:") { "v2/executions/details" } else { "statemachines/view" };
    super::console_url(region, &format!("states/home?region={region}#/{page}/{arn}"))
}

/// Execution status (`FAILED`…) as a pipeline [`Status`] (for symbols and colours) and a label.
pub fn status(s: &str) -> (Status, &'static str) {
    match s {
        "RUNNING" => (Status::InProgress, "Running"),
        "SUCCEEDED" => (Status::Succeeded, "Succeeded"),
        "FAILED" => (Status::Failed, "Failed"),
        "TIMED_OUT" => (Status::Failed, "Timed out"),
        "ABORTED" => (Status::Stopped, "Aborted"),
        "PENDING_REDRIVE" => (Status::Stopping, "Pending redrive"),
        _ => (Status::Unknown, "Unknown"),
    }
}

// ---- steps of an execution -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Running,
    Succeeded,
    Failed,
    /// Failed, and a Catch sent the execution on to another state.
    Caught,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub name: String,
    /// `Task`, `Pass`, `Choice`, `Map`, `Parallel`, `Wait`, `Succeed`, `Fail`…
    pub kind: String,
    pub status: StepStatus,
    pub start: Option<f64>,
    pub end: Option<f64>,
    /// `lambda:invoke`, `sqs:sendMessage`… or a function ARN.
    pub resource: Option<String>,
    /// Lambda function name, when the step runs one.
    pub lambda: Option<String>,
    pub retries: usize,
    pub error: Option<String>,
    pub cause: Option<String>,
    /// The state's input and output (JSON), when the history includes execution data.
    pub input: Option<String>,
    pub output: Option<String>,
}

impl Step {
    pub fn duration(&self, now: f64) -> Option<f64> {
        Some(self.end.unwrap_or(now) - self.start?)
    }

    /// Log group of the Lambda function the step runs.
    pub fn log_group(&self) -> Option<String> {
        self.lambda.as_ref().map(|f| format!("/aws/lambda/{f}"))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Failure {
    pub state: Option<String>,
    pub error: Option<String>,
    pub cause: Option<String>,
}

/// `arn:aws:lambda:us-east-1:123:function:orders-sync:live`, `orders-sync:live` or
/// `orders-sync` → `orders-sync`.
pub fn lambda_name(function: &str) -> String {
    let name = function.split_once(":function:").map_or(function, |(_, rest)| rest);
    name.split(':').next().unwrap_or(name).to_string()
}

/// The steps of an execution, in the order they were entered. Events are linked to their
/// state through `previousEventId`, so parallel branches and retries land on the right step.
pub fn steps(events: &[HistoryEvent]) -> Vec<Step> {
    let by_id: HashMap<i64, &HistoryEvent> = events.iter().map(|e| (e.id, e)).collect();
    let mut step_of_entered: HashMap<i64, usize> = HashMap::new();
    let mut steps: Vec<Step> = Vec::new();

    // The step an event belongs to: walk back to its `*StateEntered` event.
    let owner = |e: &HistoryEvent, step_of_entered: &HashMap<i64, usize>| {
        let mut id = e.previous_event_id;
        for _ in 0..64 {
            if let Some(&i) = step_of_entered.get(&id) {
                return Some(i);
            }
            id = by_id.get(&id)?.previous_event_id;
        }
        None
    };

    for e in events {
        if let (Some(kind), Some(d)) = (e.kind.strip_suffix("StateEntered"), &e.state_entered_event_details) {
            step_of_entered.insert(e.id, steps.len());
            steps.push(Step {
                name: d.name.clone(),
                kind: kind.to_string(),
                status: if kind == "Fail" { StepStatus::Failed } else { StepStatus::Running },
                start: e.timestamp,
                end: if kind == "Fail" { e.timestamp } else { None },
                resource: None,
                lambda: None,
                retries: 0,
                error: None,
                cause: None,
                input: d.input.clone(),
                output: None,
            });
            continue;
        }
        let Some(i) = owner(e, &step_of_entered) else { continue };
        let step = &mut steps[i];
        if e.kind.ends_with("StateExited") {
            step.end = e.timestamp;
            step.output = e.state_exited_event_details.as_ref().and_then(|d| d.output.clone());
            step.status = match step.status {
                StepStatus::Failed if step.kind != "Fail" => StepStatus::Caught,
                StepStatus::Running => StepStatus::Succeeded,
                other => other,
            };
        } else if let Some(t) = &e.task_scheduled_event_details {
            if step.status == StepStatus::Failed {
                step.retries += 1;
                step.status = StepStatus::Running;
            }
            let (rtype, action) = (t.resource_type.clone().unwrap_or_default(), t.resource.clone().unwrap_or_default());
            step.resource = Some(format!("{rtype}:{action}"));
            if rtype == "lambda" {
                step.lambda = t
                    .parameters
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                    .and_then(|p| p.get("FunctionName").and_then(|f| f.as_str()).map(lambda_name));
            }
        } else if let Some(l) = &e.lambda_function_scheduled_event_details {
            if step.status == StepStatus::Failed {
                step.retries += 1;
                step.status = StepStatus::Running;
            }
            step.resource = l.resource.clone();
            step.lambda = l.resource.as_deref().map(lambda_name);
        } else if let Some(f) = e.failure() {
            step.status = StepStatus::Failed;
            step.end = e.timestamp;
            step.error = f.error.clone();
            step.cause = f.cause.clone();
        }
    }

    // An execution that failed, timed out or was aborted ends the steps still running.
    if let Some(end) = events.iter().rev().find(|e| e.execution_failure().is_some()) {
        for step in steps.iter_mut().filter(|s| s.status == StepStatus::Running) {
            step.status = StepStatus::Failed;
            step.end = end.timestamp;
        }
    }
    steps
}

/// Where and why an execution failed: the error and cause of the execution, and the last
/// step that failed (not one a Catch recovered from).
pub fn failure(steps: &[Step], events: &[HistoryEvent], detail: Option<&ExecutionDetail>) -> Option<Failure> {
    let failed_step = steps.iter().rev().find(|s| s.status == StepStatus::Failed && s.kind != "Fail");
    let execution = events.iter().rev().find_map(HistoryEvent::execution_failure);
    let error = detail
        .and_then(|d| d.error.clone())
        .or_else(|| execution.and_then(|f| f.error.clone()))
        .or_else(|| failed_step.and_then(|s| s.error.clone()));
    let cause = detail
        .and_then(|d| d.cause.clone())
        .or_else(|| execution.and_then(|f| f.cause.clone()))
        .or_else(|| failed_step.and_then(|s| s.cause.clone()));
    if error.is_none() && cause.is_none() && failed_step.is_none() {
        return None;
    }
    let state = failed_step.or_else(|| steps.iter().rev().find(|s| s.kind == "Fail")).map(|s| s.name.clone());
    Some(Failure { state, error, cause })
}

/// A cause is often a JSON document (a Lambda error with its stack trace): pretty-print it.
pub fn pretty_cause(cause: &str) -> String {
    pretty_json(cause).unwrap_or_else(|| cause.to_string())
}

// ---- report --------------------------------------------------------------------------

/// Input shown in a report, at most this many characters.
const REPORT_INPUT: usize = 4000;

/// A Markdown report of an execution, for an issue or an AI agent: what failed, the error
/// and cause, the input, the steps and where to read the logs.
pub fn report(env_name: &str, region: &str, machine: &MachineDetail, detail: &ExecutionDetail, events: &[HistoryEvent]) -> String {
    let steps = steps(events);
    let failure = failure(&steps, events, Some(detail));
    let (_, label) = status(&detail.status);
    let now = crate::util::now_secs();
    let mut md = String::new();
    let _ = writeln!(md, "## Step Functions execution {}: {} ({env_name})\n", label.to_lowercase(), machine.name);
    let _ = writeln!(md, "- **State machine:** `{}` ({}) · environment `{env_name}` · region `{region}`", machine.name, machine.kind);
    let _ = writeln!(md, "- **Execution:** `{}`", detail.execution_arn);
    let started = detail.start_date.map(|t| format!(" · started {} ({})", fmt_utc(t), fmt_ago(t))).unwrap_or_default();
    let ran = detail.duration(now).map(|d| format!(" · ran {}", fmt_duration(d))).unwrap_or_default();
    let _ = writeln!(md, "- **Status:** {label}{started}{ran}");
    if let Some(n) = detail.redrive_count.filter(|n| *n > 0) {
        let _ = writeln!(md, "- **Redriven:** {n} time{}", if n == 1 { "" } else { "s" });
    }
    if let Some(f) = &failure {
        if let Some(state) = &f.state {
            let _ = writeln!(md, "- **Failed state:** `{state}`");
        }
        if let Some(e) = &f.error {
            let _ = writeln!(md, "- **Error:** `{e}`");
        }
        if let Some(c) = &f.cause {
            let _ = writeln!(md, "\n**Cause:**\n");
            write_block(&mut md, "json", &pretty_cause(c));
        }
    }
    if let Some(input) = &detail.input {
        let text = pretty_json(input).unwrap_or_else(|| input.clone());
        let shown = crate::util::ellipsize(&text, REPORT_INPUT);
        let note = if shown.chars().count() < text.chars().count() { " (truncated)" } else { "" };
        let _ = writeln!(md, "\n**Input**{note}:\n");
        write_block(&mut md, "json", &shown);
    }
    if !steps.is_empty() {
        let _ = writeln!(md, "\n**Steps** (last {} of {}):\n", steps.len().min(25), steps.len());
        let _ = writeln!(md, "| State | Type | Status | Duration | Resource |");
        let _ = writeln!(md, "|---|---|---|---|---|");
        for s in steps.iter().skip(steps.len().saturating_sub(25)) {
            let status = match s.status {
                StepStatus::Running => "running",
                StepStatus::Succeeded => "succeeded",
                StepStatus::Failed => "**failed**",
                StepStatus::Caught => "failed, caught",
            };
            let retries = match s.retries {
                0 => String::new(),
                1 => " (1 retry)".into(),
                n => format!(" ({n} retries)"),
            };
            let resource = s.lambda.as_deref().map(|l| format!("lambda `{l}`")).or_else(|| s.resource.clone()).unwrap_or_default();
            let duration = s.duration(now).map(fmt_duration).unwrap_or_default();
            let _ = writeln!(md, "| `{}` | {} | {status}{retries} | {duration} | {resource} |", s.name, s.kind);
        }
    }
    let mut logs = Vec::new();
    if let Some(group) = steps.iter().rev().find(|s| s.status == StepStatus::Failed).and_then(Step::log_group) {
        logs.push(format!("`dcli tail {env_name} {group} --since 1h`"));
    }
    if let Some(group) = machine.log_group() {
        logs.push(format!("`dcli tail {env_name} {group} --since 1h`"));
    }
    if !logs.is_empty() {
        let _ = writeln!(md, "\n- **Logs:** {}", logs.join(" · "));
    }
    let _ = writeln!(md, "\n_Report by dcli {}._", env!("CARGO_PKG_VERSION"));
    md
}

fn write_block(md: &mut String, lang: &str, text: &str) {
    crate::util::md_block(md, lang, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Standard execution: ValidateOrder (Lambda through the optimized integration)
    /// succeeds, ChargePayment fails once, is retried and fails again, then the
    /// execution fails.
    fn history() -> Vec<HistoryEvent> {
        let raw = r#"[
          {"id":1,"previousEventId":0,"timestamp":100,"type":"ExecutionStarted","executionStartedEventDetails":{"input":"{\"orderId\":\"ORD-1\"}"}},
          {"id":2,"previousEventId":0,"timestamp":100.1,"type":"TaskStateEntered","stateEnteredEventDetails":{"name":"ValidateOrder"}},
          {"id":3,"previousEventId":2,"timestamp":100.2,"type":"TaskScheduled","taskScheduledEventDetails":{"resourceType":"lambda","resource":"invoke","parameters":"{\"FunctionName\":\"arn:aws:lambda:us-east-1:1:function:orders-sync:live\"}"}},
          {"id":4,"previousEventId":3,"timestamp":100.3,"type":"TaskStarted"},
          {"id":5,"previousEventId":4,"timestamp":101.5,"type":"TaskSucceeded"},
          {"id":6,"previousEventId":5,"timestamp":101.6,"type":"TaskStateExited","stateExitedEventDetails":{"name":"ValidateOrder"}},
          {"id":7,"previousEventId":6,"timestamp":101.7,"type":"TaskStateEntered","stateEnteredEventDetails":{"name":"ChargePayment"}},
          {"id":8,"previousEventId":7,"timestamp":101.8,"type":"LambdaFunctionScheduled","lambdaFunctionScheduledEventDetails":{"resource":"arn:aws:lambda:us-east-1:1:function:payments-webhook"}},
          {"id":9,"previousEventId":8,"timestamp":101.9,"type":"LambdaFunctionStarted"},
          {"id":10,"previousEventId":9,"timestamp":104.9,"type":"LambdaFunctionFailed","lambdaFunctionFailedEventDetails":{"error":"TimeoutError","cause":"first"}},
          {"id":11,"previousEventId":10,"timestamp":106,"type":"LambdaFunctionScheduled","lambdaFunctionScheduledEventDetails":{"resource":"arn:aws:lambda:us-east-1:1:function:payments-webhook"}},
          {"id":12,"previousEventId":11,"timestamp":106.1,"type":"LambdaFunctionStarted"},
          {"id":13,"previousEventId":12,"timestamp":109.1,"type":"LambdaFunctionFailed","lambdaFunctionFailedEventDetails":{"error":"TimeoutError","cause":"{\"errorMessage\":\"no answer\"}"}},
          {"id":14,"previousEventId":13,"timestamp":109.2,"type":"ExecutionFailed","executionFailedEventDetails":{"error":"TimeoutError","cause":"{\"errorMessage\":\"no answer\"}"}}
        ]"#;
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn builds_steps_and_finds_the_failure() {
        let events = history();
        let steps = steps(&events);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].name, "ValidateOrder");
        assert_eq!(steps[0].status, StepStatus::Succeeded);
        assert_eq!(steps[0].lambda.as_deref(), Some("orders-sync"));
        assert_eq!(steps[0].resource.as_deref(), Some("lambda:invoke"));
        assert_eq!(steps[1].status, StepStatus::Failed);
        assert_eq!(steps[1].retries, 1);
        assert_eq!(steps[1].lambda.as_deref(), Some("payments-webhook"));
        assert_eq!(steps[1].log_group().as_deref(), Some("/aws/lambda/payments-webhook"));
        assert_eq!(steps[1].end, Some(109.1));

        let f = failure(&steps, &events, None).unwrap();
        assert_eq!(f.state.as_deref(), Some("ChargePayment"));
        assert_eq!(f.error.as_deref(), Some("TimeoutError"));
        assert!(pretty_cause(f.cause.as_deref().unwrap()).contains("\"errorMessage\": \"no answer\""));
    }

    #[test]
    fn caught_failures_and_aborts() {
        let raw = r#"[
          {"id":1,"previousEventId":0,"timestamp":1,"type":"TaskStateEntered","stateEnteredEventDetails":{"name":"Try"}},
          {"id":2,"previousEventId":1,"timestamp":2,"type":"TaskFailed","taskFailedEventDetails":{"error":"Boom"}},
          {"id":3,"previousEventId":2,"timestamp":3,"type":"TaskStateExited","stateExitedEventDetails":{"name":"Try"}},
          {"id":4,"previousEventId":3,"timestamp":4,"type":"WaitStateEntered","stateEnteredEventDetails":{"name":"Pause"}},
          {"id":5,"previousEventId":4,"timestamp":9,"type":"ExecutionAborted","executionAbortedEventDetails":{"error":"Stopped","cause":"by hand"}}
        ]"#;
        let events: Vec<HistoryEvent> = serde_json::from_str(raw).unwrap();
        let steps = steps(&events);
        assert_eq!(steps[0].status, StepStatus::Caught);
        assert_eq!(steps[1].status, StepStatus::Failed);
        assert_eq!(steps[1].end, Some(9.0));
        let f = failure(&steps, &events, None).unwrap();
        assert_eq!(f.state.as_deref(), Some("Pause"));
        assert_eq!(f.error.as_deref(), Some("Stopped"));
    }

    #[test]
    fn names_and_log_groups() {
        assert_eq!(lambda_name("orders-sync"), "orders-sync");
        assert_eq!(lambda_name("orders-sync:live"), "orders-sync");
        assert_eq!(lambda_name("arn:aws:lambda:us-east-1:1:function:x:3"), "x");
        assert_eq!(
            log_group_from_arn("arn:aws:logs:us-east-1:1:log-group:/aws/vendedlogs/states/ingest-Logs:*").as_deref(),
            Some("/aws/vendedlogs/states/ingest-Logs")
        );
        assert_eq!(status("TIMED_OUT").1, "Timed out");
    }

    #[test]
    fn report_mentions_the_failure() {
        let events = history();
        let machine = MachineDetail { name: "orders".into(), kind: "STANDARD".into(), ..Default::default() };
        let detail = ExecutionDetail {
            execution_arn: "arn:x".into(),
            status: "FAILED".into(),
            start_date: Some(100.0),
            stop_date: Some(109.2),
            input: Some("{\"orderId\":\"ORD-1\"}".into()),
            error: Some("TimeoutError".into()),
            cause: Some("{\"errorMessage\":\"no answer\"}".into()),
            ..Default::default()
        };
        let md = report("dev", "us-east-1", &machine, &detail, &events);
        assert!(md.contains("**Failed state:** `ChargePayment`"), "{md}");
        assert!(md.contains("\"orderId\": \"ORD-1\""), "{md}");
        assert!(md.contains("dcli tail dev /aws/lambda/payments-webhook"), "{md}");
    }
}

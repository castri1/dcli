import {
  CodePipelineClient,
  StartPipelineExecutionCommand,
  GetPipelineExecutionCommand,
  ListActionExecutionsCommand,
  GetPipelineStateCommand,
  ListPipelinesCommand,
  ListPipelineExecutionsCommand,
  PutApprovalResultCommand,
  StopPipelineExecutionCommand,
  type ActionExecutionDetail,
  type StageState,
} from "@aws-sdk/client-codepipeline";
import { createCredentialProvider } from "./credentials.js";
import type { SourceRevision } from "./git-info.js";

export interface StageStatus {
  stageName: string;
  status: "InProgress" | "Succeeded" | "Failed" | "Stopped" | "Pending";
  actionName?: string;
  startTime?: Date;
  lastUpdateTime?: Date;
  errorMessage?: string;
  waitingForApproval?: boolean;
  approvalToken?: string;
}

export interface PendingApproval {
  pipelineName: string;
  stageName: string;
  actionName: string;
  token: string;
  customData?: string;
  approvalReviewLink?: string;
}

export interface PipelineExecutionStatus {
  executionId: string;
  pipelineName: string;
  status: string;
  startTime?: Date;
  lastUpdateTime?: Date;
  stages: StageStatus[];
  pendingApprovals: PendingApproval[];
  sourceRevision?: SourceRevision;
}

function createClient(profile: string, region: string): CodePipelineClient {
  return new CodePipelineClient({
    region,
    credentials: createCredentialProvider(profile),
  });
}

/**
 * Start a pipeline execution and return the execution ID.
 */
export async function startPipeline(
  profile: string,
  region: string,
  pipelineName: string
): Promise<string> {
  const client = createClient(profile, region);
  const command = new StartPipelineExecutionCommand({ name: pipelineName });
  const response = await client.send(command);

  if (!response.pipelineExecutionId) {
    throw new Error("Failed to start pipeline: no execution ID returned");
  }

  return response.pipelineExecutionId;
}

/**
 * Get the overall execution status for a pipeline execution.
 */
export async function getExecutionStatus(
  profile: string,
  region: string,
  pipelineName: string,
  executionId: string
): Promise<PipelineExecutionStatus> {
  const client = createClient(profile, region);

  // Get overall execution status
  const execCommand = new GetPipelineExecutionCommand({
    pipelineName,
    pipelineExecutionId: executionId,
  });
  const execResponse = await client.send(execCommand);
  const execution = execResponse.pipelineExecution;

  if (!execution) {
    throw new Error(`Execution ${executionId} not found`);
  }

  // Get pipeline state for stage-level status
  const stateCommand = new GetPipelineStateCommand({ name: pipelineName });
  const stateResponse = await client.send(stateCommand);

  // Get action executions for more detailed info
  const actionsCommand = new ListActionExecutionsCommand({
    pipelineName,
    filter: { pipelineExecutionId: executionId },
  });
  const actionsResponse = await client.send(actionsCommand);

  const stageStates = stateResponse.stageStates || [];

  const stages = buildStageStatuses(
    stageStates,
    actionsResponse.actionExecutionDetails || [],
    executionId
  );

  // Extract pending approvals from pipeline state
  const pendingApprovals = extractPendingApprovals(
    pipelineName,
    stageStates,
    executionId
  );

  // Mark stages that are waiting for approval
  for (const approval of pendingApprovals) {
    const stage = stages.find((s) => s.stageName === approval.stageName);
    if (stage) {
      stage.waitingForApproval = true;
      stage.approvalToken = approval.token;
    }
  }

  // Extract source revision from source action
  const actionDetails = actionsResponse.actionExecutionDetails || [];
  const sourceRevision = extractSourceRevision(actionDetails);

  return {
    executionId,
    pipelineName,
    status: execution.status || "Unknown",
    startTime: execution.lastUpdateTime,
    lastUpdateTime: execution.lastUpdateTime,
    stages,
    pendingApprovals,
    sourceRevision,
  };
}

/**
 * List all pipelines in an AWS account. Handles pagination automatically.
 * Returns an array of pipeline names sorted alphabetically.
 */
export async function listPipelines(
  profile: string,
  region: string
): Promise<string[]> {
  const client = createClient(profile, region);
  const pipelineNames: string[] = [];
  let nextToken: string | undefined;

  do {
    const command = new ListPipelinesCommand({
      nextToken,
      maxResults: 100,
    });
    const response = await client.send(command);

    if (response.pipelines) {
      for (const pipeline of response.pipelines) {
        if (pipeline.name) {
          pipelineNames.push(pipeline.name);
        }
      }
    }

    nextToken = response.nextToken;
  } while (nextToken);

  return pipelineNames.sort((a, b) => a.localeCompare(b));
}

export type TriggerType = "self" | "webhook" | "manual" | "event" | "schedule" | "unknown";

export interface LatestExecution {
  executionId: string;
  status: string;
  startTime: Date;
  triggerType: TriggerType;
}

/**
 * Get the most recent execution for a pipeline.
 * Returns null if no executions found.
 */
export async function getLatestExecution(
  profile: string,
  region: string,
  pipelineName: string
): Promise<LatestExecution | null> {
  const client = createClient(profile, region);
  const command = new ListPipelineExecutionsCommand({
    pipelineName,
    maxResults: 1,
  });
  const response = await client.send(command);

  const executions = response.pipelineExecutionSummaries;
  if (!executions || executions.length === 0) {
    return null;
  }

  const latest = executions[0];
  if (!latest.pipelineExecutionId || !latest.status) {
    return null;
  }

  return {
    executionId: latest.pipelineExecutionId,
    status: latest.status,
    startTime: latest.startTime || new Date(),
    triggerType: mapTriggerType(latest.trigger?.triggerType),
  };
}

export interface ExecutionHistoryEntry {
  executionId: string;
  status: string;
  triggerType: TriggerType;
  startTime?: Date;
  lastUpdateTime?: Date;
  sourceRevisionSummary?: string;
}

/**
 * Get execution history for a pipeline (last N executions).
 */
export async function getExecutionHistory(
  profile: string,
  region: string,
  pipelineName: string,
  maxResults: number = 10
): Promise<ExecutionHistoryEntry[]> {
  const client = createClient(profile, region);
  const command = new ListPipelineExecutionsCommand({
    pipelineName,
    maxResults,
  });
  const response = await client.send(command);

  const executions = response.pipelineExecutionSummaries || [];

  return executions
    .filter((e) => e.pipelineExecutionId && e.status)
    .map((e) => ({
      executionId: e.pipelineExecutionId!,
      status: e.status!,
      triggerType: mapTriggerType(e.trigger?.triggerType),
      startTime: e.startTime,
      lastUpdateTime: e.lastUpdateTime,
      sourceRevisionSummary:
        e.sourceRevisions?.[0]?.revisionSummary || undefined,
    }));
}

/**
 * Map AWS trigger type strings to our simplified trigger types.
 */
function mapTriggerType(awsTriggerType?: string): TriggerType {
  if (!awsTriggerType) return "unknown";

  switch (awsTriggerType) {
    case "StartPipelineExecution":
    case "CreatePipeline":
    case "PutActionRevision":
      return "manual";
    case "CloudWatchEvent":
      return "event";
    case "PollForSourceChanges":
    case "Webhook":
    case "WebhookV2":
      return "webhook";
    case "AutomatedTrigger":
    case "ScheduledTrigger":
      return "schedule";
    default:
      return "unknown";
  }
}

/**
 * Build stage statuses by combining pipeline state and action execution details.
 */
function buildStageStatuses(
  stageStates: StageState[],
  actionDetails: ActionExecutionDetail[],
  executionId: string
): StageStatus[] {
  return stageStates.map((stage) => {
    const stageName = stage.stageName || "Unknown";

    // Find action executions belonging to this stage and execution
    const stageActions = actionDetails.filter(
      (a) =>
        a.stageName === stageName &&
        a.pipelineExecutionId === executionId
    );

    // Determine stage status from its latest action execution
    const latestAction = stageActions.sort((a, b) => {
      const aTime = a.lastUpdateTime?.getTime() || 0;
      const bTime = b.lastUpdateTime?.getTime() || 0;
      return bTime - aTime;
    })[0];

    let status: StageStatus["status"] = "Pending";
    let errorMessage: string | undefined;

    if (latestAction) {
      switch (latestAction.status) {
        case "InProgress":
          status = "InProgress";
          break;
        case "Succeeded":
          status = "Succeeded";
          break;
        case "Failed":
          status = "Failed";
          errorMessage =
            latestAction.output?.executionResult?.externalExecutionSummary ||
            undefined;
          break;
        case "Abandoned":
          status = "Stopped";
          break;
        default:
          status = "Pending";
      }
    } else {
      // No action details yet — check the stage's latestExecution for a basic status
      const latestExec = stage.latestExecution;
      if (latestExec?.pipelineExecutionId === executionId) {
        switch (latestExec.status) {
          case "InProgress":
            status = "InProgress";
            break;
          case "Succeeded":
            status = "Succeeded";
            break;
          case "Failed":
            status = "Failed";
            break;
          case "Stopped":
            status = "Stopped";
            break;
          default:
            status = "Pending";
        }
      }
    }

    return {
      stageName,
      status,
      actionName: latestAction?.actionName,
      startTime: latestAction?.startTime,
      lastUpdateTime: latestAction?.lastUpdateTime,
      errorMessage,
    };
  });
}

/**
 * Extract pending manual approval actions from pipeline state.
 */
function extractPendingApprovals(
  pipelineName: string,
  stageStates: StageState[],
  executionId: string
): PendingApproval[] {
  const approvals: PendingApproval[] = [];

  for (const stage of stageStates) {
    if (!stage.actionStates) continue;

    for (const action of stage.actionStates) {
      // Check if this action is a manual approval waiting for input
      if (
        action.latestExecution?.status === "InProgress" &&
        action.latestExecution?.token
      ) {
        approvals.push({
          pipelineName,
          stageName: stage.stageName || "Unknown",
          actionName: action.actionName || "Unknown",
          token: action.latestExecution.token,
          customData: action.latestExecution.summary || undefined,
          approvalReviewLink: action.revisionUrl || undefined,
        });
      }
    }
  }

  return approvals;
}

/**
 * Submit an approval or rejection for a manual approval action.
 */
export async function submitApprovalResult(
  profile: string,
  region: string,
  pipelineName: string,
  stageName: string,
  actionName: string,
  token: string,
  approved: boolean,
  comment?: string
): Promise<void> {
  const client = createClient(profile, region);
  const command = new PutApprovalResultCommand({
    pipelineName,
    stageName,
    actionName,
    token,
    result: {
      summary: comment || (approved ? "Approved via deploy-cli" : "Rejected via deploy-cli"),
      status: approved ? "Approved" : "Rejected",
    },
  });
  await client.send(command);
}

/**
 * Extract source revision info from action execution details.
 */
function extractSourceRevision(
  actionDetails: ActionExecutionDetail[]
): SourceRevision | undefined {
  const sourceAction = actionDetails.find(
    (a) =>
      a.stageName === "Source" ||
      a.input?.actionTypeId?.category === "Source"
  );

  if (!sourceAction) return undefined;

  const outputVars = sourceAction.output?.outputVariables || {};
  const config = sourceAction.input?.configuration as
    | Record<string, string>
    | undefined;

  const commitId =
    outputVars["CommitId"] ||
    outputVars["commitId"] ||
    undefined;

  const commitMessage =
    outputVars["CommitMessage"] ||
    outputVars["commitMessage"] ||
    sourceAction.output?.executionResult?.externalExecutionSummary ||
    undefined;

  const branch =
    outputVars["BranchName"] ||
    outputVars["branchName"] ||
    config?.["BranchName"] ||
    config?.["Branch"] ||
    undefined;

  const repoName =
    config?.["RepositoryName"] ||
    config?.["Repo"] ||
    config?.["FullRepositoryId"] ||
    undefined;

  if (!commitId && !commitMessage && !branch) {
    return undefined;
  }

  return {
    commitId: commitId ? commitId.slice(0, 7) : undefined,
    commitMessage: commitMessage
      ? commitMessage.split("\n")[0].slice(0, 80)
      : undefined,
    branch,
    repoName,
  };
}

/**
 * Stop a running pipeline execution.
 * @param abandon - If true, stops without waiting for in-progress actions to finish.
 *                  If false, waits for in-progress actions to complete before stopping.
 */
export async function stopPipelineExecution(
  profile: string,
  region: string,
  pipelineName: string,
  executionId: string,
  reason: string,
  abandon: boolean = false
): Promise<void> {
  const client = createClient(profile, region);
  const command = new StopPipelineExecutionCommand({
    pipelineName,
    pipelineExecutionId: executionId,
    reason,
    abandon,
  });
  await client.send(command);
}

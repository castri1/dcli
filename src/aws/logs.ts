import {
  CodeBuildClient,
  BatchGetBuildsCommand,
} from "@aws-sdk/client-codebuild";
import {
  CloudWatchLogsClient,
  GetLogEventsCommand,
} from "@aws-sdk/client-cloudwatch-logs";
import {
  CodePipelineClient,
  ListActionExecutionsCommand,
} from "@aws-sdk/client-codepipeline";
import { createCredentialProvider } from "./credentials.js";

export interface BuildLogInfo {
  stageName: string;
  actionName: string;
  buildId: string;
  logGroupName: string;
  logStreamName: string;
  buildStatus?: string;
}

export interface LogEvent {
  timestamp: number;
  message: string;
}

/**
 * Find CodeBuild actions in a pipeline execution and extract their build IDs.
 */
export async function findBuildActions(
  profile: string,
  region: string,
  pipelineName: string,
  executionId: string
): Promise<
  Array<{
    stageName: string;
    actionName: string;
    externalExecutionId?: string;
    status?: string;
  }>
> {
  const client = new CodePipelineClient({
    region,
    credentials: createCredentialProvider(profile),
  });

  const command = new ListActionExecutionsCommand({
    pipelineName,
    filter: { pipelineExecutionId: executionId },
  });
  const response = await client.send(command);

  const actions = response.actionExecutionDetails || [];

  // Filter for CodeBuild actions (they have externalExecutionId like "build-id")
  return actions
    .filter(
      (a) =>
        a.output?.executionResult?.externalExecutionId &&
        (a.input?.actionTypeId?.provider === "CodeBuild" ||
          a.output?.executionResult?.externalExecutionUrl?.includes(
            "codebuild"
          ))
    )
    .map((a) => ({
      stageName: a.stageName || "Unknown",
      actionName: a.actionName || "Unknown",
      externalExecutionId:
        a.output?.executionResult?.externalExecutionId || undefined,
      status: a.status || undefined,
    }));
}

/**
 * Get the CloudWatch log group and stream for a CodeBuild build.
 */
export async function getBuildLogInfo(
  profile: string,
  region: string,
  buildId: string
): Promise<BuildLogInfo | null> {
  const client = new CodeBuildClient({
    region,
    credentials: createCredentialProvider(profile),
  });

  const command = new BatchGetBuildsCommand({ ids: [buildId] });
  const response = await client.send(command);

  const build = response.builds?.[0];
  if (!build) return null;

  const logs = build.logs;
  if (!logs?.groupName || !logs?.streamName) return null;

  return {
    stageName: "",
    actionName: "",
    buildId,
    logGroupName: logs.groupName,
    logStreamName: logs.streamName,
    buildStatus: build.buildStatus,
  };
}

/**
 * Fetch log events from CloudWatch Logs.
 * Returns events and a forward token for pagination.
 */
export async function getLogEvents(
  profile: string,
  region: string,
  logGroupName: string,
  logStreamName: string,
  nextToken?: string
): Promise<{ events: LogEvent[]; nextForwardToken?: string }> {
  const client = new CloudWatchLogsClient({
    region,
    credentials: createCredentialProvider(profile),
  });

  const command = new GetLogEventsCommand({
    logGroupName,
    logStreamName,
    startFromHead: true,
    nextToken,
    limit: 100,
  });

  const response = await client.send(command);

  const events: LogEvent[] = (response.events || [])
    .filter((e) => e.message)
    .map((e) => ({
      timestamp: e.timestamp || Date.now(),
      message: e.message!.trimEnd(),
    }));

  return {
    events,
    nextForwardToken: response.nextForwardToken,
  };
}

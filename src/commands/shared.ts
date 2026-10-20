import type { AppConfig, PipelineConfig } from "../config/types.js";
import { getEnvironment, getPipeline } from "../config/loader.js";
import { validateCredentials } from "../aws/credentials.js";
import { ssoLogin } from "../aws/auth.js";
import { getLatestExecution, type TriggerType } from "../aws/pipeline.js";
import {
  getAllTrackedExecutions,
  upsertExternalExecution,
} from "../state/manager.js";
import type { TrackedExecution } from "../state/types.js";
import { selectEnvironment, selectPipeline } from "../ui/prompts.js";
import { CredentialsExpiredError } from "../errors.js";
import * as log from "../ui/logger.js";

export interface PipelineContext {
  envName: string;
  pipeline: PipelineConfig;
  profile: string;
  region: string;
}

export interface ResolvedExecutionReference extends TrackedExecution {
  source: "tracked" | "aws";
}

export async function resolvePipelineContext(
  config: AppConfig,
  envName?: string,
  pipelineName?: string
): Promise<PipelineContext> {
  if (envName) {
    const env = getEnvironment(config, envName);
    const pipeline = getPipeline(env, envName, pipelineName);
    return {
      envName,
      pipeline,
      profile: env.profile,
      region: env.region,
    };
  }

  const selected = await selectEnvironment(config);
  const pipeline = await selectPipeline(selected.env, selected.envName);
  return {
    envName: selected.envName,
    pipeline,
    profile: selected.env.profile,
    region: selected.env.region,
  };
}

export async function ensureCredentials(
  envName: string,
  profile: string
): Promise<void> {
  const credentialsValid = await log.withSpinner(
    `Checking credentials for ${envName} (profile: ${profile})`,
    () => validateCredentials(profile),
    "Credentials valid"
  );

  if (credentialsValid) {
    return;
  }

  log.warn(`SSO session expired or not found for profile '${profile}'.`);
  log.info(`Opening AWS SSO login for ${envName}...`);
  ssoLogin(profile);

  const refreshed = await log.withSpinner(
    `Re-checking credentials for ${envName}`,
    () => validateCredentials(profile),
    "Credentials refreshed"
  );

  if (!refreshed) {
    throw new CredentialsExpiredError(envName, profile);
  }
}

export async function resolveExecutionReference(
  context: PipelineContext,
  options?: { requireActive?: boolean }
): Promise<ResolvedExecutionReference | undefined> {
  const requireActive = options?.requireActive || false;
  const tracked = await getAllTrackedExecutions();
  const trackedMatches = tracked
    .filter(
      (entry) =>
        entry.env === context.envName && entry.pipeline === context.pipeline.name
    )
    .sort(
      (a, b) =>
        new Date(b.startedAt).getTime() - new Date(a.startedAt).getTime()
    );

  let latestExecution:
    | {
        executionId: string;
        status: TrackedExecution["status"];
        startedAt: string;
        triggeredBy: TriggerType;
      }
    | undefined;

  try {
    const latest = await getLatestExecution(
      context.profile,
      context.region,
      context.pipeline.name
    );
    if (latest) {
      latestExecution = {
        executionId: latest.executionId,
        status: mapExecutionStatus(latest.status),
        startedAt: latest.startTime.toISOString(),
        triggeredBy: latest.triggerType,
      };

      if (latestExecution.status === "InProgress") {
        await upsertExternalExecution({
          env: context.envName,
          pipeline: context.pipeline.name,
          profile: context.profile,
          region: context.region,
          executionId: latestExecution.executionId,
          startedAt: latestExecution.startedAt,
          status: latestExecution.status,
          triggeredBy: latestExecution.triggeredBy,
        });
      }
    }
  } catch {
    // Fall back to tracked state if AWS lookup fails.
  }

  if (requireActive) {
    if (latestExecution?.status === "InProgress") {
      return {
        env: context.envName,
        pipeline: context.pipeline.name,
        profile: context.profile,
        region: context.region,
        executionId: latestExecution.executionId,
        startedAt: latestExecution.startedAt,
        status: latestExecution.status,
        triggeredBy: latestExecution.triggeredBy,
        source: trackedMatches.some(
          (entry) => entry.executionId === latestExecution?.executionId
        )
          ? "tracked"
          : "aws",
      };
    }

    const activeTracked = trackedMatches.find(
      (entry) => entry.status === "InProgress" || entry.status === "Unknown"
    );
    return activeTracked
      ? { ...activeTracked, source: "tracked" }
      : undefined;
  }

  const newestTracked = trackedMatches[0];

  if (
    latestExecution &&
    (!newestTracked ||
      new Date(latestExecution.startedAt).getTime() >=
        new Date(newestTracked.startedAt).getTime())
  ) {
    return {
      env: context.envName,
      pipeline: context.pipeline.name,
      profile: context.profile,
      region: context.region,
      executionId: latestExecution.executionId,
      startedAt: latestExecution.startedAt,
      status: latestExecution.status,
      triggeredBy: latestExecution.triggeredBy,
      source: trackedMatches.some(
        (entry) => entry.executionId === latestExecution?.executionId
      )
        ? "tracked"
        : "aws",
    };
  }

  return newestTracked ? { ...newestTracked, source: "tracked" } : undefined;
}

export function mapExecutionStatus(
  apiStatus: string
): TrackedExecution["status"] {
  switch (apiStatus) {
    case "InProgress":
      return "InProgress";
    case "Succeeded":
      return "Succeeded";
    case "Failed":
      return "Failed";
    case "Stopped":
    case "Stopping":
    case "Superseded":
    case "Cancelled":
      return "Stopped";
    default:
      return "Unknown";
  }
}

export function printJson(value: unknown): void {
  console.log(JSON.stringify(value, null, 2));
}

export function parsePositiveInt(
  value: string | undefined,
  fallback: number
): number {
  if (!value) {
    return fallback;
  }

  const parsed = Number.parseInt(value, 10);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    throw new Error(`Expected a positive integer, received '${value}'`);
  }

  return parsed;
}

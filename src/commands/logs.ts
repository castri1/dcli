import { select } from "@inquirer/prompts";
import chalk from "chalk";
import gradient from "gradient-string";
import { loadConfig } from "../config/loader.js";
import { findBuildActions, getBuildLogInfo, getLogEvents } from "../aws/logs.js";
import {
  ensureCredentials,
  resolveExecutionReference,
  resolvePipelineContext,
} from "./shared.js";
import * as log from "../ui/logger.js";

const HEADER_GRADIENT = gradient(["#00d2ff", "#3a7bd5"]);

export async function logsCommand(
  envName?: string,
  pipelineName?: string
): Promise<void> {
  const config = loadConfig();
  const context = await resolvePipelineContext(config, envName, pipelineName);
  const resolvedEnvName = context.envName;
  const resolvedPipelineName = context.pipeline.name;

  await ensureCredentials(resolvedEnvName, context.profile);

  const trackedExec = await resolveExecutionReference(context);

  if (!trackedExec) {
    log.info(
      `No recent execution found for '${resolvedPipelineName}' in ${resolvedEnvName}.`
    );
    return;
  }

  // Find CodeBuild actions
  const buildActions = await log.withSpinner(
    "Finding build actions",
    () =>
      findBuildActions(
        context.profile,
        context.region,
        resolvedPipelineName,
        trackedExec.executionId
      ),
    "Build actions found"
  );

  if (buildActions.length === 0) {
    log.info("No CodeBuild actions found in this pipeline execution.");
    log.dim(
      "Logs streaming is only available for pipelines with CodeBuild actions."
    );
    return;
  }

  // Select build action if multiple
  let selectedAction: (typeof buildActions)[0];
  if (buildActions.length === 1) {
    selectedAction = buildActions[0];
  } else {
    const actionValue = await select({
      message: "Select a build action:",
      choices: buildActions.map((a) => ({
        name: `${chalk.cyan(a.stageName)} / ${chalk.white(a.actionName)} ${a.status ? chalk.gray(`(${a.status})`) : ""}`,
        value: `${a.stageName}:${a.actionName}`,
      })),
    });
    const [stage, action] = actionValue.split(":");
    selectedAction = buildActions.find(
      (a) => a.stageName === stage && a.actionName === action
    )!;
  }

  if (!selectedAction.externalExecutionId) {
    log.info("Build has not started yet. No logs available.");
    return;
  }

  // Get build log info
  const buildLogInfo = await log.withSpinner(
    "Fetching build log location",
    () =>
      getBuildLogInfo(
        context.profile,
        context.region,
        selectedAction.externalExecutionId!
      ),
    "Log location found"
  );

  if (!buildLogInfo) {
    log.info("Could not find log information for this build.");
    log.dim(
      "The build may not have started yet, or logs may be stored in a non-default location."
    );
    return;
  }

  // Stream logs
  console.log("");
  console.log(
    "  " +
      HEADER_GRADIENT(
        `Logs — ${resolvedPipelineName} / ${selectedAction.stageName} / ${selectedAction.actionName}`
      )
  );
  console.log(chalk.gray("  " + "─".repeat(60)));
  console.log("");

  let nextToken: string | undefined;
  let previousToken: string | undefined;
  const isActive =
    trackedExec.status === "InProgress" ||
    trackedExec.status === "Unknown";

  // Initial fetch — get all existing logs
  let hasLogs = false;
  while (true) {
      const result = await getLogEvents(
        context.profile,
        context.region,
        buildLogInfo.logGroupName,
        buildLogInfo.logStreamName,
        nextToken
    );

    for (const event of result.events) {
      hasLogs = true;
      printLogLine(event.message);
    }

    if (
      result.nextForwardToken === previousToken ||
      result.events.length === 0
    ) {
      break;
    }

    previousToken = nextToken;
    nextToken = result.nextForwardToken;
  }

  if (!hasLogs) {
    log.dim("No log events available yet.");
  }

  // If pipeline is still active, keep streaming
  if (isActive) {
    console.log("");
    console.log(chalk.gray("  ↓ Streaming... (Ctrl+C to stop)"));
    console.log("");

    // Use a promise + SIGINT handler to allow returning to caller on Ctrl+C
    await new Promise<void>((resolve) => {
      const streamInterval = setInterval(async () => {
        try {
          const result = await getLogEvents(
            context.profile,
            context.region,
            buildLogInfo.logGroupName,
            buildLogInfo.logStreamName,
            nextToken
          );

          for (const event of result.events) {
            printLogLine(event.message);
          }

          if (
            result.nextForwardToken !== previousToken &&
            result.events.length > 0
          ) {
            previousToken = nextToken;
            nextToken = result.nextForwardToken;
          }
        } catch {
          // Silently skip polling errors
        }
      }, 2000);

      const onSignal = (): void => {
        clearInterval(streamInterval);
        process.off("SIGINT", onSignal);
        process.off("SIGTERM", onSignal);
        console.log("");
        log.dim("Stopped streaming.");
        console.log("");
        resolve();
      };
      process.on("SIGINT", onSignal);
      process.on("SIGTERM", onSignal);
    });
  } else {
    console.log("");
    console.log(chalk.gray("  ─ End of logs ─"));
    console.log("");
  }
}

function printLogLine(line: string): void {
  if (line.startsWith("[Container]")) {
    console.log(chalk.cyan(line));
  } else if (
    line.includes("FAILED") ||
    line.includes("Error") ||
    line.includes("error")
  ) {
    console.log(chalk.red(line));
  } else if (
    line.includes("Succeeded") ||
    line.includes("SUCCEEDED") ||
    line.includes("✓")
  ) {
    console.log(chalk.green(line));
  } else {
    console.log(chalk.white(line));
  }
}

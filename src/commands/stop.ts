import { select, confirm, input } from "@inquirer/prompts";
import chalk from "chalk";
import boxen from "boxen";
import { loadConfig } from "../config/loader.js";
import { stopPipelineExecution, getExecutionStatus, type PipelineExecutionStatus } from "../aws/pipeline.js";
import { getAllTrackedExecutions, updateTrackedExecution } from "../state/manager.js";
import { formatElapsed } from "../ui/format.js";
import {
  ensureCredentials,
  resolveExecutionReference,
  resolvePipelineContext,
} from "./shared.js";
import * as log from "../ui/logger.js";

export async function stopCommand(
  envName?: string,
  pipelineName?: string
): Promise<void> {
  const config = loadConfig();

  let resolvedEnvName: string;
  let resolvedPipelineName: string;
  let profile: string;
  let region: string;

  if (envName) {
    const context = await resolvePipelineContext(config, envName, pipelineName);
    resolvedEnvName = context.envName;
    resolvedPipelineName = context.pipeline.name;
    profile = context.profile;
    region = context.region;
  } else {
    // Show only environments/pipelines that have active executions
    const tracked = await getAllTrackedExecutions();
    const active = tracked.filter(
      (t) => t.status === "InProgress" || t.status === "Unknown"
    );

    if (active.length === 0) {
      const context = await resolvePipelineContext(config);
      resolvedEnvName = context.envName;
      resolvedPipelineName = context.pipeline.name;
      profile = context.profile;
      region = context.region;
    } else if (active.length === 1) {
      const exec = active[0];
      resolvedEnvName = exec.env;
      resolvedPipelineName = exec.pipeline;
      profile = exec.profile;
      region = exec.region;
    } else {
      const selected = await select({
        message: "Select a running pipeline to stop:",
        choices: active.map((exec) => ({
          name: `${chalk.cyan.bold(exec.env.toUpperCase().padEnd(5))} ${chalk.white(exec.pipeline)} ${chalk.gray(`[${formatElapsed(exec.startedAt)}]`)}`,
          value: exec.executionId,
        })),
      });

      const exec = active.find((e) => e.executionId === selected)!;
      resolvedEnvName = exec.env;
      resolvedPipelineName = exec.pipeline;
      profile = exec.profile;
      region = exec.region;
    }
  }

  await ensureCredentials(resolvedEnvName, profile);

  const trackedExec = await resolveExecutionReference(
    {
      envName: resolvedEnvName,
      pipeline: { name: resolvedPipelineName },
      profile,
      region,
    },
    { requireActive: true }
  );

  if (!trackedExec) {
    log.info(
      `No active execution found for '${resolvedPipelineName}' in ${resolvedEnvName}.`
    );
    return;
  }

  // Get current status for display
  let currentStatus: PipelineExecutionStatus | undefined;
  try {
    currentStatus = await getExecutionStatus(
      profile,
      region,
      resolvedPipelineName,
      trackedExec.executionId
    );
  } catch {
    // Continue even if status fetch fails
  }

  // Show execution details
  const detailLines: string[] = [];
  detailLines.push(
    `${chalk.bold("Pipeline:")}   ${chalk.white(resolvedPipelineName)}`
  );
  detailLines.push(
    `${chalk.bold("Env:")}        ${chalk.cyan(resolvedEnvName.toUpperCase())}`
  );
  detailLines.push(
    `${chalk.bold("Execution:")}  ${chalk.gray(trackedExec.executionId)}`
  );
  detailLines.push(
    `${chalk.bold("Running:")}    ${chalk.yellow(formatElapsed(trackedExec.startedAt))}`
  );

  if (currentStatus) {
    const stagesSummary = currentStatus.stages
      .map((s) => {
        switch (s.status) {
          case "Succeeded":
            return chalk.green(`${s.stageName} ✓`);
          case "InProgress":
            return chalk.yellow(`${s.stageName} ⟳`);
          case "Failed":
            return chalk.red(`${s.stageName} ✗`);
          default:
            return chalk.gray(`${s.stageName} ○`);
        }
      })
      .join(chalk.gray(" → "));
    detailLines.push(`${chalk.bold("Stages:")}     ${stagesSummary}`);
  }

  console.log("");
  console.log(
    boxen(detailLines.join("\n"), {
      borderStyle: "round",
      borderColor: "#f44336",
      padding: { left: 1, right: 1, top: 0, bottom: 0 },
      title: chalk.red.bold(" Stop Pipeline "),
      titleAlignment: "center",
    })
  );
  console.log("");

  // Ask how to stop
  const stopMode = await select({
    message: "How do you want to stop this pipeline?",
    choices: [
      {
        name: `${chalk.yellow("Stop gracefully")} ${chalk.gray("— Wait for in-progress actions to finish, then stop")}`,
        value: "graceful" as const,
      },
      {
        name: `${chalk.red("Abandon immediately")} ${chalk.gray("— Stop all actions immediately without waiting")}`,
        value: "abandon" as const,
      },
      {
        name: chalk.gray("Cancel (do nothing)"),
        value: "cancel" as const,
      },
    ],
  });

  if (stopMode === "cancel") {
    log.info("No action taken.");
    return;
  }

  const abandon = stopMode === "abandon";

  // Optional reason
  const reason = await input({
    message: "Reason for stopping (optional):",
  });

  // Confirm
  const modeLabel = abandon
    ? chalk.red("ABANDON")
    : chalk.yellow("STOP");
  const confirmed = await confirm({
    message: `Are you sure you want to ${modeLabel} this pipeline?`,
    default: false,
  });

  if (!confirmed) {
    log.info("Cancelled.");
    return;
  }

  // Execute stop
  const stopReason = reason.trim() || (abandon ? "Abandoned via deploy-cli" : "Stopped via deploy-cli");

  await log.withSpinner(
    abandon ? "Abandoning pipeline execution" : "Stopping pipeline execution",
    () =>
      stopPipelineExecution(
        profile,
        region,
        resolvedPipelineName,
        trackedExec.executionId,
        stopReason,
        abandon
      ),
    abandon
      ? "Pipeline execution abandoned"
      : "Pipeline execution stopped"
  );

  // Update local state
  await updateTrackedExecution(trackedExec.executionId, "Stopped");

  console.log("");
}

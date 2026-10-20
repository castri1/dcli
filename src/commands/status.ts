import chalk from "chalk";
import boxen from "boxen";
import { loadConfig } from "../config/loader.js";
import { getExecutionStatus } from "../aws/pipeline.js";
import { formatElapsed } from "../ui/format.js";
import {
  ensureCredentials,
  printJson,
  resolveExecutionReference,
  resolvePipelineContext,
} from "./shared.js";
import * as log from "../ui/logger.js";

export async function statusCommand(
  envName?: string,
  pipelineName?: string,
  options?: { json?: boolean }
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
    log.dim(
      "Start a deployment with: deploy-cli deploy " +
        resolvedEnvName +
        " " +
        resolvedPipelineName
    );
    return;
  }

  // Get current status
  const status = await log.withSpinner(
    "Fetching pipeline status",
    () =>
      getExecutionStatus(
        context.profile,
        context.region,
        resolvedPipelineName,
        trackedExec.executionId
      ),
    "Status retrieved"
  );

  if (options?.json) {
    printJson({
      env: resolvedEnvName,
      pipeline: resolvedPipelineName,
      executionId: trackedExec.executionId,
      startedAt: trackedExec.startedAt,
      trackedSource: trackedExec.source,
      status,
    });
    return;
  }

  // Build status display
  const overallColor = statusColor(status.status);
  const overallIcon = statusIcon(status.status);

  const lines: string[] = [];
  lines.push(
    `${chalk.bold("Pipeline:")}   ${chalk.white(resolvedPipelineName)}`
  );
  lines.push(
    `${chalk.bold("Env:")}        ${chalk.cyan(resolvedEnvName.toUpperCase())}`
  );
  lines.push(
    `${chalk.bold("Execution:")}  ${chalk.gray(trackedExec.executionId)}`
  );
  lines.push(
    `${chalk.bold("Status:")}     ${overallIcon} ${overallColor(status.status)}`
  );
  lines.push(
    `${chalk.bold("Elapsed:")}    ${chalk.white(formatElapsed(trackedExec.startedAt))}`
  );

  // Git info
  if (status.sourceRevision) {
    const rev = status.sourceRevision;
    if (rev.commitId) {
      const msg = rev.commitMessage ? ` — "${rev.commitMessage}"` : "";
      lines.push(
        `${chalk.bold("Commit:")}     ${chalk.yellow(rev.commitId)}${chalk.gray(msg)}`
      );
    }
    if (rev.branch) {
      lines.push(
        `${chalk.bold("Branch:")}     ${chalk.magenta(rev.branch)}`
      );
    }
  }

  lines.push("");
  lines.push(chalk.bold("Stages:"));

  for (const stage of status.stages) {
    const icon = stageStatusIcon(stage.status);
    const elapsed =
      stage.startTime && stage.status === "InProgress"
        ? chalk.yellow(` (${formatElapsed(stage.startTime)})`)
        : "";
    const approval = stage.waitingForApproval
      ? chalk.yellow(" (awaiting approval)")
      : "";
    const error = stage.errorMessage
      ? `\n         ${chalk.red(`Error: ${stage.errorMessage}`)}`
      : "";
    lines.push(`  ${icon} ${stage.stageName}${elapsed}${approval}${error}`);
  }

  console.log("");
  console.log(
    boxen(lines.join("\n"), {
      borderStyle: "round",
      borderColor: borderColorForStatus(status.status),
      padding: { left: 1, right: 1, top: 0, bottom: 0 },
      title: chalk.bold(" Pipeline Status "),
      titleAlignment: "center",
    })
  );
  console.log("");
}

function statusColor(status: string): (text: string) => string {
  switch (status) {
    case "InProgress":
      return chalk.yellow;
    case "Succeeded":
      return chalk.green;
    case "Failed":
      return chalk.red;
    case "Stopped":
      return chalk.gray;
    default:
      return chalk.white;
  }
}

function statusIcon(status: string): string {
  switch (status) {
    case "InProgress":
      return chalk.yellow("⟳");
    case "Succeeded":
      return chalk.green("✓");
    case "Failed":
      return chalk.red("✗");
    case "Stopped":
      return chalk.gray("■");
    default:
      return chalk.gray("○");
  }
}

function borderColorForStatus(status: string): string {
  switch (status) {
    case "InProgress":
      return "#f5a623";
    case "Succeeded":
      return "#4caf50";
    case "Failed":
      return "#f44336";
    default:
      return "#3a7bd5";
  }
}

function stageStatusIcon(status: string): string {
  switch (status) {
    case "Succeeded":
      return chalk.green("✓");
    case "InProgress":
      return chalk.yellow("⟳");
    case "Failed":
      return chalk.red("✗");
    case "Stopped":
      return chalk.gray("■");
    default:
      return chalk.gray("○");
  }
}

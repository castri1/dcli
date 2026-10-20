import chalk from "chalk";
import type { StageStatus, PipelineExecutionStatus, TriggerType } from "../aws/pipeline.js";

/**
 * Format elapsed time as a human-readable string.
 */
export function formatElapsed(startTime: Date | string): string {
  const start =
    typeof startTime === "string" ? new Date(startTime) : startTime;
  const elapsed = Math.max(0, Date.now() - start.getTime());
  const seconds = Math.floor(elapsed / 1000);
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;

  if (minutes > 0) {
    return `${minutes}m ${remainingSeconds.toString().padStart(2, "0")}s`;
  }
  return `${seconds}s`;
}

/**
 * Format a stage status icon.
 */
function stageIcon(status: StageStatus["status"]): string {
  switch (status) {
    case "Succeeded":
      return chalk.green("✓");
    case "InProgress":
      return chalk.yellow("⟳");
    case "Failed":
      return chalk.red("✗");
    case "Stopped":
      return chalk.gray("■");
    case "Pending":
    default:
      return chalk.gray("○");
  }
}

/**
 * Format a stage name with its status.
 */
function formatStage(stage: StageStatus): string {
  const name = stage.stageName;

  // Special icon for approval waiting
  if (stage.waitingForApproval) {
    return `${name} ${chalk.yellow("⏳")} ${chalk.yellow.dim("(awaiting approval)")}`;
  }

  const icon = stageIcon(stage.status);

  if (stage.status === "InProgress" && stage.startTime) {
    const elapsed = formatElapsed(stage.startTime);
    return `${name} ${icon} (${chalk.yellow(elapsed)})`;
  }

  return `${name} ${icon}`;
}

/**
 * Format a trigger type tag for display.
 * Returns empty string for "self" (no tag needed — it's your own execution).
 */
function formatTriggerTag(triggeredBy?: TriggerType): string {
  switch (triggeredBy) {
    case "self":
    case undefined:
      return "";
    case "webhook":
      return chalk.cyan("[webhook] ");
    case "manual":
      return chalk.yellow("[manual] ");
    case "event":
      return chalk.magenta("[event] ");
    case "schedule":
      return chalk.blue("[schedule] ");
    case "unknown":
    default:
      return chalk.gray("[ext] ");
  }
}

/**
 * Format a single pipeline execution as a compact one-liner.
 *
 * Example output:
 *   DEV  my-app-dev  Source ✓ → Build ⟳ (1m34s) → Deploy ○     [1m46s]
 *   DEV  my-api-dev  [webhook] Source ✓ → Build ⟳ (1m12s)       [1m44s]
 */
export function formatPipelineLine(
  env: string,
  pipelineStatus: PipelineExecutionStatus,
  startedAt: string,
  triggeredBy: TriggerType = "self",
  pipelineNameWidth: number = 25
): string {
  const envLabel = chalk.bold.cyan(env.toUpperCase().padEnd(5));
  const pipelineName = chalk.white(
    pipelineStatus.pipelineName.length > pipelineNameWidth
      ? pipelineStatus.pipelineName.slice(0, pipelineNameWidth - 1) + "…"
      : pipelineStatus.pipelineName.padEnd(pipelineNameWidth)
  );
  const triggerTag = formatTriggerTag(triggeredBy);

  const stages = pipelineStatus.stages
    .map((s) => formatStage(s))
    .join(chalk.gray(" → "));

  const elapsed = chalk.gray(`[${formatElapsed(startedAt)}]`);

  let finalStatus = "";
  switch (pipelineStatus.status) {
    case "Succeeded":
      finalStatus = chalk.green.bold(" ✓ SUCCESS");
      break;
    case "Failed":
      finalStatus = chalk.red.bold(" ✗ FAILED");
      break;
    case "Stopped":
      finalStatus = chalk.gray.bold(" ■ STOPPED");
      break;
  }

  let line = `${envLabel} ${pipelineName} ${triggerTag}${stages}  ${elapsed}${finalStatus}`;

  // Add git info as a second line if available
  const rev = pipelineStatus.sourceRevision;
  if (rev && (rev.commitId || rev.branch)) {
    const parts: string[] = [];
    if (rev.commitId) parts.push(chalk.yellow(rev.commitId));
    if (rev.commitMessage) {
      const msg = rev.commitMessage.length > 50
        ? rev.commitMessage.slice(0, 50) + "..."
        : rev.commitMessage;
      parts.push(chalk.gray(`"${msg}"`));
    }
    if (rev.branch) parts.push(chalk.magenta(`(${rev.branch})`));
    line += `\n      ${chalk.gray("↳")} ${parts.join(" ")}`;
  }

  return line;
}

/**
 * Format a pipeline that failed to fetch status (e.g., auth error).
 */
export function formatPipelineError(
  env: string,
  pipelineName: string,
  error: string
): string {
  const envLabel = chalk.bold.cyan(env.toUpperCase().padEnd(5));
  const name = chalk.white(pipelineName.padEnd(25));
  return `${envLabel} ${name} ${chalk.red(`Error: ${error}`)}`;
}

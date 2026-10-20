import chalk from "chalk";
import gradient from "gradient-string";
import Table from "cli-table3";
import { loadConfig } from "../config/loader.js";
import { getExecutionHistory, getExecutionStatus } from "../aws/pipeline.js";
import {
  ensureCredentials,
  parsePositiveInt,
  printJson,
  resolvePipelineContext,
} from "./shared.js";
import * as log from "../ui/logger.js";

const HEADER_GRADIENT = gradient(["#00d2ff", "#3a7bd5"]);

function formatDuration(start?: Date, end?: Date): string {
  if (!start || !end) return chalk.gray("—");
  const ms = end.getTime() - start.getTime();
  if (ms < 0) return chalk.gray("—");
  const seconds = Math.floor(ms / 1000);
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  if (minutes > 0) {
    return `${minutes}m ${remainingSeconds.toString().padStart(2, "0")}s`;
  }
  return `${seconds}s`;
}

function formatStatus(status: string): string {
  switch (status) {
    case "Succeeded":
      return chalk.green("✓ Passed");
    case "InProgress":
      return chalk.yellow("⟳ Running");
    case "Failed":
      return chalk.red("✗ Failed");
    case "Stopped":
      return chalk.gray("■ Stopped");
    case "Superseded":
      return chalk.gray("⊘ Superseded");
    case "Cancelled":
      return chalk.gray("✕ Cancelled");
    default:
      return chalk.white(status);
  }
}

function formatTrigger(trigger: string): string {
  switch (trigger) {
    case "webhook":
      return chalk.cyan("webhook");
    case "manual":
      return chalk.yellow("manual");
    case "event":
      return chalk.magenta("event");
    case "schedule":
      return chalk.blue("schedule");
    default:
      return chalk.gray(trigger);
  }
}

function formatTime(date?: Date): string {
  if (!date) return chalk.gray("—");
  return chalk.white(
    date.toLocaleString("en-US", {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
      hour12: true,
    })
  );
}

export async function historyCommand(
  envName?: string,
  pipelineName?: string,
  options?: {
    json?: boolean;
    limit?: string;
    status?: string;
    details?: string | boolean;
  }
): Promise<void> {
  const config = loadConfig();
  const context = await resolvePipelineContext(config, envName, pipelineName);
  const resolvedEnvName = context.envName;
  const resolvedPipelineName = context.pipeline.name;
  const limit = parsePositiveInt(options?.limit, 10);

  await ensureCredentials(resolvedEnvName, context.profile);

  // Fetch history
  const fullHistory = await log.withSpinner(
    "Fetching deployment history",
    () =>
      getExecutionHistory(
        context.profile,
        context.region,
        resolvedPipelineName,
        limit
      ),
    "History retrieved"
  );

  const history = options?.status
    ? fullHistory.filter(
        (entry) =>
          entry.status.toLowerCase() === options.status?.toLowerCase()
      )
    : fullHistory;

  if (history.length === 0) {
    log.info(`No execution history for '${resolvedPipelineName}'.`);
    return;
  }

  if (options?.details) {
    const requestedExecutionId =
      typeof options.details === "string" ? options.details : history[0].executionId;
    const entry = history.find(
      (historyEntry) => historyEntry.executionId === requestedExecutionId
    );

    if (!entry) {
      throw new Error(
        `Execution '${requestedExecutionId}' was not found in the retrieved history window.`
      );
    }

    const detail = await log.withSpinner(
      `Fetching execution details for '${requestedExecutionId}'`,
      () =>
        getExecutionStatus(
          context.profile,
          context.region,
          resolvedPipelineName,
          requestedExecutionId
        ),
      "Execution details retrieved"
    );

    printJson({
      env: resolvedEnvName,
      pipeline: resolvedPipelineName,
      summary: entry,
      detail,
    });
    return;
  }

  if (options?.json) {
    printJson({
      env: resolvedEnvName,
      pipeline: resolvedPipelineName,
      count: history.length,
      history,
    });
    return;
  }

  console.log("");
  console.log(
    "  " +
      HEADER_GRADIENT(
      `Deployment History — ${resolvedPipelineName} (${resolvedEnvName.toUpperCase()})`
      )
  );
  console.log("");

  const table = new Table({
    head: [
      chalk.cyan.bold("Status"),
      chalk.cyan.bold("Duration"),
      chalk.cyan.bold("Trigger"),
      chalk.cyan.bold("Started"),
      chalk.cyan.bold("Commit"),
    ],
    chars: {
      top: "─",
      "top-mid": "┬",
      "top-left": "  ┌",
      "top-right": "┐",
      bottom: "─",
      "bottom-mid": "┴",
      "bottom-left": "  └",
      "bottom-right": "┘",
      left: "  │",
      "left-mid": "  ├",
      mid: "─",
      "mid-mid": "┼",
      right: "│",
      "right-mid": "┤",
      middle: "│",
    },
    style: {
      head: [],
      border: ["gray"],
      "padding-left": 1,
      "padding-right": 1,
    },
    colWidths: [16, 12, 12, 22, 30],
    wordWrap: true,
  });

  let successCount = 0;
  let failedCount = 0;

  for (const entry of history) {
    if (entry.status === "Succeeded") successCount++;
    if (entry.status === "Failed") failedCount++;

    const commitMsg = entry.sourceRevisionSummary
      ? chalk.gray(
          entry.sourceRevisionSummary.length > 27
            ? entry.sourceRevisionSummary.slice(0, 27) + "..."
            : entry.sourceRevisionSummary
        )
      : chalk.gray("—");

    table.push([
      formatStatus(entry.status),
      formatDuration(entry.startTime, entry.lastUpdateTime),
      formatTrigger(entry.triggerType),
      formatTime(entry.startTime),
      commitMsg,
    ]);
  }

  console.log(table.toString());
  console.log("");

  const total = history.length;
  const successRate =
    total > 0 ? Math.round((successCount / total) * 100) : 0;
  const successRateColor =
    successRate >= 80
      ? chalk.green
      : successRate >= 50
        ? chalk.yellow
        : chalk.red;

  console.log(
    chalk.gray(
      `  Showing ${total} execution${total !== 1 ? "s" : ""} | Success rate: ${successRateColor(`${successRate}%`)} | ${chalk.green(`${successCount} passed`)} | ${chalk.red(`${failedCount} failed`)}`
    )
  );
  console.log("");
}

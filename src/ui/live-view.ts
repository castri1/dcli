import logUpdate from "log-update";
import boxen from "boxen";
import chalk from "chalk";
import { getExecutionStatus, getLatestExecution } from "../aws/pipeline.js";
import {
  getAllTrackedExecutions,
  updateTrackedExecution,
  upsertExternalExecution,
} from "../state/manager.js";
import type { TrackedExecution } from "../state/types.js";
import type { PipelineExecutionStatus } from "../aws/pipeline.js";
import type { AppConfig } from "../config/types.js";
import {
  formatPipelineLine,
  formatPipelineError,
  formatElapsed,
} from "./format.js";
import { notifyPipelineComplete, notifyApprovalNeeded, notifyStageTransition } from "./notifications.js";
import {
  isRawModeSupported,
  enableRawMode,
  disableRawMode,
  onKeypress,
  cleanupKeypress,
} from "./keypress.js";
import { deployCommand } from "../commands/deploy.js";
import { approveCommand } from "../commands/approve.js";
import { stopCommand } from "../commands/stop.js";
import { logsCommand } from "../commands/logs.js";
import { historyCommand } from "../commands/history.js";
import { isUserCancelled } from "../errors.js";
import * as log from "./logger.js";

// Max concurrent AWS API calls to avoid rate limiting
const MAX_CONCURRENCY = 4;
// Discovery runs every Nth poll cycle (not every cycle)
const DISCOVERY_INTERVAL = 3;

interface TrackedPipelineView {
  execution: TrackedExecution;
  status?: PipelineExecutionStatus;
  error?: string;
}

export interface LiveViewFilter {
  envName?: string;
  pipelineName?: string;
}

export interface LiveViewResult {
  reason: "completed" | "detached";
}

function matchesFilter(
  execution: Pick<TrackedExecution, "env" | "pipeline">,
  filter?: LiveViewFilter
): boolean {
  if (!filter) {
    return true;
  }

  if (filter.envName && execution.env !== filter.envName) {
    return false;
  }

  if (filter.pipelineName && execution.pipeline !== filter.pipelineName) {
    return false;
  }

  return true;
}

/**
 * Run async tasks with a concurrency limit.
 */
async function withConcurrencyLimit<T>(
  tasks: (() => Promise<T>)[],
  limit: number
): Promise<T[]> {
  const results: T[] = [];
  let index = 0;

  async function worker(): Promise<void> {
    while (index < tasks.length) {
      const i = index++;
      results[i] = await tasks[i]();
    }
  }

  const workers = Array.from(
    { length: Math.min(limit, tasks.length) },
    () => worker()
  );
  await Promise.all(workers);
  return results;
}

/**
 * Check if an error looks like an expired credential / auth error.
 */
function isCredentialError(err: unknown): boolean {
  if (!(err instanceof Error)) return false;
  const msg = err.message.toLowerCase();
  return (
    msg.includes("expired") ||
    msg.includes("token") ||
    msg.includes("unrecognizedclient") ||
    msg.includes("not authorized") ||
    msg.includes("security token") ||
    msg.includes("credentials")
  );
}

/**
 * Format an error for display in the live view.
 */
function friendlyError(err: unknown, envName: string): string {
  if (isCredentialError(err)) {
    return `Credentials expired — run: deploy-cli login ${envName}`;
  }
  if (err instanceof Error) {
    // Trim verbose AWS SDK errors
    const msg = err.message;
    if (msg.length > 80) return msg.slice(0, 77) + "...";
    return msg;
  }
  return "Unknown error";
}

/**
 * Scan all configured pipelines for external (non-tracked) InProgress executions.
 */
async function discoverExternalExecutions(
  config: AppConfig,
  filter?: LiveViewFilter
): Promise<void> {
  const tracked = await getAllTrackedExecutions();
  const trackedIds = new Set(tracked.map((t) => t.executionId));

  const tasks: (() => Promise<void>)[] = [];

  for (const [envName, env] of Object.entries(config.environments)) {
    if (filter?.envName && filter.envName !== envName) {
      continue;
    }

    for (const pipeline of env.pipelines) {
      if (filter?.pipelineName && filter.pipelineName !== pipeline.name) {
        continue;
      }

      tasks.push(async () => {
        try {
          const latest = await getLatestExecution(
            env.profile,
            env.region,
            pipeline.name
          );

          if (
            latest &&
            latest.status === "InProgress" &&
            !trackedIds.has(latest.executionId)
          ) {
            await upsertExternalExecution({
              env: envName,
              pipeline: pipeline.name,
              profile: env.profile,
              region: env.region,
              executionId: latest.executionId,
              startedAt: latest.startTime.toISOString(),
              status: "InProgress",
              triggeredBy: latest.triggerType,
            });
          }
        } catch {
          // Silently skip discovery errors
        }
      });
    }
  }

  await withConcurrencyLimit(tasks, MAX_CONCURRENCY);
}

/**
 * Start the live polling view that shows all tracked pipelines.
 * Returns a Promise that resolves when all pipelines complete or the user presses Ctrl+C.
 */
export function startLiveView(
  pollIntervalSeconds: number,
  config: AppConfig,
  options?: { filter?: LiveViewFilter }
): Promise<LiveViewResult> {
  let stopped = false;
  let paused = false;
  let timeoutId: ReturnType<typeof setTimeout> | undefined;
  let pollCount = 0;
  const notifiedCompletions = new Set<string>();
  const notifiedApprovals = new Set<string>();
  const enableNotifications = config.defaults.notifications;
  const filter = options?.filter;
  let stopReason: LiveViewResult["reason"] = "completed";
  const interactiveMode = isRawModeSupported();
  let cleanupKeypressHandler: (() => void) | null = null;

  // Cache completed execution statuses to avoid re-polling AWS
  const completedCache = new Map<string, PipelineExecutionStatus>();

  // Track previous stage statuses per execution for stage transition detection
  const previousStageStatuses = new Map<string, Map<string, string>>();

  let resolvePromise: (result: LiveViewResult) => void;
  const promise = new Promise<LiveViewResult>((resolve) => {
    resolvePromise = resolve;
  });

  /**
   * Pause the tracker, run a command, then resume.
   */
  async function pauseAndRunCommand(fn: () => Promise<void>): Promise<void> {
    paused = true;
    // Tear down keypress and raw mode so inquirer prompts work
    if (cleanupKeypressHandler) {
      cleanupKeypressHandler();
      cleanupKeypressHandler = null;
    }
    disableRawMode();
    logUpdate.done(); // finalize current render
    console.log("");

    try {
      await fn();
    } catch (err) {
      if (!isUserCancelled(err)) {
        log.errorIfNew(err);
      }
    }

    console.log("");

    // Resume tracker
    paused = false;
    if (!stopped) {
      setupKeypress();
      // Immediate re-render with fresh data
      await poll();
    }
  }

  /**
   * Set up keypress listening for interactive shortcuts.
   */
  function setupKeypress(): void {
    if (!interactiveMode || stopped) return;
    enableRawMode();
    cleanupKeypressHandler = onKeypress((key) => {
      switch (key) {
        case "d":
          pauseAndRunCommand(() =>
            deployCommand(undefined, undefined, { skipLiveView: true })
          );
          break;
        case "a":
          pauseAndRunCommand(() => approveCommand());
          break;
        case "s":
          pauseAndRunCommand(() => stopCommand());
          break;
        case "l":
          pauseAndRunCommand(() => logsCommand());
          break;
        case "h":
          pauseAndRunCommand(() => historyCommand());
          break;
        case "q":
        case "ctrl+c":
        case "escape":
          stopReason = "detached";
          stop();
          break;
      }
    });
  }

  async function poll(): Promise<void> {
    if (stopped) return;
    pollCount++;

    // Discovery runs less frequently to reduce API calls
    if (pollCount === 1 || pollCount % DISCOVERY_INTERVAL === 0) {
      await discoverExternalExecutions(config, filter);
    }

    const tracked = (await getAllTrackedExecutions()).filter((execution) =>
      matchesFilter(execution, filter)
    );

    if (tracked.length === 0) {
      const scopeLabel = filter?.pipelineName
        ? `${filter.envName || "*"}/${filter.pipelineName}`
        : filter?.envName || "all environments";
      const emptyContent = chalk.gray("No tracked deployments in scope.\n") +
        chalk.gray(
          filter
            ? `Watching ${scopeLabel} for new executions...`
            : "Start one with: deploy-cli deploy <env> [pipeline]"
        );
      logUpdate(
        boxen(emptyContent, {
          borderStyle: "round",
          borderColor: "#3a7bd5",
          padding: { left: 1, right: 1, top: 0, bottom: 0 },
          title: chalk.bold(" Pipeline Tracker "),
          titleAlignment: "center",
        })
      );
      return;
    }

    // Build tasks for fetching status, with concurrency control
    const tasks = tracked.map((exec) => async (): Promise<TrackedPipelineView> => {
      // Use cache for completed executions
      if (
        exec.status !== "InProgress" &&
        exec.status !== "Unknown"
      ) {
        const cached = completedCache.get(exec.executionId);
        if (cached) {
          return { execution: exec, status: cached };
        }
        // Fetch once more to populate cache
        try {
          const status = await getExecutionStatus(
            exec.profile, exec.region, exec.pipeline, exec.executionId
          );
          completedCache.set(exec.executionId, status);
          return { execution: exec, status };
        } catch {
          return { execution: exec, error: "Could not fetch final status" };
        }
      }

      try {
        const status = await getExecutionStatus(
          exec.profile, exec.region, exec.pipeline, exec.executionId
        );

        // Update state if the execution completed
        const newStatus = mapApiStatus(status.status);
        if (newStatus !== exec.status) {
          const wasActive = exec.status === "InProgress" || exec.status === "Unknown";
          exec.status = newStatus;
          await updateTrackedExecution(exec.executionId, newStatus);

          // Cache the completed status
          if (newStatus !== "InProgress" && newStatus !== "Unknown") {
            completedCache.set(exec.executionId, status);
          }

          // Send OS notification on completion
          if (
            enableNotifications &&
            wasActive &&
            (newStatus === "Succeeded" || newStatus === "Failed" || newStatus === "Stopped") &&
            !notifiedCompletions.has(exec.executionId)
          ) {
            notifiedCompletions.add(exec.executionId);
            notifyPipelineComplete(
              exec.pipeline, exec.env, newStatus, formatElapsed(exec.startedAt)
            );
          }
        }

        // Notify about pending approvals
        if (enableNotifications && status.pendingApprovals) {
          for (const approval of status.pendingApprovals) {
            const key = `${exec.executionId}:${approval.stageName}`;
            if (!notifiedApprovals.has(key)) {
              notifiedApprovals.add(key);
              notifyApprovalNeeded(exec.pipeline, exec.env, approval.stageName);
            }
          }
        }

        // Detect per-stage transitions and fire notifications
        if (enableNotifications && status.stages) {
          const prevStages = previousStageStatuses.get(exec.executionId);
          const currentStages = new Map<string, string>();

          for (const stage of status.stages) {
            currentStages.set(stage.stageName, stage.status);

            // Skip stage notifications on the first poll (just populate baseline)
            if (pollCount > 1 && prevStages) {
              const prevStatus = prevStages.get(stage.stageName);

              if (prevStatus && prevStatus !== stage.status) {
                // Stage status changed — determine the transition type
                if (stage.status === "InProgress") {
                  notifyStageTransition(
                    exec.pipeline, exec.env, stage.stageName, "started"
                  );
                } else if (stage.status === "Succeeded") {
                  const duration = stage.startTime
                    ? formatElapsed(stage.startTime)
                    : undefined;
                  notifyStageTransition(
                    exec.pipeline, exec.env, stage.stageName, "passed", duration
                  );
                } else if (stage.status === "Failed") {
                  notifyStageTransition(
                    exec.pipeline, exec.env, stage.stageName, "failed"
                  );
                }
              }
            }
          }

          previousStageStatuses.set(exec.executionId, currentStages);
        }

        return { execution: exec, status };
      } catch (err) {
        return { execution: exec, error: friendlyError(err, exec.env) };
      }
    });

    const views = await withConcurrencyLimit(tasks, MAX_CONCURRENCY);

    const activeCount = views.filter(
      (v) =>
        v.execution.status === "InProgress" ||
        v.execution.status === "Unknown"
    ).length;

    // Calculate dynamic pipeline name width
    const maxNameLen = Math.max(
      ...views.map((v) => v.execution.pipeline.length),
      15 // minimum
    );
    const pipelineNameWidth = Math.min(maxNameLen + 2, 40); // cap at 40

    // Build the box content
    const pipelineLines: string[] = [];

    for (const view of views) {
      if (view.error) {
        pipelineLines.push(
          formatPipelineError(
            view.execution.env,
            view.execution.pipeline,
            view.error
          )
        );
      } else if (view.status) {
        pipelineLines.push(
          formatPipelineLine(
            view.execution.env,
            view.status,
            view.execution.startedAt,
            view.execution.triggeredBy,
            pipelineNameWidth
          )
        );
      }
    }

    // Check for pending approvals
    const allApprovals = views.flatMap((v) =>
      v.status?.pendingApprovals?.map((a) => ({
        env: v.execution.env,
        ...a,
      })) || []
    );

    let approvalAlert = "";
    if (allApprovals.length > 0) {
      const approvalLines = allApprovals.map(
        (a) =>
          chalk.yellow(`  ⏳ ${a.env.toUpperCase()} / ${a.pipelineName} waiting for approval at "${a.stageName}"`)
      );
      approvalAlert =
        "\n" +
        approvalLines.join("\n") +
        "\n" +
        chalk.yellow.dim("  Run: deploy-cli approve");
    }

    // Header info
    let headerText: string;
    if (activeCount === views.length) {
      headerText = chalk.bold(`${activeCount} active deployment${activeCount > 1 ? "s" : ""}`) +
        chalk.gray(` | Polling every ${pollIntervalSeconds}s`);
    } else if (activeCount > 0) {
      headerText = chalk.bold(`${views.length} deployment${views.length > 1 ? "s" : ""}`) +
        chalk.gray(` (${chalk.green(activeCount + " active")}, ${chalk.gray((views.length - activeCount) + " done")})`) +
        chalk.gray(` | Polling every ${pollIntervalSeconds}s`);
    } else {
      headerText = chalk.green.bold("All deployments complete!");
    }

    // Build footer — hotkey bar if interactive, simple text if not
    let footer: string;
    if (activeCount > 0 && interactiveMode) {
      const hotkeyBar =
        chalk.gray("[") + chalk.white("d") + chalk.gray("]Deploy ") +
        chalk.gray("[") + chalk.white("a") + chalk.gray("]Approve ") +
        chalk.gray("[") + chalk.white("s") + chalk.gray("]Stop ") +
        chalk.gray("[") + chalk.white("l") + chalk.gray("]Logs ") +
        chalk.gray("[") + chalk.white("h") + chalk.gray("]History ") +
        chalk.gray("[") + chalk.white("q") + chalk.gray("]Quit");
      footer = "\n" + hotkeyBar;
    } else if (activeCount > 0) {
      footer = chalk.gray("\nCtrl+C to detach (pipelines keep running)");
    } else {
      footer = "";
    }

    const content = `${headerText}\n\n${pipelineLines.join("\n")}${approvalAlert}${footer}`;

    // Choose border color based on overall status
    let borderColor = "#3a7bd5";
    const hasFailed = views.some((v) => v.execution.status === "Failed");
    const allDone = activeCount === 0;
    if (hasFailed) {
      borderColor = "#f44336";
    } else if (allDone) {
      borderColor = "#4caf50";
    } else {
      borderColor = "#f5a623";
    }

    const boxTitle = allDone
      ? chalk.green.bold(" Pipeline Tracker — Complete ")
      : chalk.bold(" Pipeline Tracker ");

    // Only render when not paused (a command is running below)
    if (!paused) {
      logUpdate(
        boxen(content, {
          borderStyle: "round",
          borderColor,
          padding: { left: 1, right: 1, top: 0, bottom: 0 },
          title: boxTitle,
          titleAlignment: "center",
        })
      );
    }

    // If all completed, stop polling
    if (activeCount === 0) {
      stop();
    }
  }

  function stop(): void {
    if (stopped) return;
    stopped = true;
    if (timeoutId) {
      clearTimeout(timeoutId);
      timeoutId = undefined;
    }
    // Clean up keypress
    if (cleanupKeypressHandler) {
      cleanupKeypressHandler();
      cleanupKeypressHandler = null;
    }
    cleanupKeypress();
    logUpdate.done();
    process.off("SIGINT", onSignal);
    process.off("SIGTERM", onSignal);
    resolvePromise({ reason: stopReason });
  }

  // Self-scheduling: prevents overlapping polls
  async function scheduleLoop(): Promise<void> {
    if (stopped) return;
    await poll();

    // Set up keypress shortcuts after the first render
    if (pollCount === 1 && interactiveMode && !stopped) {
      setupKeypress();
    }

    if (!stopped) {
      timeoutId = setTimeout(scheduleLoop, pollIntervalSeconds * 1000);
    }
  }

  const onSignal = (): void => {
    stopReason = "detached";
    stop();
  };
  process.on("SIGINT", onSignal);
  process.on("SIGTERM", onSignal);

  scheduleLoop();

  return promise;
}

function mapApiStatus(
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

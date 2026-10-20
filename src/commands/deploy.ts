import chalk from "chalk";
import { loadConfig } from "../config/loader.js";
import { startPipeline, getExecutionStatus } from "../aws/pipeline.js";
import { getLocalGitInfo } from "../aws/git-info.js";
import { addTrackedExecution, getActiveExecutions } from "../state/manager.js";
import { startLiveView } from "../ui/live-view.js";
import { runHooks } from "../hooks/runner.js";
import { confirm } from "@inquirer/prompts";
import { DeployCliError } from "../errors.js";
import { ensureCredentials, resolvePipelineContext } from "./shared.js";
import * as log from "../ui/logger.js";

export async function deployCommand(
  envName?: string,
  pipelineName?: string,
  options?: { skipHooks?: boolean; skipLiveView?: boolean }
): Promise<void> {
  const config = loadConfig();
  const context = await resolvePipelineContext(config, envName, pipelineName);
  const resolvedEnvName = context.envName;
  const resolvedPipeline = context.pipeline;
  const profile = context.profile;
  const region = context.region;

  const resolvedPipelineName = resolvedPipeline.name;

  // Show local git context
  const gitInfo = getLocalGitInfo();
  if (gitInfo) {
    log.dim(
      `Git: ${chalk.yellow(gitInfo.commitId)} ${chalk.gray(`"${gitInfo.commitMessage}"`)} ${chalk.magenta(`(${gitInfo.branch})`)}`
    );
  }

  // Run pre-deploy hooks
  const skipHooks = options?.skipHooks || false;
  const preHooks = resolvedPipeline.hooks?.["pre-deploy"] || [];
  if (preHooks.length > 0 && !skipHooks) {
    console.log("");
    const { allPassed } = await runHooks("pre-deploy", preHooks, true);
    if (!allPassed) {
      throw new DeployCliError(
        "Deployment aborted due to pre-deploy hook failure. Use --skip-hooks to bypass."
      );
    }
    console.log("");
  }

  // Check if pipeline is already running
  const activeExecs = await getActiveExecutions();
  const alreadyRunning = activeExecs.find(
    (e) => e.env === resolvedEnvName && e.pipeline === resolvedPipelineName
  );
  if (alreadyRunning) {
    log.warn(
      `Pipeline '${resolvedPipelineName}' is already running in ${resolvedEnvName} (execution: ${alreadyRunning.executionId}).`
    );
    log.dim("Starting a new execution will supersede the current one.");
    const proceed = await confirm({
      message: "Continue anyway?",
      default: false,
    });
    if (!proceed) {
      log.info("Cancelled.");
      return;
    }
  }

  await ensureCredentials(resolvedEnvName, profile);

  // Start the pipeline
  const executionId = await log.withSpinner(
    `Starting pipeline '${resolvedPipelineName}' in ${resolvedEnvName}`,
    () => startPipeline(profile, region, resolvedPipelineName),
    `Pipeline started`
  );

  log.dim(`Execution ID: ${executionId}`);

  // Add to shared state
  await addTrackedExecution({
    env: resolvedEnvName,
    pipeline: resolvedPipelineName,
    profile,
    region,
    executionId,
    startedAt: new Date().toISOString(),
    status: "InProgress",
    triggeredBy: "self",
  });

  log.info("Tracking started. You can also run: deploy-cli watch");
  console.log("");

  // When called from within the tracker (via hotkey), skip starting a new live view.
  // The existing tracker will pick up the new pipeline from the state file.
  if (options?.skipLiveView) {
    log.success("Pipeline added to tracker.");
    return;
  }

  const liveViewResult = await startLiveView(
    config.defaults.pollIntervalSeconds,
    config,
    {
      filter: {
        envName: resolvedEnvName,
        pipelineName: resolvedPipelineName,
      },
    }
  );

  const postHooks = resolvedPipeline.hooks?.["post-deploy"] || [];
  if (
    postHooks.length > 0 &&
    !skipHooks &&
    liveViewResult.reason === "completed"
  ) {
    const finalStatus = await log.withSpinner(
      `Checking final status for '${resolvedPipelineName}'`,
      () =>
        getExecutionStatus(profile, region, resolvedPipelineName, executionId),
      "Final status retrieved"
    );

    if (finalStatus.status === "Succeeded") {
      console.log("");
      await runHooks("post-deploy", postHooks, false);
      console.log("");
    } else {
      log.warn(
        `Skipping post-deploy hooks because execution finished with status '${finalStatus.status}'.`
      );
    }
  }
}

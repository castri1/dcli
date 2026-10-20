// Node.js version check
const nodeVersion = parseInt(process.versions.node.split(".")[0], 10);
if (nodeVersion < 18) {
  console.error(
    `deploy-cli requires Node.js >= 18. You are running Node.js ${process.versions.node}.`
  );
  process.exit(1);
}

import { Command } from "commander";
import { loginCommand } from "./commands/login.js";
import { deployCommand } from "./commands/deploy.js";
import { watchCommand } from "./commands/watch.js";
import { statusCommand } from "./commands/status.js";
import { listCommand } from "./commands/list.js";
import { addCommand } from "./commands/add.js";
import { removeCommand } from "./commands/remove.js";
import { approveCommand } from "./commands/approve.js";
import { historyCommand } from "./commands/history.js";
import { logsCommand } from "./commands/logs.js";
import { stopCommand } from "./commands/stop.js";
import { initCommand } from "./commands/init.js";
import { configCommand } from "./commands/config.js";
import { syncCommand } from "./commands/sync.js";
import { interactiveMenu } from "./commands/menu.js";
import { isUserCancelled } from "./errors.js";
import chalk from "chalk";
import * as log from "./ui/logger.js";

/**
 * Wrap a command action so that errors are handled consistently.
 * - User cancellations (Ctrl+C) print "Cancelled." and exit 0.
 * - DeployCliError and other errors print the message and exit 1.
 * - Errors already displayed by withSpinner are not shown again.
 */
function wrapAction<T extends unknown[]>(
  fn: (...args: T) => Promise<void>
): (...args: T) => Promise<void> {
  return async (...args: T) => {
    try {
      await fn(...args);
    } catch (err) {
      if (isUserCancelled(err)) {
        console.log("");
        log.info("Cancelled.");
        process.exit(0);
      }
      log.errorIfNew(err);
      process.exit(1);
    }
  };
}

const program = new Command();

program
  .name("deploy-cli")
  .description(
    "CLI tool to launch and monitor AWS CodePipeline deployments across multiple environments"
  )
  .version("1.0.0")
  .option("--no-color", "Disable colored output")
  .option("--quiet", "Suppress informational output")
  .hook("preAction", () => {
    const opts = program.opts();
    if (opts.color === false) {
      chalk.level = 0;
    }
  })
  .action(wrapAction(async () => {
    await interactiveMenu();
  }));

program
  .command("init")
  .description("Set up deploy-cli with your first environment and pipeline")
  .action(wrapAction(async () => {
    await initCommand();
  }));

program
  .command("login")
  .description("Run AWS SSO login for an environment")
  .argument("[env]", "Environment name (e.g., dev, qa, prod)")
  .action(wrapAction(async (env?: string) => {
    await loginCommand(env);
  }));

program
  .command("deploy")
  .description("Start a pipeline execution and monitor its progress")
  .argument("[env]", "Environment name (e.g., dev, qa, prod)")
  .argument("[pipeline]", "Pipeline name (optional if only one pipeline in env)")
  .option("--skip-hooks", "Skip pre-deploy and post-deploy hooks")
  .action(wrapAction(async (env?: string, pipeline?: string, opts?: { skipHooks?: boolean }) => {
    await deployCommand(env, pipeline, opts);
  }));

program
  .command("watch")
  .description("Show live status of all tracked pipeline executions")
  .argument("[env]", "Environment name (optional)")
  .argument("[pipeline]", "Pipeline name (optional)")
  .action(wrapAction(async (env?: string, pipeline?: string) => {
    await watchCommand(env, pipeline);
  }));

program
  .command("status")
  .description("Show detailed status of a specific pipeline execution")
  .argument("[env]", "Environment name (e.g., dev, qa, prod)")
  .argument("[pipeline]", "Pipeline name (optional if only one pipeline in env)")
  .option("--json", "Output machine-readable JSON")
  .action(wrapAction(async (env?: string, pipeline?: string, opts?: { json?: boolean }) => {
    await statusCommand(env, pipeline, opts);
  }));

program
  .command("list")
  .description("List all configured environments and pipelines")
  .option("--json", "Output machine-readable JSON")
  .action(wrapAction(async (opts?: { json?: boolean }) => {
    await listCommand(opts);
  }));

program
  .command("add")
  .description("Add a new pipeline or environment to the config interactively")
  .action(wrapAction(async () => {
    await addCommand();
  }));

program
  .command("remove")
  .description("Remove a pipeline or environment from the config")
  .action(wrapAction(async () => {
    await removeCommand();
  }));

program
  .command("sync")
  .description("Import missing pipelines from AWS into the config")
  .argument("[env]", "Environment name (e.g., dev, qa, prod)")
  .option("--all", "Import all missing pipelines without prompting")
  .action(wrapAction(async (env?: string, opts?: { all?: boolean }) => {
    await syncCommand(env, opts);
  }));

program
  .command("approve")
  .description("Approve or reject a pending manual approval action")
  .argument("[env]", "Environment name (e.g., dev, qa, prod)")
  .argument("[pipeline]", "Pipeline name (optional if only one pipeline in env)")
  .action(wrapAction(async (env?: string, pipeline?: string) => {
    await approveCommand(env, pipeline);
  }));

program
  .command("history")
  .description("Show recent deployment history for a pipeline")
  .argument("[env]", "Environment name (e.g., dev, qa, prod)")
  .argument("[pipeline]", "Pipeline name (optional if only one pipeline in env)")
  .option("-n, --limit <number>", "Number of executions to fetch", "10")
  .option("--status <status>", "Filter results by status")
  .option(
    "--details [executionId]",
    "Fetch stage-level details for the latest or a specific execution"
  )
  .option("--json", "Output machine-readable JSON")
  .action(wrapAction(async (
    env?: string,
    pipeline?: string,
    opts?: {
      json?: boolean;
      limit?: string;
      status?: string;
      details?: string | boolean;
    }
  ) => {
    await historyCommand(env, pipeline, opts);
  }));

program
  .command("logs")
  .description("Stream CodeBuild logs for a pipeline execution")
  .argument("[env]", "Environment name (e.g., dev, qa, prod)")
  .argument("[pipeline]", "Pipeline name (optional if only one pipeline in env)")
  .action(wrapAction(async (env?: string, pipeline?: string) => {
    await logsCommand(env, pipeline);
  }));

program
  .command("stop")
  .description("Stop a running pipeline execution")
  .argument("[env]", "Environment name (e.g., dev, qa, prod)")
  .argument("[pipeline]", "Pipeline name (optional if only one pipeline in env)")
  .action(wrapAction(async (env?: string, pipeline?: string) => {
    await stopCommand(env, pipeline);
  }));

program
  .command("config")
  .description("Show or edit the active config file")
  .option("--edit", "Open the interactive config editor")
  .option("--json", "Output machine-readable JSON")
  .action(wrapAction(async (opts?: { edit?: boolean; json?: boolean }) => {
    await configCommand(opts);
  }));

program.parse();

import { select, Separator } from "@inquirer/prompts";
import chalk from "chalk";
import { renderBanner } from "../ui/banner.js";
import { deployCommand } from "./deploy.js";
import { watchCommand } from "./watch.js";
import { statusCommand } from "./status.js";
import { listCommand } from "./list.js";
import { addCommand } from "./add.js";
import { removeCommand } from "./remove.js";
import { loginCommand } from "./login.js";
import { approveCommand } from "./approve.js";
import { historyCommand } from "./history.js";
import { logsCommand } from "./logs.js";
import { stopCommand } from "./stop.js";
import { initCommand } from "./init.js";
import { configCommand } from "./config.js";
import { syncCommand } from "./sync.js";
import { isUserCancelled } from "../errors.js";
import { resolveConfigPath } from "../config/loader.js";
import { getActiveExecutions } from "../state/manager.js";
import * as log from "../ui/logger.js";

type MenuAction =
  | "deploy"
  | "watch"
  | "status"
  | "approve"
  | "history"
  | "logs"
  | "stop"
  | "list"
  | "add"
  | "remove"
  | "sync"
  | "config"
  | "login"
  | "exit";

export async function interactiveMenu(): Promise<void> {
  console.log(renderBanner());
  console.log("");

  // Check if config exists — offer init if not
  if (!resolveConfigPath()) {
    log.info("No configuration found. Let's set up your first environment.");
    console.log("");
    try {
      await initCommand();
    } catch (err) {
      if (isUserCancelled(err)) {
        log.dim("Goodbye!");
        return;
      }
      throw err;
    }
  }

  while (true) {
    try {
      // Check for active deployments to show context in menu
      let activeCount = 0;
      try {
        const active = await getActiveExecutions();
        activeCount = active.length;
      } catch {
        // Ignore — config may not exist yet
      }

      const watchLabel = activeCount > 0
        ? `  ${chalk.yellow("◉")}  Watch active deployments ${chalk.yellow(`(${activeCount} active)`)}`
        : `  ${chalk.yellow("◉")}  Watch active deployments`;

      const action = await select<MenuAction>({
        message: chalk.bold("What would you like to do?"),
        choices: [
          new Separator(chalk.cyan.bold("  ── Deployment ──────────────────")),
          {
            name: `  ${chalk.green("▶")}  Deploy a pipeline`,
            value: "deploy" as MenuAction,
            description: "Start a CodePipeline execution and monitor it",
          },
          {
            name: watchLabel,
            value: "watch" as MenuAction,
            description: "Live view of all tracked pipeline executions",
          },
          {
            name: `  ${chalk.blue("◈")}  Check pipeline status`,
            value: "status" as MenuAction,
            description: "Detailed status of a specific pipeline",
          },
          {
            name: `  ${chalk.yellow("⏳")}  Approve/Reject pending approval`,
            value: "approve" as MenuAction,
            description: "Handle manual approval gates",
          },
          {
            name: `  ${chalk.gray("⧖")}  Deployment history`,
            value: "history" as MenuAction,
            description: "View recent executions for a pipeline",
          },
          {
            name: `  ${chalk.white("⎙")}  Stream build logs`,
            value: "logs" as MenuAction,
            description: "Stream CodeBuild logs in real time",
          },
          {
            name: `  ${chalk.red("■")}  Stop a running pipeline`,
            value: "stop" as MenuAction,
            description: "Gracefully stop or abandon an execution",
          },
          new Separator(chalk.cyan.bold("  ── Configuration ───────────────")),
          {
            name: `  ${chalk.white("☰")}  List environments & pipelines`,
            value: "list" as MenuAction,
            description: "Show all configured environments",
          },
          {
            name: `  ${chalk.green("+")}  Add a pipeline or environment`,
            value: "add" as MenuAction,
            description: "Configure a new pipeline or environment",
          },
          {
            name: `  ${chalk.red("−")}  Remove a pipeline or environment`,
            value: "remove" as MenuAction,
            description: "Remove from configuration",
          },
          {
            name: `  ${chalk.cyan("⇅")}  Sync pipelines from AWS`,
            value: "sync" as MenuAction,
            description: "Import missing pipelines into config",
          },
          {
            name: `  ${chalk.blue("⚙")}  Edit config settings`,
            value: "config" as MenuAction,
            description: "Manage defaults, profiles, and hooks",
          },
          new Separator(chalk.cyan.bold("  ── Authentication ──────────────")),
          {
            name: `  ${chalk.magenta("⚷")}  Login to an environment`,
            value: "login" as MenuAction,
            description: "Run AWS SSO login for an environment",
          },
          new Separator(chalk.gray("  ─────────────────────────────────")),
          {
            name: `  ${chalk.gray("✕")}  Exit`,
            value: "exit" as MenuAction,
          },
        ],
      });

      if (action === "exit") {
        console.log("");
        log.dim("Goodbye!");
        console.log("");
        break;
      }

      console.log("");

      switch (action) {
        case "deploy":
          await deployCommand();
          break;
        case "watch":
          await watchCommand();
          break;
        case "status":
          await statusCommand();
          break;
        case "approve":
          await approveCommand();
          break;
        case "history":
          await historyCommand();
          break;
        case "logs":
          await logsCommand();
          break;
        case "stop":
          await stopCommand();
          break;
        case "list":
          await listCommand();
          break;
        case "add":
          await addCommand();
          break;
        case "remove":
          await removeCommand();
          break;
        case "sync":
          await syncCommand();
          break;
        case "config":
          await configCommand({ edit: true });
          break;
        case "login":
          await loginCommand();
          break;
      }

      // After command completes, show a separator before the next menu loop
      console.log("");
      console.log(chalk.gray("  ─────────────────────────────────────────────"));
      console.log("");
    } catch (err) {
      // Handle user cancellations (Ctrl+C in prompts)
      if (isUserCancelled(err)) {
        console.log("");
        // If the main menu select was cancelled, exit
        // Otherwise just return to menu
        continue;
      }

      // Display the error (skip if already shown by withSpinner)
      log.errorIfNew(err);
      console.log("");
    }
  }
}

import chalk from "chalk";
import gradient from "gradient-string";
import Table from "cli-table3";
import { loadConfigWithMeta } from "../config/loader.js";
import { printJson } from "./shared.js";
import * as log from "../ui/logger.js";

const HEADER_GRADIENT = gradient(["#00d2ff", "#3a7bd5"]);

export async function listCommand(options?: { json?: boolean }): Promise<void> {
  const { config, path } = loadConfigWithMeta();
  const envNames = Object.keys(config.environments);

  if (envNames.length === 0) {
    log.info("No environments configured.");
    log.dim("Run: deploy-cli add");
    return;
  }

  if (options?.json) {
    printJson({
      path,
      defaults: config.defaults,
      environments: config.environments,
    });
    return;
  }

  console.log("");
  console.log("  " + HEADER_GRADIENT("Environments & Pipelines"));
  console.log("");

  const table = new Table({
    head: [
      chalk.cyan.bold("Environment"),
      chalk.cyan.bold("Profile"),
      chalk.cyan.bold("Region"),
      chalk.cyan.bold("Pipelines"),
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
    colWidths: [15, 20, 13, 40],
    wordWrap: true,
  });

  for (const envName of envNames) {
    const env = config.environments[envName];
    const pipelineList = env.pipelines
      .map((p) => {
        const desc = p.description
          ? chalk.gray(` - ${p.description}`)
          : "";
        return `${chalk.white(p.name)}${desc}`;
      })
      .join("\n");

    table.push([
      chalk.cyan.bold(envName.toUpperCase()),
      chalk.white(env.profile),
      chalk.white(env.region),
      pipelineList,
    ]);
  }

  console.log(table.toString());
  console.log("");
  console.log(
    chalk.gray(
      `  Poll interval: ${config.defaults.pollIntervalSeconds}s  |  Notifications: ${config.defaults.notifications ? "on" : "off"}  |  ${envNames.length} environment${envNames.length !== 1 ? "s" : ""} configured`
    )
  );
  console.log(chalk.gray(`  Config path: ${path}`));
  console.log("");
}

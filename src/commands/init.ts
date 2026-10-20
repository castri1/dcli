import { input } from "@inquirer/prompts";
import chalk from "chalk";
import boxen from "boxen";
import { resolveConfigPath } from "../config/loader.js";
import { saveConfig, getConfigFilePath } from "../config/writer.js";
import { DEFAULT_CONFIG } from "../config/types.js";
import type { AppConfig } from "../config/types.js";
import * as log from "../ui/logger.js";

export async function initCommand(): Promise<void> {
  const existingConfigPath = resolveConfigPath();
  const configPath = getConfigFilePath(existingConfigPath);

  if (existingConfigPath) {
    log.info(`Config file already exists at ${configPath}`);
    log.dim("Use 'deploy-cli add' to add more pipelines.");
    return;
  }

  console.log("");
  console.log(
    boxen(
      chalk.bold("Welcome to deploy-cli!") +
        "\n\n" +
        chalk.gray("Let's set up your first environment and pipeline.") +
        "\n" +
        chalk.gray("You can always add more later with: deploy-cli add"),
      {
        borderStyle: "round",
        borderColor: "#3a7bd5",
        padding: { left: 2, right: 2, top: 1, bottom: 1 },
      }
    )
  );
  console.log("");

  // Environment
  const envName = await input({
    message: "Environment name (e.g., dev, qa, prod):",
    validate: (val) => {
      const trimmed = val.trim();
      if (!trimmed) return "Environment name is required";
      if (!/^[a-zA-Z0-9_-]+$/.test(trimmed)) {
        return "Use only letters, numbers, hyphens, and underscores";
      }
      return true;
    },
  });

  const profile = await input({
    message: "AWS SSO profile name (from ~/.aws/config):",
    validate: (val) => (val.trim() ? true : "Profile name is required"),
  });

  const region = await input({
    message: "AWS region:",
    default: "us-east-1",
    validate: (val) => (val.trim() ? true : "Region is required"),
  });

  const pipelineName = await input({
    message: "Pipeline name (exact name in AWS CodePipeline):",
    validate: (val) => (val.trim() ? true : "Pipeline name is required"),
  });

  const description = await input({
    message: "Description (optional, press Enter to skip):",
  });

  // Build config
  const config: AppConfig = {
    environments: {
      [envName.trim()]: {
        profile: profile.trim(),
        region: region.trim(),
        pipelines: [
          {
            name: pipelineName.trim(),
            description: description.trim() || undefined,
          },
        ],
      },
    },
    defaults: { ...DEFAULT_CONFIG },
  };

  saveConfig(config, configPath);

  console.log("");
  log.success(`Config created at ${configPath}`);
  log.dim(`Environment '${envName.trim()}' with pipeline '${pipelineName.trim()}'`);
  console.log("");
  log.info("Next steps:");
  log.dim(`  1. Login:  deploy-cli login ${envName.trim()}`);
  log.dim(`  2. Deploy: deploy-cli deploy ${envName.trim()} ${pipelineName.trim()}`);
  log.dim(`  3. Or just run: deploy-cli`);
  console.log("");
}

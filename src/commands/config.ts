import { confirm, input, select } from "@inquirer/prompts";
import chalk from "chalk";
import { loadConfigWithMeta, resolveConfigPath } from "../config/loader.js";
import { getConfigFilePath, saveConfig } from "../config/writer.js";
import { DEFAULT_CONFIG } from "../config/types.js";
import type { AppConfig, HookConfig, PipelineConfig } from "../config/types.js";
import { printJson } from "./shared.js";
import * as log from "../ui/logger.js";

function loadEditableConfig(): { config: AppConfig; path: string } {
  const existingPath = resolveConfigPath();
  if (existingPath) {
    return loadConfigWithMeta();
  }

  return {
    config: {
      environments: {},
      defaults: { ...DEFAULT_CONFIG },
    },
    path: getConfigFilePath(),
  };
}

function pipelineLabel(pipeline: PipelineConfig): string {
  return pipeline.description
    ? `${pipeline.name} — ${pipeline.description}`
    : pipeline.name;
}

async function editDefaults(config: AppConfig): Promise<void> {
  const pollInterval = await input({
    message: "Poll interval in seconds:",
    default: String(config.defaults.pollIntervalSeconds),
    validate: (value) => {
      const parsed = Number.parseInt(value, 10);
      return Number.isFinite(parsed) && parsed > 0
        ? true
        : "Enter a positive integer";
    },
  });

  const notifications = await confirm({
    message: "Enable desktop notifications?",
    default: config.defaults.notifications,
  });

  config.defaults.pollIntervalSeconds = Number.parseInt(pollInterval, 10);
  config.defaults.notifications = notifications;
}

async function editEnvironment(config: AppConfig): Promise<void> {
  const envNames = Object.keys(config.environments);
  if (envNames.length === 0) {
    log.info("No environments configured yet. Use 'deploy-cli add' first.");
    return;
  }

  const envName = await select({
    message: "Select an environment to edit:",
    choices: envNames.map((name) => ({
      name: `${name.toUpperCase()} (profile: ${config.environments[name].profile}, region: ${config.environments[name].region})`,
      value: name,
    })),
  });

  const env = config.environments[envName];
  const field = await select({
    message: `What do you want to edit in ${envName.toUpperCase()}?`,
    choices: [
      { name: "AWS profile", value: "profile" },
      { name: "AWS region", value: "region" },
    ],
  });

  const nextValue = await input({
    message: field === "profile" ? "AWS profile:" : "AWS region:",
    default: field === "profile" ? env.profile : env.region,
    validate: (value) => (value.trim() ? true : "Value is required"),
  });

  if (field === "profile") {
    env.profile = nextValue.trim();
  } else {
    env.region = nextValue.trim();
  }
}

async function editHookList(
  pipeline: PipelineConfig,
  phase: "pre-deploy" | "post-deploy"
): Promise<void> {
  pipeline.hooks ||= {};
  pipeline.hooks[phase] ||= [];

  while (true) {
    const hooks = pipeline.hooks[phase] || [];
    const action = await select({
      message: `${phase} hooks for ${pipeline.name}`,
      choices: [
        {
          name: `Add hook (${hooks.length} configured)`,
          value: "add",
        },
        {
          name: hooks.length > 0 ? "Remove hook" : "Remove hook (none)",
          value: "remove",
          disabled: hooks.length === 0,
        },
        {
          name: hooks.length > 0 ? "Clear all hooks" : "Clear all hooks (none)",
          value: "clear",
          disabled: hooks.length === 0,
        },
        {
          name: "Back",
          value: "back",
        },
      ],
    });

    if (action === "back") {
      if (pipeline.hooks[phase]?.length === 0) {
        delete pipeline.hooks[phase];
      }
      if (Object.keys(pipeline.hooks).length === 0) {
        delete pipeline.hooks;
      }
      return;
    }

    if (action === "add") {
      const command = await input({
        message: "Hook command:",
        validate: (value) => (value.trim() ? true : "Command is required"),
      });
      const description = await input({
        message: "Description (optional):",
      });
      pipeline.hooks[phase]!.push({
        command: command.trim(),
        description: description.trim() || undefined,
      });
      continue;
    }

    if (action === "remove") {
      const hookIndex = await select({
        message: "Select a hook to remove:",
        choices: hooks.map((hook, index) => ({
          name: hook.description
            ? `${hook.description} — ${hook.command}`
            : hook.command,
          value: String(index),
        })),
      });
      hooks.splice(Number.parseInt(hookIndex, 10), 1);
      continue;
    }

    const confirmed = await confirm({
      message: `Remove all ${phase} hooks from ${pipeline.name}?`,
      default: false,
    });
    if (confirmed) {
      delete pipeline.hooks[phase];
    }
  }
}

async function editPipeline(config: AppConfig): Promise<void> {
  const envNames = Object.keys(config.environments).filter(
    (name) => config.environments[name].pipelines.length > 0
  );
  if (envNames.length === 0) {
    log.info("No pipelines configured yet. Use 'deploy-cli add' first.");
    return;
  }

  const envName = await select({
    message: "Select the pipeline environment:",
    choices: envNames.map((name) => ({ name: name.toUpperCase(), value: name })),
  });
  const env = config.environments[envName];
  const pipelineName = await select({
    message: `Select a pipeline in ${envName.toUpperCase()}:`,
    choices: env.pipelines.map((pipeline) => ({
      name: pipelineLabel(pipeline),
      value: pipeline.name,
    })),
  });
  const pipeline = env.pipelines.find((entry) => entry.name === pipelineName)!;

  const action = await select({
    message: `What do you want to edit for ${pipeline.name}?`,
    choices: [
      { name: "Description", value: "description" },
      { name: "Pre-deploy hooks", value: "pre-deploy" },
      { name: "Post-deploy hooks", value: "post-deploy" },
    ],
  });

  if (action === "description") {
    const description = await input({
      message: "Pipeline description (leave blank to clear):",
      default: pipeline.description || "",
    });
    pipeline.description = description.trim() || undefined;
    return;
  }

  await editHookList(pipeline, action as "pre-deploy" | "post-deploy");
}

function showConfigSummary(config: AppConfig, path: string): void {
  console.log("");
  console.log(chalk.bold("  Config file path:"));
  console.log(`  ${chalk.cyan(path)}`);
  console.log("");
  console.log(chalk.bold("  Defaults:"));
  console.log(
    chalk.gray(
      `  pollIntervalSeconds=${config.defaults.pollIntervalSeconds}, notifications=${config.defaults.notifications}`
    )
  );
  console.log("");

  const envNames = Object.keys(config.environments);
  if (envNames.length === 0) {
    log.info("No environments configured yet.");
    return;
  }

  console.log(chalk.bold("  Environments:"));
  for (const envName of envNames) {
    const env = config.environments[envName];
    console.log(
      `  ${chalk.cyan(envName.toUpperCase())} ${chalk.gray(`profile=${env.profile}, region=${env.region}`)}`
    );
    for (const pipeline of env.pipelines) {
      const preHooks = pipeline.hooks?.["pre-deploy"]?.length || 0;
      const postHooks = pipeline.hooks?.["post-deploy"]?.length || 0;
      console.log(
        `    - ${pipeline.name}${pipeline.description ? chalk.gray(` — ${pipeline.description}`) : ""}${chalk.gray(` [pre:${preHooks} post:${postHooks}]`)}`
      );
    }
  }
  console.log("");
}

export async function configCommand(options?: {
  edit?: boolean;
  json?: boolean;
}): Promise<void> {
  const { config, path } = loadEditableConfig();

  if (options?.json) {
    printJson({ path, config });
    return;
  }

  if (!options?.edit) {
    showConfigSummary(config, path);
    return;
  }

  while (true) {
    const action = await select({
      message: "Config editor",
      choices: [
        { name: "Edit defaults", value: "defaults" },
        { name: "Edit environment profile/region", value: "environment" },
        { name: "Edit pipeline description/hooks", value: "pipeline" },
        { name: "Show summary", value: "summary" },
        { name: "Done", value: "done" },
      ],
    });

    if (action === "done") {
      console.log("");
      log.success(`Config saved to ${path}`);
      console.log("");
      return;
    }

    if (action === "summary") {
      showConfigSummary(config, path);
      continue;
    }

    if (action === "defaults") {
      await editDefaults(config);
    } else if (action === "environment") {
      await editEnvironment(config);
    } else {
      await editPipeline(config);
    }

    saveConfig(config, path);
    log.success(`Saved changes to ${path}`);
    console.log("");
  }
}

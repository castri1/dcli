import { select, input, search } from "@inquirer/prompts";
import { existsSync } from "node:fs";
import { loadConfigWithMeta, resolveConfigPath } from "../config/loader.js";
import { saveConfig, getConfigFilePath } from "../config/writer.js";
import { DEFAULT_CONFIG } from "../config/types.js";
import type { AppConfig, EnvironmentConfig } from "../config/types.js";
import { validateCredentials } from "../aws/credentials.js";
import { listPipelines } from "../aws/pipeline.js";
import * as log from "../ui/logger.js";

const NEW_ENV_VALUE = "__new_environment__";

/**
 * Load existing config or create a blank one if no config file exists.
 */
function loadOrCreateConfig(): AppConfig {
  const configPath = resolveConfigPath();
  if (configPath && existsSync(configPath)) {
    try {
      return loadConfigWithMeta().config;
    } catch {
      // Config exists but is invalid — start fresh
    }
  }
  return { environments: {}, defaults: { ...DEFAULT_CONFIG } };
}

/**
 * Try to fetch pipelines from AWS and let the user pick one with search.
 * Falls back to manual input if credentials are invalid or fetch fails.
 */
async function selectPipelineFromAws(
  env: EnvironmentConfig,
  envName: string
): Promise<string | null> {
  const credentialsValid = await log.withSpinner(
    `Checking credentials for ${envName} (profile: ${env.profile})`,
    () => validateCredentials(env.profile),
    "Credentials valid"
  );

  if (!credentialsValid) {
    log.warn(
      `SSO session expired or not found for profile '${env.profile}'. Falling back to manual entry.`
    );
    log.dim(`Run: deploy-cli login ${envName}`);
    return null;
  }

  const allPipelines = await log.withSpinner(
    `Fetching pipelines from AWS (${env.region})`,
    () => listPipelines(env.profile, env.region),
    "Pipelines fetched"
  );

  if (allPipelines.length === 0) {
    log.warn("No pipelines found in this AWS account/region.");
    return null;
  }

  // Filter out pipelines already configured in this environment
  const existingNames = new Set(env.pipelines.map((p) => p.name));
  const available = allPipelines.filter((name) => !existingNames.has(name));

  if (available.length === 0) {
    log.info("All pipelines in this account are already configured.");
    return null;
  }

  log.success(`Found ${allPipelines.length} pipeline${allPipelines.length > 1 ? "s" : ""} (${available.length} not yet configured)`);

  const selected = await search({
    message: `Select a pipeline (type to search, ${available.length} available):`,
    source: async (term) => {
      const filtered = term
        ? available.filter((name) =>
            name.toLowerCase().includes(term.toLowerCase())
          )
        : available;

      return filtered.map((name) => ({
        name,
        value: name,
      }));
    },
  });

  return selected;
}

/**
 * Prompt the user to type a pipeline name manually.
 */
async function inputPipelineManually(
  env: EnvironmentConfig,
  envName: string
): Promise<string> {
  const pipelineName = await input({
    message: "Pipeline name (exact name in AWS CodePipeline):",
    validate: (val) => {
      const trimmed = val.trim();
      if (!trimmed) return "Pipeline name is required";
      if (env.pipelines.some((p) => p.name === trimmed)) {
        return `Pipeline '${trimmed}' already exists in ${envName}`;
      }
      return true;
    },
  });
  return pipelineName.trim();
}

export async function addCommand(): Promise<void> {
    const currentConfigPath = resolveConfigPath();
    const config = loadOrCreateConfig();
    const envNames = Object.keys(config.environments);

    // Step 1: Select or create an environment
    const choices = [
      ...envNames.map((name) => ({
        name: `${name.toUpperCase()} (profile: ${config.environments[name].profile})`,
        value: name,
      })),
      {
        name: "+ Create new environment",
        value: NEW_ENV_VALUE,
      },
    ];

    const selectedEnv = await select({
      message: "Select an environment or create a new one:",
      choices,
    });

    let envName: string;
    let env: EnvironmentConfig;

    if (selectedEnv === NEW_ENV_VALUE) {
      // Create new environment
      envName = await input({
        message: "Environment name (e.g., dev, qa, staging, prod):",
        validate: (val) => {
          const trimmed = val.trim();
          if (!trimmed) return "Environment name is required";
          if (config.environments[trimmed]) {
            return `Environment '${trimmed}' already exists`;
          }
          if (!/^[a-zA-Z0-9_-]+$/.test(trimmed)) {
            return "Use only letters, numbers, hyphens, and underscores";
          }
          return true;
        },
      });
      envName = envName.trim();

      const profile = await input({
        message: "AWS SSO profile name:",
        validate: (val) => (val.trim() ? true : "Profile name is required"),
      });

      const region = await input({
        message: "AWS region:",
        default: "us-east-1",
        validate: (val) => (val.trim() ? true : "Region is required"),
      });

      env = {
        profile: profile.trim(),
        region: region.trim(),
        pipelines: [],
      };

      config.environments[envName] = env;
    } else {
      envName = selectedEnv;
      env = config.environments[envName];
    }

    // Step 2: Choose how to specify the pipeline
    const method = await select({
      message: "How do you want to specify the pipeline?",
      choices: [
        {
          name: "Browse pipelines from AWS account",
          value: "browse",
        },
        {
          name: "Enter name manually",
          value: "manual",
        },
      ],
    });

    let pipelineName: string;

    if (method === "browse") {
      try {
        const selected = await selectPipelineFromAws(env, envName);
        if (selected) {
          pipelineName = selected;
        } else {
          // Fallback to manual
          pipelineName = await inputPipelineManually(env, envName);
        }
      } catch (err) {
        // If AWS call fails, fall back to manual
        const message = err instanceof Error ? err.message : String(err);
        log.warn(`Could not fetch pipelines: ${message}`);
        log.info("Falling back to manual entry.");
        pipelineName = await inputPipelineManually(env, envName);
      }
    } else {
      pipelineName = await inputPipelineManually(env, envName);
    }

    // Step 3: Optional description
    const description = await input({
      message: "Description (optional, press Enter to skip):",
    });

    // Step 4: Add pipeline to the environment
    env.pipelines.push({
      name: pipelineName,
      description: description.trim() || undefined,
    });

    // Step 5: Save config
    saveConfig(config, currentConfigPath);

    console.log("");
    if (selectedEnv === NEW_ENV_VALUE) {
      log.success(
        `Created environment '${envName}' with pipeline '${pipelineName}'`
      );
    } else {
      log.success(
        `Added pipeline '${pipelineName}' to environment '${envName}'`
      );
    }
    log.dim(`Config saved to ${getConfigFilePath(currentConfigPath)}`);
    console.log("");
}

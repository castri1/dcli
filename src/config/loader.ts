import { readFileSync, existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { parse } from "yaml";
import type { AppConfig, EnvironmentConfig, PipelineConfig, PipelineHooks, HookConfig } from "./types.js";
import { DEFAULT_CONFIG } from "./types.js";

const CONFIG_DIR = join(homedir(), ".deployment-cli");
const CONFIG_FILE = "config.yaml";

export interface LoadedConfig {
  config: AppConfig;
  path: string;
}

export function getPrimaryConfigPath(): string {
  return join(CONFIG_DIR, CONFIG_FILE);
}

export function getConfigPaths(): string[] {
  return [
    getPrimaryConfigPath(),
    join(process.cwd(), `deployment-cli.yaml`),
    join(process.cwd(), `.deployment-cli.yaml`),
  ];
}

export function resolveConfigPath(): string | undefined {
  return getConfigPaths().find((path) => existsSync(path));
}

export function getConfigDir(): string {
  return CONFIG_DIR;
}

export function loadConfig(): AppConfig {
  return loadConfigWithMeta().config;
}

export function loadConfigWithMeta(): LoadedConfig {
  const configPath = resolveConfigPath();

  if (!configPath) {
    throw new Error(
      `No config file found.\n\n  Run: deploy-cli init    (first-time setup)\n  Or:  deploy-cli add     (add environments and pipelines interactively)`
    );
  }

  const raw = readFileSync(configPath, "utf-8");
  const parsed = parse(raw);

  return {
    config: validateConfig(parsed, configPath),
    path: configPath,
  };
}

function validateConfig(raw: unknown, path: string): AppConfig {
  if (!raw || typeof raw !== "object") {
    throw new Error(`Invalid config file at ${path}: expected an object`);
  }

  const obj = raw as Record<string, unknown>;

  if (!obj.environments || typeof obj.environments !== "object") {
    throw new Error(
      `Invalid config at ${path}: missing 'environments' section`
    );
  }

  const environments: Record<string, EnvironmentConfig> = {};

  for (const [envName, envRaw] of Object.entries(
    obj.environments as Record<string, unknown>
  )) {
    if (!envRaw || typeof envRaw !== "object") {
      throw new Error(`Invalid environment '${envName}' in config`);
    }

    const env = envRaw as Record<string, unknown>;

    if (typeof env.profile !== "string" || !env.profile) {
      throw new Error(
        `Environment '${envName}' is missing required 'profile' field`
      );
    }

    if (typeof env.region !== "string" || !env.region) {
      throw new Error(
        `Environment '${envName}' is missing required 'region' field`
      );
    }

    if (!Array.isArray(env.pipelines) || env.pipelines.length === 0) {
      throw new Error(
        `Environment '${envName}' must have at least one pipeline`
      );
    }

    const pipelines: PipelineConfig[] = env.pipelines.map(
      (p: unknown, i: number) => {
        if (!p || typeof p !== "object") {
          throw new Error(
            `Pipeline ${i} in environment '${envName}' is invalid`
          );
        }
        const pipeline = p as Record<string, unknown>;
        if (typeof pipeline.name !== "string" || !pipeline.name) {
          throw new Error(
            `Pipeline ${i} in environment '${envName}' is missing 'name'`
          );
        }
        // Parse hooks
        let hooks: PipelineHooks | undefined;
        if (pipeline.hooks && typeof pipeline.hooks === "object") {
          const rawHooks = pipeline.hooks as Record<string, unknown>;
          hooks = {};
          for (const phase of ["pre-deploy", "post-deploy"] as const) {
            if (Array.isArray(rawHooks[phase])) {
              hooks[phase] = (rawHooks[phase] as unknown[]).map(
                (h: unknown) => {
                  if (typeof h === "string") {
                    return { command: h };
                  }
                  if (h && typeof h === "object") {
                    const hook = h as Record<string, unknown>;
                    return {
                      command: String(hook.command || ""),
                      description:
                        typeof hook.description === "string"
                          ? hook.description
                          : undefined,
                    } as HookConfig;
                  }
                  return { command: "" };
                }
              ).filter((h) => h.command);
            }
          }
        }

        return {
          name: pipeline.name,
          description:
            typeof pipeline.description === "string"
              ? pipeline.description
              : undefined,
          hooks,
        };
      }
    );

    environments[envName] = {
      profile: env.profile,
      region: env.region,
      pipelines,
    };
  }

  const defaults = { ...DEFAULT_CONFIG };
  if (obj.defaults && typeof obj.defaults === "object") {
    const d = obj.defaults as Record<string, unknown>;
    if (typeof d.pollIntervalSeconds === "number" && d.pollIntervalSeconds > 0) {
      defaults.pollIntervalSeconds = d.pollIntervalSeconds;
    }
    if (typeof d.notifications === "boolean") {
      defaults.notifications = d.notifications;
    }
  }

  return { environments, defaults };
}

export function getEnvironment(
  config: AppConfig,
  envName: string
): EnvironmentConfig {
  const env = config.environments[envName];
  if (!env) {
    const available = Object.keys(config.environments).join(", ");
    throw new Error(
      `Unknown environment '${envName}'. Available: ${available}`
    );
  }
  return env;
}

export function getPipeline(
  env: EnvironmentConfig,
  envName: string,
  pipelineName?: string
): PipelineConfig {
  if (!pipelineName) {
    if (env.pipelines.length === 1) {
      return env.pipelines[0];
    }
    const names = env.pipelines.map((p) => p.name).join(", ");
    throw new Error(
      `Environment '${envName}' has multiple pipelines. Specify one: ${names}`
    );
  }

  const pipeline = env.pipelines.find((p) => p.name === pipelineName);
  if (!pipeline) {
    const names = env.pipelines.map((p) => p.name).join(", ");
    throw new Error(
      `Pipeline '${pipelineName}' not found in environment '${envName}'. Available: ${names}`
    );
  }

  return pipeline;
}

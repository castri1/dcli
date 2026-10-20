import { writeFileSync, existsSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { stringify } from "yaml";
import { getPrimaryConfigPath } from "./loader.js";
import type { AppConfig } from "./types.js";

/**
 * Get the primary config file path (~/.deployment-cli/config.yaml).
 */
export function getConfigFilePath(filePath?: string): string {
  return filePath || getPrimaryConfigPath();
}

/**
 * Save the config to the primary config file path.
 * Creates the directory if it doesn't exist.
 */
export function saveConfig(config: AppConfig, filePath?: string): void {
  const targetPath = getConfigFilePath(filePath);
  const dir = dirname(targetPath);
  if (!existsSync(dir)) {
    mkdirSync(dir, { recursive: true });
  }

  // Build the output object, omitting undefined description fields
  const output: Record<string, unknown> = {
    environments: Object.fromEntries(
      Object.entries(config.environments).map(([envName, env]) => [
        envName,
        {
          profile: env.profile,
          region: env.region,
          pipelines: env.pipelines.map((p) => {
            const pipeline: Record<string, unknown> = { name: p.name };
            if (p.description) {
              pipeline.description = p.description;
            }
            if (p.hooks) {
              pipeline.hooks = p.hooks;
            }
            return pipeline;
          }),
        },
      ])
    ),
    defaults: config.defaults,
  };

  const yamlStr = stringify(output, { lineWidth: 120 });

  writeFileSync(targetPath, yamlStr, "utf-8");
}

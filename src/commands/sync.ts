import { checkbox } from "@inquirer/prompts";
import { loadConfigWithMeta, getEnvironment } from "../config/loader.js";
import { getConfigFilePath, saveConfig } from "../config/writer.js";
import { listPipelines } from "../aws/pipeline.js";
import { ensureCredentials } from "./shared.js";
import { selectEnvironment } from "../ui/prompts.js";
import * as log from "../ui/logger.js";

export async function syncCommand(
  envName?: string,
  options?: { all?: boolean }
): Promise<void> {
  const { config, path } = loadConfigWithMeta();

  const resolved = envName
    ? { envName, env: getEnvironment(config, envName) }
    : await selectEnvironment(config);

  await ensureCredentials(resolved.envName, resolved.env.profile);

  const remotePipelines = await log.withSpinner(
    `Fetching pipelines from AWS (${resolved.env.region})`,
    () => listPipelines(resolved.env.profile, resolved.env.region),
    "Pipelines fetched"
  );

  const configuredNames = new Set(resolved.env.pipelines.map((pipeline) => pipeline.name));
  const missing = remotePipelines.filter((name) => !configuredNames.has(name));

  if (missing.length === 0) {
    log.info(`No new pipelines found for ${resolved.envName}.`);
    return;
  }

  const selected = options?.all
    ? missing
    : await checkbox({
        message: `Select pipelines to import into ${resolved.envName.toUpperCase()}:`,
        choices: missing.map((name) => ({ name, value: name, checked: true })),
      });

  if (selected.length === 0) {
    log.info("No pipelines selected.");
    return;
  }

  for (const pipelineName of selected) {
    resolved.env.pipelines.push({ name: pipelineName });
  }

  resolved.env.pipelines.sort((a, b) => a.name.localeCompare(b.name));
  saveConfig(config, path);

  console.log("");
  log.success(
    `Imported ${selected.length} pipeline${selected.length !== 1 ? "s" : ""} into ${resolved.envName}`
  );
  log.dim(`Config saved to ${getConfigFilePath(path)}`);
  console.log("");
}

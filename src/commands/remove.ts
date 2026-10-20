import { select, confirm } from "@inquirer/prompts";
import { loadConfigWithMeta } from "../config/loader.js";
import { saveConfig, getConfigFilePath } from "../config/writer.js";
import * as log from "../ui/logger.js";

export async function removeCommand(): Promise<void> {
    const { config, path } = loadConfigWithMeta();
    const envNames = Object.keys(config.environments);

    if (envNames.length === 0) {
      log.info("No environments configured. Nothing to remove.");
      return;
    }

    // Step 1: Select an environment
    const envName = await select({
      message: "Select an environment:",
      choices: envNames.map((name) => {
        const env = config.environments[name];
        const count = env.pipelines.length;
        return {
          name: `${name.toUpperCase()} (profile: ${env.profile}) — ${count} pipeline${count !== 1 ? "s" : ""}`,
          value: name,
        };
      }),
    });

    const env = config.environments[envName];

    // Step 2: What to remove
    const action = await select({
      message: "What do you want to remove?",
      choices: [
        {
          name: `A pipeline from ${envName.toUpperCase()}`,
          value: "pipeline" as const,
        },
        {
          name: `The entire ${envName.toUpperCase()} environment`,
          value: "environment" as const,
        },
      ],
    });

    if (action === "environment") {
      // Remove entire environment
      const count = env.pipelines.length;
      const confirmed = await confirm({
        message: `Are you sure you want to remove the entire ${envName.toUpperCase()} environment (${count} pipeline${count !== 1 ? "s" : ""})?`,
        default: false,
      });

      if (!confirmed) {
        log.info("Cancelled.");
        return;
      }

      delete config.environments[envName];
      saveConfig(config, path);

      console.log("");
      log.success(
        `Removed environment '${envName}' (${count} pipeline${count !== 1 ? "s" : ""})`
      );
      log.dim(`Config saved to ${getConfigFilePath(path)}`);
      console.log("");
    } else {
      // Remove a pipeline
      if (env.pipelines.length === 0) {
        log.info(
          `No pipelines in ${envName.toUpperCase()}. Nothing to remove.`
        );
        return;
      }

      const pipelineName = await select({
        message: "Select the pipeline to remove:",
        choices: env.pipelines.map((p) => ({
          name: p.description ? `${p.name} — ${p.description}` : p.name,
          value: p.name,
        })),
      });

      const confirmed = await confirm({
        message: `Are you sure you want to remove pipeline '${pipelineName}' from ${envName.toUpperCase()}?`,
        default: false,
      });

      if (!confirmed) {
        log.info("Cancelled.");
        return;
      }

      env.pipelines = env.pipelines.filter((p) => p.name !== pipelineName);

      // If no pipelines left, remove the environment too
      if (env.pipelines.length === 0) {
        delete config.environments[envName];
        saveConfig(config, path);

        console.log("");
        log.success(
          `Removed pipeline '${pipelineName}' from ${envName.toUpperCase()}`
        );
        log.info(
          `Environment '${envName}' had no remaining pipelines and was also removed.`
        );
        log.dim(`Config saved to ${getConfigFilePath(path)}`);
        console.log("");
      } else {
        saveConfig(config, path);

        console.log("");
        log.success(
          `Removed pipeline '${pipelineName}' from ${envName.toUpperCase()}`
        );
        log.dim(`Config saved to ${getConfigFilePath(path)}`);
        console.log("");
      }
    }
}

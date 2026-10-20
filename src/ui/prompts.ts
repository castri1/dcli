import { select } from "@inquirer/prompts";
import chalk from "chalk";
import type { AppConfig, EnvironmentConfig, PipelineConfig } from "../config/types.js";

/**
 * Interactive environment picker. Shows all configured environments.
 */
export async function selectEnvironment(
  config: AppConfig
): Promise<{ envName: string; env: EnvironmentConfig }> {
  const envNames = Object.keys(config.environments);

  if (envNames.length === 0) {
    throw new Error(
      "No environments configured. Run: deploy-cli add"
    );
  }

  if (envNames.length === 1) {
    const envName = envNames[0];
    return { envName, env: config.environments[envName] };
  }

  const envName = await select({
    message: "Select an environment:",
    choices: envNames.map((name) => {
      const env = config.environments[name];
      const count = env.pipelines.length;
      return {
        name: `${chalk.cyan.bold(name.toUpperCase().padEnd(8))} ${chalk.gray(`profile: ${env.profile}`)}  ${chalk.gray(`${count} pipeline${count !== 1 ? "s" : ""}`)}`,
        value: name,
      };
    }),
  });

  return { envName, env: config.environments[envName] };
}

/**
 * Interactive pipeline picker. Shows all pipelines for an environment.
 */
export async function selectPipeline(
  env: EnvironmentConfig,
  envName: string
): Promise<PipelineConfig> {
  if (env.pipelines.length === 1) {
    return env.pipelines[0];
  }

  const pipelineName = await select({
    message: `Select a pipeline in ${chalk.cyan(envName.toUpperCase())}:`,
    choices: env.pipelines.map((p) => ({
      name: p.description
        ? `${chalk.white(p.name)} ${chalk.gray("—")} ${chalk.gray(p.description)}`
        : chalk.white(p.name),
      value: p.name,
    })),
  });

  const pipeline = env.pipelines.find((p) => p.name === pipelineName)!;
  return pipeline;
}

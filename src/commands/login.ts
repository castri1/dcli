import { loadConfig, getEnvironment } from "../config/loader.js";
import { ssoLogin } from "../aws/auth.js";
import { selectEnvironment } from "../ui/prompts.js";
import * as log from "../ui/logger.js";

export async function loginCommand(envName?: string): Promise<void> {
  const config = loadConfig();

  let resolvedEnvName: string;
  let profile: string;

  if (envName) {
    const env = getEnvironment(config, envName);
    resolvedEnvName = envName;
    profile = env.profile;
  } else {
    const { envName: selectedEnv, env } = await selectEnvironment(config);
    resolvedEnvName = selectedEnv;
    profile = env.profile;
  }

  log.info(
    `Logging in to ${resolvedEnvName} (profile: ${profile})...`
  );
  ssoLogin(profile);
  log.success(`Successfully logged in to ${resolvedEnvName}`);
}

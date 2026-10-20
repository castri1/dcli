/**
 * Base error class for deploy-cli.
 * Commands should throw these instead of calling process.exit().
 */
export class DeployCliError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "DeployCliError";
  }
}

/**
 * Thrown when AWS SSO credentials are expired or unavailable.
 */
export class CredentialsExpiredError extends DeployCliError {
  public readonly envName: string;
  public readonly profile: string;

  constructor(envName: string, profile: string) {
    super(
      `SSO session expired or not found for profile '${profile}'. Run: deploy-cli login ${envName}`
    );
    this.name = "CredentialsExpiredError";
    this.envName = envName;
    this.profile = profile;
  }
}

/**
 * Thrown when the user cancels an interactive prompt (Ctrl+C).
 */
export class UserCancelledError extends DeployCliError {
  constructor() {
    super("Cancelled.");
    this.name = "UserCancelledError";
  }
}

/**
 * Check if an error is a user cancellation (from inquirer or our own).
 */
export function isUserCancelled(err: unknown): boolean {
  if (err instanceof UserCancelledError) return true;
  if (err && typeof err === "object" && (err as Error).name === "ExitPromptError") return true;
  return false;
}

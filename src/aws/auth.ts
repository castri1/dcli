import { execFileSync } from "node:child_process";

/**
 * Runs `aws sso login --profile <profile>` interactively.
 * This opens the browser for SSO authentication.
 * Uses execFileSync to avoid shell injection via profile names.
 */
export function ssoLogin(profile: string): void {
  try {
    execFileSync("aws", ["sso", "login", "--profile", profile], {
      stdio: "inherit",
    });
  } catch (err) {
    const code = (err as { status?: number })?.status;
    if (code === 127 || (err instanceof Error && err.message.includes("ENOENT"))) {
      throw new Error(
        "AWS CLI not found. Install it from: https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html"
      );
    }
    throw err;
  }
}

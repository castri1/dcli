import { execSync } from "node:child_process";
import chalk from "chalk";
import type { HookConfig } from "../config/types.js";
import * as log from "../ui/logger.js";

export interface HookResult {
  hook: HookConfig;
  success: boolean;
  output?: string;
  error?: string;
  durationMs: number;
}

/**
 * Run a list of hooks sequentially.
 * Returns results for each hook.
 * If a hook fails and `stopOnError` is true, stops immediately.
 */
export async function runHooks(
  phase: "pre-deploy" | "post-deploy",
  hooks: HookConfig[],
  stopOnError: boolean = true
): Promise<{ results: HookResult[]; allPassed: boolean }> {
  if (hooks.length === 0) {
    return { results: [], allPassed: true };
  }

  const phaseLabel = phase === "pre-deploy" ? "Pre-deploy" : "Post-deploy";
  log.info(`Running ${phaseLabel.toLowerCase()} hooks...`);

  const results: HookResult[] = [];
  let allPassed = true;

  for (const hook of hooks) {
    const label = hook.description || hook.command;
    const s = log.spinner(`  ${label}`);
    const start = Date.now();

    try {
      const output = execSync(hook.command, {
        stdio: ["pipe", "pipe", "pipe"],
        timeout: 300000, // 5 minute timeout per hook
        encoding: "utf-8",
      });

      const durationMs = Date.now() - start;
      const durationStr = formatDuration(durationMs);

      s.succeed(`  ${label} ${chalk.gray(`(${durationStr})`)}`);

      results.push({
        hook,
        success: true,
        output: output?.trim() || undefined,
        durationMs,
      });
    } catch (err) {
      const durationMs = Date.now() - start;
      const errorMessage =
        err instanceof Error ? err.message : String(err);

      // Extract stderr from the exec error
      const stderr = (err as { stderr?: string })?.stderr?.trim();
      const displayError = stderr || errorMessage;

      s.fail(`  ${label} ${chalk.red("— Failed")}`);

      if (displayError) {
        console.log(chalk.red(`    ${displayError.split("\n")[0]}`));
      }

      results.push({
        hook,
        success: false,
        error: displayError,
        durationMs,
      });

      allPassed = false;

      if (stopOnError) {
        break;
      }
    }
  }

  if (allPassed) {
    log.success(`All ${phaseLabel.toLowerCase()} hooks passed`);
  } else if (phase === "pre-deploy") {
    log.error(`${phaseLabel} hook failed — aborting deployment`);
  } else {
    log.warn(`Some ${phaseLabel.toLowerCase()} hooks failed`);
  }

  return { results, allPassed };
}

function formatDuration(ms: number): string {
  const seconds = Math.floor(ms / 1000);
  if (seconds < 1) return `${ms}ms`;
  const minutes = Math.floor(seconds / 60);
  if (minutes > 0) {
    return `${minutes}m ${(seconds % 60).toString().padStart(2, "0")}s`;
  }
  return `${seconds}s`;
}

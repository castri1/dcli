import chalk from "chalk";
import ora, { type Ora } from "ora";

export function info(message: string): void {
  console.log(chalk.blue("ℹ") + " " + message);
}

export function success(message: string): void {
  console.log(chalk.green("✓") + " " + message);
}

export function error(message: string): void {
  console.error(chalk.red("✗") + " " + message);
}

export function warn(message: string): void {
  console.log(chalk.yellow("⚠") + " " + message);
}

export function dim(message: string): void {
  console.log(chalk.gray("  " + message));
}

/**
 * Create and start an ora spinner with the given text.
 * Returns the spinner instance so the caller can call .succeed(), .fail(), etc.
 */
export function spinner(message: string): Ora {
  return ora({
    text: message,
    color: "cyan",
    spinner: "dots",
  }).start();
}

/**
 * Run an async function with a spinner.
 * Shows the spinner while the function runs, then succeeds or fails.
 * The error is displayed by the spinner's .fail() — callers should not
 * display it again (the error is re-thrown with a flag to indicate it
 * was already displayed).
 */
export async function withSpinner<T>(
  message: string,
  fn: () => Promise<T>,
  successMessage?: string
): Promise<T> {
  const s = spinner(message);
  try {
    const result = await fn();
    s.succeed(successMessage || message);
    return result;
  } catch (err) {
    const errMsg = err instanceof Error ? err.message : String(err);
    s.fail(errMsg);
    // Mark the error as already displayed so callers can skip re-displaying
    if (err instanceof Error) {
      (err as Error & { _displayed?: boolean })._displayed = true;
    }
    throw err;
  }
}

/**
 * Display an error message only if it wasn't already shown by a spinner.
 */
export function errorIfNew(err: unknown): void {
  if (err && typeof err === "object" && (err as { _displayed?: boolean })._displayed) {
    return;
  }
  error(err instanceof Error ? err.message : String(err));
}

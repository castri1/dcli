import {
  readFileSync,
  writeFileSync,
  renameSync,
  existsSync,
  mkdirSync,
  copyFileSync,
} from "node:fs";
import { join } from "node:path";
import lockfile from "proper-lockfile";
import { getConfigDir } from "../config/loader.js";
import type { AppState, TrackedExecution } from "./types.js";

const STATE_FILE = "state.json";
const COMPLETED_TTL_MS = 10 * 60 * 1000; // 10 minutes

function getStatePath(): string {
  return join(getConfigDir(), STATE_FILE);
}

function ensureDir(): void {
  const dir = getConfigDir();
  if (!existsSync(dir)) {
    mkdirSync(dir, { recursive: true });
  }
}

function emptyState(): AppState {
  return { tracked: [] };
}

/**
 * Write state atomically: write to .tmp, then rename.
 */
function atomicWriteState(statePath: string, state: AppState): void {
  const tmpPath = statePath + ".tmp";
  writeFileSync(tmpPath, JSON.stringify(state, null, 2), "utf-8");
  renameSync(tmpPath, statePath);
}

/**
 * Parse state from file contents with backward compatibility.
 * Handles missing `triggeredBy` field from older state files.
 */
function parseState(raw: string, statePath: string): AppState {
  try {
    const state: AppState = JSON.parse(raw);
    if (!state || !Array.isArray(state.tracked)) {
      return emptyState();
    }
    // Backward compat: ensure triggeredBy exists
    for (const entry of state.tracked) {
      if (!entry.triggeredBy) {
        entry.triggeredBy = "unknown";
      }
    }
    return state;
  } catch (err) {
    if (err instanceof SyntaxError) {
      // Corrupt JSON — backup and reset
      const backupPath = statePath + ".corrupt." + Date.now();
      try {
        copyFileSync(statePath, backupPath);
      } catch {
        // Ignore backup failures
      }
      console.error(
        `Warning: state.json was corrupt and has been reset. Backup at: ${backupPath}`
      );
      return emptyState();
    }
    throw err;
  }
}

/**
 * Clean up stale completed entries from state.
 */
function cleanStaleEntries(state: AppState): AppState {
  const now = Date.now();
  state.tracked = state.tracked.filter((t) => {
    if (
      t.status !== "InProgress" &&
      t.status !== "Unknown" &&
      t.completedAt
    ) {
      const completedTime = new Date(t.completedAt).getTime();
      return now - completedTime < COMPLETED_TTL_MS;
    }
    return true;
  });
  return state;
}

/**
 * Acquire a lock on the state file for writing.
 * Returns a release function.
 */
async function acquireLock(
  statePath: string
): Promise<() => Promise<void>> {
  try {
    return await lockfile.lock(statePath, {
      retries: { retries: 5, minTimeout: 100, maxTimeout: 1000 },
    });
  } catch (err) {
    throw new Error(
      `Could not acquire lock on state file. Another deploy-cli process may be running. (${(err as Error).message})`
    );
  }
}

/**
 * Read the state file (read-only, no writes).
 */
export async function readState(): Promise<AppState> {
  const statePath = getStatePath();
  ensureDir();

  if (!existsSync(statePath)) {
    return emptyState();
  }

  try {
    const raw = readFileSync(statePath, "utf-8");
    const state = parseState(raw, statePath);
    // Filter in memory but do not write back
    return cleanStaleEntries(state);
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") {
      return emptyState();
    }
    throw err;
  }
}

/**
 * Add a tracked execution to the state file.
 */
export async function addTrackedExecution(
  execution: TrackedExecution
): Promise<void> {
  const statePath = getStatePath();
  ensureDir();

  // Ensure file exists for locking
  if (!existsSync(statePath)) {
    writeFileSync(statePath, JSON.stringify(emptyState(), null, 2), "utf-8");
  }

  const release = await acquireLock(statePath);
  try {
    const raw = readFileSync(statePath, "utf-8");
    const state = cleanStaleEntries(parseState(raw, statePath));

    // Remove existing active entry for same pipeline+env
    state.tracked = state.tracked.filter(
      (t) =>
        !(t.env === execution.env && t.pipeline === execution.pipeline) ||
        (t.status !== "InProgress" && t.status !== "Unknown")
    );

    state.tracked.push(execution);
    atomicWriteState(statePath, state);
  } finally {
    await release();
  }
}

/**
 * Update the status of a tracked execution.
 */
export async function updateTrackedExecution(
  executionId: string,
  status: TrackedExecution["status"]
): Promise<void> {
  const statePath = getStatePath();
  ensureDir();

  if (!existsSync(statePath)) {
    return;
  }

  const release = await acquireLock(statePath);
  try {
    const raw = readFileSync(statePath, "utf-8");
    const state = cleanStaleEntries(parseState(raw, statePath));

    const entry = state.tracked.find((t) => t.executionId === executionId);
    if (entry) {
      entry.status = status;
      if (status !== "InProgress" && status !== "Unknown") {
        entry.completedAt = new Date().toISOString();
      }
    }

    atomicWriteState(statePath, state);
  } finally {
    await release();
  }
}

/**
 * Upsert an externally-discovered execution.
 * Only adds it if no execution with the same executionId already exists.
 * Returns true if it was inserted (new external execution), false if it already existed.
 */
export async function upsertExternalExecution(
  execution: TrackedExecution
): Promise<boolean> {
  const statePath = getStatePath();
  ensureDir();

  if (!existsSync(statePath)) {
    writeFileSync(statePath, JSON.stringify(emptyState(), null, 2), "utf-8");
  }

  const release = await acquireLock(statePath);
  try {
    const raw = readFileSync(statePath, "utf-8");
    const state = cleanStaleEntries(parseState(raw, statePath));

    // Check if this execution is already tracked
    const exists = state.tracked.some(
      (t) => t.executionId === execution.executionId
    );

    if (exists) {
      return false;
    }

    state.tracked.push(execution);
    atomicWriteState(statePath, state);
    return true;
  } finally {
    await release();
  }
}

/**
 * Get all currently active (InProgress) tracked executions.
 */
export async function getActiveExecutions(): Promise<TrackedExecution[]> {
  const state = await readState();
  return state.tracked.filter(
    (t) => t.status === "InProgress" || t.status === "Unknown"
  );
}

/**
 * Get all tracked executions (including recently completed).
 */
export async function getAllTrackedExecutions(): Promise<TrackedExecution[]> {
  const state = await readState();
  return state.tracked;
}

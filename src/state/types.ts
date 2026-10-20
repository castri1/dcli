import type { TriggerType } from "../aws/pipeline.js";

export interface TrackedExecution {
  env: string;
  pipeline: string;
  profile: string;
  region: string;
  executionId: string;
  startedAt: string; // ISO 8601
  status: "InProgress" | "Succeeded" | "Failed" | "Stopped" | "Unknown";
  completedAt?: string; // ISO 8601
  triggeredBy: TriggerType;
}

export interface AppState {
  tracked: TrackedExecution[];
}

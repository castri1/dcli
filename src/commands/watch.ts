import { loadConfig } from "../config/loader.js";
import { getAllTrackedExecutions } from "../state/manager.js";
import { startLiveView } from "../ui/live-view.js";
import { ensureCredentials, resolvePipelineContext } from "./shared.js";
import * as log from "../ui/logger.js";

export async function watchCommand(
  envName?: string,
  pipelineName?: string
): Promise<void> {
  const config = loadConfig();
  const filter = envName
    ? await resolvePipelineContext(config, envName, pipelineName)
    : undefined;

  if (filter) {
    await ensureCredentials(filter.envName, filter.profile);
  }

  const tracked = (await getAllTrackedExecutions()).filter((entry) => {
    if (!filter) {
      return true;
    }
    return (
      entry.env === filter.envName && entry.pipeline === filter.pipeline.name
    );
  });

  const activeCount = tracked.filter(
    (t) => t.status === "InProgress" || t.status === "Unknown"
  ).length;

  if (tracked.length > 0) {
    log.info(
      `Found ${tracked.length} tracked deployment${tracked.length > 1 ? "s" : ""} (${activeCount} active)`
    );
  } else if (filter) {
    log.info(
      `Watching ${filter.envName}/${filter.pipeline.name} for active executions...`
    );
  } else {
    log.info("No tracked deployments yet. Scanning configured pipelines...");
  }
  console.log("");

  // Start live view — returns when all pipelines complete or user presses Ctrl+C
  await startLiveView(config.defaults.pollIntervalSeconds, config, {
    filter: filter
      ? {
          envName: filter.envName,
          pipelineName: filter.pipeline.name,
        }
      : undefined,
  });
}

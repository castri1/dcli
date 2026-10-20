import notifier from "node-notifier";

/**
 * Send an OS notification for a pipeline completion.
 */
export function notifyPipelineComplete(
  pipelineName: string,
  env: string,
  status: "Succeeded" | "Failed" | "Stopped",
  duration: string
): void {
  const isSuccess = status === "Succeeded";
  const icon = isSuccess ? "✅" : "❌";
  const title = isSuccess
    ? `${icon} Pipeline Succeeded`
    : `${icon} Pipeline ${status}`;

  const message = `${pipelineName} (${env.toUpperCase()}) ${status.toLowerCase()} in ${duration}`;

  notifier.notify({
    title,
    message,
    sound: isSuccess ? "Glass" : "Basso",
    timeout: 10,
  });
}

/**
 * Send an OS notification for a stage transition.
 */
export function notifyStageTransition(
  pipelineName: string,
  env: string,
  stageName: string,
  transition: "started" | "passed" | "failed",
  duration?: string
): void {
  const envLabel = env.toUpperCase();

  switch (transition) {
    case "started":
      notifier.notify({
        title: "▶ Stage Started",
        message: `${pipelineName} (${envLabel}) — ${stageName} stage started`,
        sound: "Pop",
        timeout: 5,
      });
      break;
    case "passed": {
      const dur = duration ? ` (${duration})` : "";
      notifier.notify({
        title: "✓ Stage Passed",
        message: `${pipelineName} (${envLabel}) — ${stageName} stage passed${dur}`,
        sound: "Tink",
        timeout: 5,
      });
      break;
    }
    case "failed":
      notifier.notify({
        title: "✗ Stage Failed",
        message: `${pipelineName} (${envLabel}) — ${stageName} stage failed`,
        sound: "Basso",
        timeout: 10,
      });
      break;
  }
}

/**
 * Send an OS notification for a pending approval.
 */
export function notifyApprovalNeeded(
  pipelineName: string,
  env: string,
  stageName: string
): void {
  notifier.notify({
    title: "⏳ Approval Required",
    message: `${pipelineName} (${env.toUpperCase()}) is waiting for approval at "${stageName}"`,
    sound: "Purr",
    timeout: 15,
  });
}

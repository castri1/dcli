import { select, confirm, input } from "@inquirer/prompts";
import chalk from "chalk";
import boxen from "boxen";
import { loadConfig } from "../config/loader.js";
import {
  getExecutionStatus,
  submitApprovalResult,
  type PendingApproval,
} from "../aws/pipeline.js";
import {
  ensureCredentials,
  resolveExecutionReference,
  resolvePipelineContext,
} from "./shared.js";
import * as log from "../ui/logger.js";

export async function approveCommand(
  envName?: string,
  pipelineName?: string
): Promise<void> {
  const config = loadConfig();
  const context = await resolvePipelineContext(config, envName, pipelineName);
  const resolvedEnvName = context.envName;
  const resolvedPipelineName = context.pipeline.name;

  await ensureCredentials(resolvedEnvName, context.profile);

  const trackedExec = await resolveExecutionReference(context, {
    requireActive: true,
  });

  if (!trackedExec) {
    log.info(
      `No active execution found for '${resolvedPipelineName}' in ${resolvedEnvName}.`
    );
    log.dim("Deploy a pipeline first, or check if it's still running.");
    return;
  }

  // Get current status to find pending approvals
  const status = await log.withSpinner(
    "Checking for pending approvals",
    () =>
      getExecutionStatus(
        context.profile,
        context.region,
        resolvedPipelineName,
        trackedExec.executionId
      ),
    "Status retrieved"
  );

  if (status.pendingApprovals.length === 0) {
    log.info("No pending approvals for this pipeline.");
    return;
  }

  // If multiple approvals, let user pick one
  let approval: PendingApproval;
  if (status.pendingApprovals.length === 1) {
    approval = status.pendingApprovals[0];
  } else {
    const selectedAction = await select({
      message: "Multiple approvals pending. Select one:",
      choices: status.pendingApprovals.map((a) => ({
        name: `${chalk.cyan(a.stageName)} / ${chalk.white(a.actionName)}`,
        value: a.actionName,
      })),
    });
    approval = status.pendingApprovals.find(
      (a) => a.actionName === selectedAction
    )!;
  }

  // Display approval details
  const detailLines: string[] = [];
  detailLines.push(
    `${chalk.bold("Pipeline:")}  ${chalk.white(resolvedPipelineName)}`
  );
  detailLines.push(
    `${chalk.bold("Env:")}       ${chalk.cyan(resolvedEnvName.toUpperCase())}`
  );
  detailLines.push(
    `${chalk.bold("Stage:")}     ${chalk.white(approval.stageName)}`
  );
  detailLines.push(
    `${chalk.bold("Action:")}    ${chalk.white(approval.actionName)}`
  );
  if (approval.customData) {
    detailLines.push(
      `${chalk.bold("Details:")}   ${chalk.gray(approval.customData)}`
    );
  }
  if (approval.approvalReviewLink) {
    detailLines.push(
      `${chalk.bold("Review:")}    ${chalk.blue.underline(approval.approvalReviewLink)}`
    );
  }

  console.log("");
  console.log(
    boxen(detailLines.join("\n"), {
      borderStyle: "round",
      borderColor: "#f5a623",
      padding: { left: 1, right: 1, top: 0, bottom: 0 },
      title: chalk.yellow.bold(" Pending Approval "),
      titleAlignment: "center",
    })
  );
  console.log("");

  // Ask for approve or reject
  const action = await select({
    message: "What would you like to do?",
    choices: [
      {
        name: chalk.green("Approve"),
        value: "approve" as const,
      },
      {
        name: chalk.red("Reject"),
        value: "reject" as const,
      },
      {
        name: chalk.gray("Cancel (do nothing)"),
        value: "cancel" as const,
      },
    ],
  });

  if (action === "cancel") {
    log.info("No action taken.");
    return;
  }

  const approved = action === "approve";

  // Optional comment
  const comment = await input({
    message: `${approved ? "Approval" : "Rejection"} comment (optional):`,
  });

  // Confirm
  const confirmed = await confirm({
    message: `Are you sure you want to ${approved ? chalk.green("APPROVE") : chalk.red("REJECT")} this action?`,
    default: approved,
  });

  if (!confirmed) {
    log.info("Cancelled.");
    return;
  }

  // Submit
  await log.withSpinner(
    approved ? "Submitting approval" : "Submitting rejection",
    () =>
      submitApprovalResult(
        context.profile,
        context.region,
        resolvedPipelineName,
        approval.stageName,
        approval.actionName,
        approval.token,
        approved,
        comment.trim() || undefined
      ),
    approved ? "Approved successfully" : "Rejected successfully"
  );

  console.log("");
}

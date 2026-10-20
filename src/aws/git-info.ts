import {
  CodePipelineClient,
  ListActionExecutionsCommand,
} from "@aws-sdk/client-codepipeline";
import { execSync } from "node:child_process";
import { createCredentialProvider } from "./credentials.js";

export interface SourceRevision {
  commitId?: string;
  commitMessage?: string;
  branch?: string;
  repoName?: string;
}

/**
 * Get source revision info from a pipeline execution's Source stage.
 */
export async function getSourceRevision(
  profile: string,
  region: string,
  pipelineName: string,
  executionId: string
): Promise<SourceRevision | null> {
  const client = new CodePipelineClient({
    region,
    credentials: createCredentialProvider(profile),
  });

  const command = new ListActionExecutionsCommand({
    pipelineName,
    filter: { pipelineExecutionId: executionId },
  });
  const response = await client.send(command);
  const actions = response.actionExecutionDetails || [];

  // Find the source action
  const sourceAction = actions.find(
    (a) =>
      a.stageName === "Source" ||
      a.input?.actionTypeId?.category === "Source"
  );

  if (!sourceAction) return null;

  const output = sourceAction.output;
  const config = sourceAction.input?.configuration as
    | Record<string, string>
    | undefined;

  // Extract commit info from output variables
  const outputVars = output?.outputVariables || {};

  const commitId =
    outputVars["CommitId"] ||
    outputVars["commitId"] ||
    undefined;

  const commitMessage =
    outputVars["CommitMessage"] ||
    outputVars["commitMessage"] ||
    output?.executionResult?.externalExecutionSummary ||
    undefined;

  const branch =
    outputVars["BranchName"] ||
    outputVars["branchName"] ||
    config?.["BranchName"] ||
    config?.["Branch"] ||
    undefined;

  const repoName =
    config?.["RepositoryName"] ||
    config?.["Repo"] ||
    config?.["FullRepositoryId"] ||
    undefined;

  if (!commitId && !commitMessage && !branch) {
    return null;
  }

  return {
    commitId: commitId ? commitId.slice(0, 7) : undefined,
    commitMessage: commitMessage
      ? commitMessage.split("\n")[0].slice(0, 80)
      : undefined,
    branch,
    repoName,
  };
}

/**
 * Get the current local git branch and short commit hash.
 * Returns null if not in a git repo or git is not available.
 */
export function getLocalGitInfo(): {
  branch: string;
  commitId: string;
  commitMessage: string;
} | null {
  try {
    // Single git call: "short-hash|subject|branch-name"
    const output = execSync("git log -1 --format=%h|%s|%D", {
      stdio: ["pipe", "pipe", "pipe"],
    })
      .toString()
      .trim();

    const [commitId, commitMessage, refs] = output.split("|");
    if (!commitId) return null;

    // Extract branch name from refs like "HEAD -> main, origin/main"
    const branchMatch = refs?.match(/HEAD -> ([^,]+)/);
    const branch = branchMatch?.[1]?.trim() || "HEAD";

    return { branch, commitId, commitMessage: commitMessage || "" };
  } catch {
    return null;
  }
}

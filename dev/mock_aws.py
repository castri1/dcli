"""Fake AWS endpoint to try or develop dcli without an AWS account (see demo.sh).
Simulates CodePipeline, CodeBuild, CloudWatch Logs, SSM and Lambda with time-based progress."""
import calendar
import hashlib
import json
import math
import sys
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs

T0 = time.time()
SIGNED = {"ok": 0, "unsigned": 0}


def eid():
    return str(uuid.uuid4())


# ---- pipelines ---------------------------------------------------------------------------
STAGES = ["Source", "Build", "Deploy", "Approval", "Production"]
AVG = {"Source": 15, "Build": 240, "Deploy": 90, "Approval": 30, "Production": 120}
PIPES = {}


def new_run(name, offset, mode="run", trigger="Webhook"):
    """mode: run (progresses with time), approval (stops at Approval), fail (fails at Build),
    cfnfail (fails at Production, a CloudFormation deploy)."""
    PIPES[name] = {
        "exec": eid(), "start": time.time() - offset, "mode": mode, "trigger": trigger,
        "stopped": False, "approved": None,
        "history": [(eid(), "Succeeded", time.time() - 86400 * k - 3600, 470 + 30 * k) for k in range(1, 6)],
    }


COMMITS = [("3f9e1a2b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f", "fix(payments): retry on timeout"),
           ("9c8b7a6f5e4d3c2b1a0f9e8d7c6b5a4f3e2d1c0b", "chore: bump queue client"),
           ("5d4c3b2a1f0e9d8c7b6a5f4e3d2c1b0a9f8e7d6c", "feat(orders): split shipments"),
           ("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678", "feat: earlier change"),
           ("0f1e2d3c4b5a69788796a5b4c3d2e1f0a9b8c7d6", "chore: initial pipeline")]


def sha_at(name, k):
    """Commit of the current run (k=0) or the k-th history entry, unless the execution was
    started on a given one. A pipeline's "lag" makes it run older commits, like a later
    environment of the same app."""
    k += PIPES.get(name, {}).get("lag", 0)
    return COMMITS[min(k, len(COMMITS) - 1)][0]


def revision(name, k, execution=None):
    sha = PIPES.get(name, {}).get("revs", {}).get(execution) or sha_at(name, k)
    msg = dict(COMMITS).get(sha, "pinned revision")
    return [{"actionName": "GitHub", "revisionId": sha,
             "revisionUrl": f"https://github.com/acme/{name}/commit/{sha}",
             "revisionSummary": json.dumps({"ProviderType": "GitHub", "CommitMessage": msg})}]


def restart(name, mode, trigger, sha=None, **extra):
    """A new execution (StartPipelineExecution, RollbackStage): the current one joins the
    history, whose commits are fixed first so they don't shift."""
    old = PIPES[name]
    revs = old.setdefault("revs", {})
    for k, (h, *_) in enumerate(old["history"]):
        revs.setdefault(h, sha_at(name, k + 1))
    history = old["history"]
    if old["exec"]:
        revs.setdefault(old["exec"], sha_at(name, 0))
        status = exec_status(old)
        history = [(old["exec"], "Superseded" if status == "InProgress" else status, old["start"],
                    time.time() - old["start"])] + history
    lag = old.get("lag", 0)
    new_run(name, 0, mode, trigger)
    p = PIPES[name]
    p.update(history=history[:10], revs=revs, lag=lag, **extra)
    revs[p["exec"]] = sha or sha_at(name, 0)
    return p["exec"]


new_run("app-dev", 120)
new_run("api-dev", 600, "approval")
new_run("worker-dev", 400, "fail")
PIPES["worker-dev"]["history"][0] = (eid(), "Failed", time.time() - 86400 - 3600, 300)
new_run("infra-dev", 900, "cfnfail")
new_run("app-prod", 0, "idle")
PIPES["app-prod"]["exec"] = None
PIPES["app-prod"]["lag"] = 2
new_run("app-qa", 600, "approval")
PIPES["app-qa"]["lag"] = 1
# Stage rollback (RollbackStage) needs V2 pipelines; the others are V1.
V2_PIPELINES = {"app-dev", "app-prod", "infra-dev"}
# In the account but not in demo-config.yaml: found with `n` (add pipelines); they get a
# finished run when first looked at.
UNLISTED = ["legacy-batch", "billing-dev", "billing-prod", "search-indexer-dev", "notifications-v2-dev",
            "support-ai-buyer-dev", "support-ai-seller-dev", "data-export-nightly"]


def timeline(p):
    """Per stage: (status, start, end) for the current execution."""
    now = time.time()
    t = p["start"]
    if p["mode"] == "rollback":
        # Only the rolled back stage runs.
        stage, dur = p["rollback_stage"], AVG[p["rollback_stage"]]
        out = {s: (None, None, None) for s in STAGES}
        out[stage] = ("Succeeded", t, t + dur) if t + dur <= now else ("InProgress", t, None)
        return out
    out = {}
    for s in STAGES:
        dur = AVG[s] * (1.4 if s == "Build" and p["mode"] == "fail" else 1)
        if p["stopped"]:
            out[s] = ("Stopped", t, now) if t < now else (None, None, None)
            t = now + 1e9
            continue
        if p["mode"] == "fail" and s == "Build":
            out[s] = ("Failed", t, t + 70)
            t = now + 1e9
            continue
        if p["mode"] == "cfnfail" and s == "Production" and t < now:
            out[s] = ("Failed", t, t + 45)
            t = now + 1e9
            continue
        if p["mode"] == "approval" and s == "Approval" and p["approved"] is None:
            out[s] = ("InProgress", t, None) if t < now else (None, None, None)
            t = now + 1e9
            continue
        if t > now:
            out[s] = (None, None, None)
        elif t + dur > now:
            out[s] = ("InProgress", t, None)
        else:
            out[s] = ("Succeeded", t, t + dur)
        t += dur
    return out


def exec_status(p):
    tl = timeline(p)
    sts = [v[0] for v in tl.values() if v[0] or p["mode"] != "rollback"]
    if "Failed" in sts:
        return "Failed"
    if "Stopped" in sts:
        return "Stopped"
    if all(s == "Succeeded" for s in sts):
        return "Succeeded"
    return "InProgress"


def summary(name, p):
    tl = timeline(p)
    ends = [v[2] for v in tl.values() if v[2]]
    out = {
        "pipelineExecutionId": p["exec"], "status": exec_status(p), "startTime": p["start"],
        "lastUpdateTime": max(ends) if exec_status(p) != "InProgress" and ends else time.time(),
        "trigger": {"triggerType": p["trigger"]},
        "sourceRevisions": revision(name, 0, p["exec"]),
        "executionType": "ROLLBACK" if p["mode"] == "rollback" else "STANDARD",
    }
    if p["mode"] == "rollback":
        out["rollbackMetadata"] = {"rollbackTargetPipelineExecutionId": p["rollback_target"]}
    return out


def declaration(name):
    """GetPipeline: the stages and their actions (the approval with its CustomData)."""
    category = {"Source": "Source", "Build": "Build", "Deploy": "Deploy", "Approval": "Approval", "Production": "Deploy"}
    provider = {"Source": "CodeStarSourceConnection", "Build": "CodeBuild", "Deploy": "CodeDeploy",
                "Approval": "Manual", "Production": "CloudFormation"}
    config = {
        "Source": {"ConnectionArn": "arn:aws:codeconnections:us-east-1:123456789012:connection/demo",
                   "FullRepositoryId": f"acme/{name}", "BranchName": "main"},
        "Build": {"ProjectName": f"{name}-build"},
        "Deploy": {"ApplicationName": name, "DeploymentGroupName": "web"},
        "Approval": {"CustomData": "Check the release notes and the staging dashboard before approving.",
                     "ExternalEntityLink": f"https://github.com/acme/{name}/releases"},
        "Production": {"StackName": f"{name}-stack", "ActionMode": "CREATE_UPDATE"},
    }
    stages = [{"name": s, "actions": [{
        "name": action_name(s), "runOrder": 1, "configuration": config[s],
        "actionTypeId": {"category": category[s], "owner": "AWS", "provider": provider[s], "version": "1"}}]}
        for s in STAGES]
    return {"pipeline": {"name": name, "pipelineType": "V2" if name in V2_PIPELINES else "V1", "version": 3,
                         "roleArn": "arn:aws:iam::123456789012:role/codepipeline", "stages": stages},
            "metadata": {"pipelineArn": f"arn:aws:codepipeline:us-east-1:123456789012:{name}"}}


def action_name(stage):
    return {"Source": "GitHub", "Build": "Build", "Deploy": "Deploy", "Approval": "ManualApproval", "Production": "DeployProd"}[stage]


def pipeline_state(name):
    p = PIPES[name]
    stages = []
    if p["exec"]:
        tl = timeline(p)
        for s in STAGES:
            st, start, end = tl[s]
            le = {"status": st, "lastStatusChange": end or start}
            if s == "Build":
                le["externalExecutionId"] = f"{name}-build:{p['exec'][:8]}"
                le["externalExecutionUrl"] = "https://console.aws.amazon.com/codebuild/home"
            if st == "Failed":
                le["errorDetails"] = {"code": "JobFailed", "message": "Error while executing command: npm test. Reason: exit status 1"
                                      if s == "Build" else f"Stack {name}-stack failed: UPDATE_ROLLBACK_COMPLETE"}
            if s == "Approval" and st == "InProgress":
                le["token"] = "tok-" + p["exec"][:8]
                le["summary"] = "Please review the release notes before promoting to production."
            stage = {"stageName": s, "actionStates": [{"actionName": action_name(s), **({"latestExecution": le} if st else {})}]}
            if st:
                stage["latestExecution"] = {"pipelineExecutionId": p["exec"], "status": st}
            stages.append(stage)
    else:
        stages = [{"stageName": s, "actionStates": [{"actionName": action_name(s)}]} for s in STAGES]
    return {"pipelineName": name, "stageStates": stages}


def action_details(name, exec_filter):
    p = PIPES[name]
    out = []
    if p["exec"] and exec_filter in (None, p["exec"]):
        for s, (st, start, end) in timeline(p).items():
            if not st:
                continue
            provider = {"Source": "CodeStarSourceConnection", "Build": "CodeBuild", "Deploy": "CodeDeploy",
                        "Approval": "Manual", "Production": "CloudFormation"}[s]
            d = {"pipelineExecutionId": p["exec"], "stageName": s, "actionName": action_name(s), "startTime": start,
                 "lastUpdateTime": end or time.time(), "status": st,
                 "input": {"actionTypeId": {"category": "Source" if s == "Source" else "Build", "owner": "AWS", "provider": provider},
                           "configuration": {"BranchName": "main", "FullRepositoryId": "acme/" + name} if s == "Source"
                           else {"StackName": f"{name}-stack", "ActionMode": "CREATE_UPDATE"} if s == "Production" else {}},
                 "output": {"outputVariables": {"BranchName": "main"} if s == "Source" else {},
                            "executionResult": {"externalExecutionId": f"{name}-build:{p['exec'][:8]}",
                                                **({"externalExecutionSummary": "Build terminated with state: FAILED",
                                                    "errorDetails": {"code": "JobFailed", "message": "Build terminated with state: FAILED. Phase BUILD: COMMAND_EXECUTION_ERROR"}}
                                                   if st == "Failed" else {})} if s == "Build" else {}}}
            out.append(d)
    if exec_filter is None:
        for hid, _, start, total in p["history"]:
            t = start
            for s in STAGES:
                out.append({"pipelineExecutionId": hid, "stageName": s, "actionName": action_name(s),
                            "startTime": t, "lastUpdateTime": t + AVG[s], "status": "Succeeded"})
                t += AVG[s]
    return out


def codepipeline(op, body):
    name = body.get("name") or body.get("pipelineName")
    if name in UNLISTED and name not in PIPES:
        new_run(name, 3000)
    if op == "ListPipelines":
        names = sorted(set(PIPES) | set(UNLISTED))
        return {"pipelines": [{"name": n, "updated": time.time() - 3600 * (i * 7 + 2), "pipelineType": "V2"}
                              for i, n in enumerate(names)]}
    if op == "GetPipelineState":
        return pipeline_state(name)
    if op == "ListPipelineExecutions":
        p = PIPES[name]
        items = ([summary(name, p)] if p["exec"] else []) + [
            {"pipelineExecutionId": h, "status": st, "startTime": s, "lastUpdateTime": s + d,
             "trigger": {"triggerType": "StartPipelineExecution"}, "sourceRevisions": revision(name, k + 1, h)}
            for k, (h, st, s, d) in enumerate(p["history"])]
        return {"pipelineExecutionSummaries": items[: body.get("maxResults", 10)]}
    if op == "GetPipelineExecution":
        return {"pipelineExecution": summary(name, PIPES[name])}
    if op == "GetPipeline":
        return declaration(name)
    if op == "ListActionExecutions":
        return {"actionExecutionDetails": action_details(name, (body.get("filter") or {}).get("pipelineExecutionId"))}
    if op == "StartPipelineExecution":
        sha = None
        for r in body.get("sourceRevisions") or []:
            if r.get("actionName") != action_name("Source"):
                raise KeyError(f"InvalidArgumentException: no source action named {r.get('actionName')} in {name}")
            if r.get("revisionType") != "COMMIT_ID":
                raise KeyError(f"ValidationException: revision type {r.get('revisionType')} is not supported for "
                               "CodeStarSourceConnection source actions")
            sha = r["revisionValue"]
        mode = "approval" if name in ("api-dev", "app-qa") else "run"
        return {"pipelineExecutionId": restart(name, mode, "StartPipelineExecution", sha)}
    if op == "RollbackStage":
        if name not in V2_PIPELINES:
            raise KeyError("ValidationException: Rollback is only supported for pipelines of type V2")
        target, stage = body["targetPipelineExecutionId"], body["stageName"]
        p = PIPES[name]
        found = [k for k, h in enumerate(p["history"]) if h[0] == target]
        if not found or p["history"][found[0]][1] != "Succeeded":
            raise KeyError(f"PipelineExecutionNotFoundException: no successful execution {target} of {name}")
        if stage not in STAGES or stage == "Source":
            raise KeyError(f"StageNotFoundException: {stage}")
        sha = p.get("revs", {}).get(target) or sha_at(name, found[0] + 1)
        return {"pipelineExecutionId": restart(name, "rollback", "ManualRollback", sha, rollback_stage=stage,
                                               rollback_target=target)}
    if op == "StopPipelineExecution":
        PIPES[name]["stopped"] = True
        return {"pipelineExecutionId": PIPES[name]["exec"]}
    if op == "PutApprovalResult":
        PIPES[name]["approved"] = body["result"]["status"]
        PIPES[name]["mode"] = "run"
        return {"approvedAt": time.time()}
    raise KeyError(op)


# ---- CloudFormation (awsQuery, Version 2010-05-15) ------------------------------------------
# Stacks for the CloudFormation module: a healthy API (JSON template, outputs, change sets),
# a rolled-back update whose cause is inside a nested stack (YAML), a failed creation, an
# update in progress and a drifted stack. The pipelines' "<pipeline>-stack" events (failure
# reports) stay as they were.
CFN_ARN = "arn:aws:cloudformation:us-east-1:123456789012:stack"
CFN_ROLE = "arn:aws:iam::123456789012:role/cfn-deployer"


def cfn_id(name):
    return f"{CFN_ARN}/{name}/{hashlib.sha256(name.encode()).hexdigest()[:8]}-0f2b-11ef-9c1d-0a1b2c3d4e5f"


CFN_ORDERS_TEMPLATE = json.dumps({
    "AWSTemplateFormatVersion": "2010-09-09", "Description": "Orders API: function, queue, table",
    "Parameters": {"Stage": {"Type": "String", "Default": "dev"},
                   "DbPassword": {"Type": "String", "NoEcho": True}},
    "Resources": {
        "OrdersFunction": {"Type": "AWS::Lambda::Function", "Properties": {
            "FunctionName": "orders-sync", "Runtime": "python3.13", "Handler": "app.handler", "MemorySize": 1024,
            "Role": {"Fn::GetAtt": ["OrdersRole", "Arn"]}, "Environment": {"Variables": {"STAGE": {"Ref": "Stage"}}}}},
        "OrdersRole": {"Type": "AWS::IAM::Role", "Properties": {"AssumeRolePolicyDocument": {"Version": "2012-10-17"}}},
        "OrdersQueue": {"Type": "AWS::SQS::Queue", "Properties": {"QueueName": "orders", "VisibilityTimeout": 60}},
        "OrdersTable": {"Type": "AWS::DynamoDB::Table", "Properties": {"TableName": "orders", "BillingMode": "PAY_PER_REQUEST"}},
        "AssetsBucket": {"Type": "AWS::S3::Bucket", "Properties": {"BucketName": "acme-app-assets"}},
        "Fulfillment": {"Type": "AWS::StepFunctions::StateMachine", "Properties": {"StateMachineName": "order-fulfillment"}},
        "OrdersLogs": {"Type": "AWS::Logs::LogGroup", "Properties": {"LogGroupName": "/aws/lambda/orders-sync", "RetentionInDays": 30}},
        "DbUrl": {"Type": "AWS::SSM::Parameter", "Properties": {"Name": "/app/db/url", "Type": "String", "Value": "postgres://db"}}},
    "Outputs": {"ApiUrl": {"Value": "https://a1b2c3.execute-api.us-east-1.amazonaws.com/dev"},
                "QueueUrl": {"Value": {"Ref": "OrdersQueue"}, "Export": {"Name": "orders-queue-url"}}}}, indent=2)

CFN_PAYMENTS_TEMPLATE = """AWSTemplateFormatVersion: '2010-09-09'
Description: Payments service
Parameters:
  Stage:
    Type: String
  StripeKey:
    Type: String
    NoEcho: true
Resources:
  WorkerStack:
    Type: AWS::CloudFormation::Stack
    Properties:
      TemplateURL: https://s3.amazonaws.com/acme-templates/payments-worker.yaml
      Parameters:
        Stage: !Ref Stage
  PaymentsApi:
    Type: AWS::ApiGateway::RestApi
    Properties:
      Name: !Sub payments-${Stage}
Outputs:
  ApiId:
    Value: !Ref PaymentsApi
"""

CFN_WORKER_TEMPLATE = """AWSTemplateFormatVersion: '2010-09-09'
Parameters:
  Stage:
    Type: String
Resources:
  WorkerRole:
    Type: AWS::IAM::Role
    Properties:
      AssumeRolePolicyDocument:
        Version: '2012-10-17'
  WorkerFunction:
    Type: AWS::Lambda::Function
    Properties:
      FunctionName: !Sub payments-worker-${Stage}
      Runtime: nodejs12.x
      Handler: index.handler
      Role: !GetAtt WorkerRole.Arn
      Code:
        S3Bucket: acme-artifacts
        S3Key: payments/worker.zip
  WorkerQueue:
    Type: AWS::SQS::Queue
"""

CFN_NETWORK_TEMPLATE = """Resources:
  AppSecurityGroup:
    Type: AWS::EC2::SecurityGroup
    Properties:
      GroupDescription: App servers
      SecurityGroupIngress:
        - IpProtocol: tcp
          FromPort: 443
          ToPort: 443
          CidrIp: 10.0.0.0/16
  LogsBucket:
    Type: AWS::S3::Bucket
    Properties:
      BucketName: acme-logs-archive
"""

CFN_CRON_TEMPLATE = json.dumps({"Resources": {
    "CronRole": {"Type": "AWS::IAM::Role", "Properties": {"RoleName": "reports-cron"}},
    "CronFunction": {"Type": "AWS::Lambda::Function", "Properties": {"Runtime": "python3.13", "Role": {"Fn::GetAtt": ["CronRole", "Arn"]}}},
    "CronSchedule": {"Type": "AWS::Events::Rule", "Properties": {"ScheduleExpression": "cron(0 6 * * ? *)"}}}}, indent=2)

CFN_ANALYTICS_TEMPLATE = """Resources:
  StreamProcessor:
    Type: AWS::Lambda::Function
    Properties:
      Runtime: java21
  Dashboard:
    Type: AWS::CloudWatch::Dashboard
"""


def cfn_stacks():
    """Each stack: its description, resources (logical id, type, physical id) and events
    (seconds after its last update started, logical id, type, status, reason), oldest first."""
    t, worker = T0 - 3 * 3600, "payments-WorkerStack-1ABC2DEF3GH4"
    s = "AWS::CloudFormation::Stack"
    return [
        {"name": "orders-api", "status": "UPDATE_COMPLETE", "created": T0 - 90 * 86400, "updated": T0 - 2 * 86400,
         "description": "Orders API: function, queue, table", "template": CFN_ORDERS_TEMPLATE,
         "params": [("Stage", "dev"), ("DbPassword", "****")],
         "outputs": [("ApiUrl", "https://a1b2c3.execute-api.us-east-1.amazonaws.com/dev", "The API's URL", None),
                     ("QueueUrl", "https://sqs.us-east-1.amazonaws.com/123456789012/orders", None, "orders-queue-url")],
         "resources": [("OrdersFunction", "AWS::Lambda::Function", "orders-sync"),
                       ("OrdersRole", "AWS::IAM::Role", "orders-api-OrdersRole-7Q2W"),
                       ("OrdersQueue", "AWS::SQS::Queue", "https://sqs.us-east-1.amazonaws.com/123456789012/orders"),
                       ("OrdersTable", "AWS::DynamoDB::Table", "orders"),
                       ("AssetsBucket", "AWS::S3::Bucket", "acme-app-assets"),
                       ("Fulfillment", "AWS::StepFunctions::StateMachine",
                        "arn:aws:states:us-east-1:123456789012:stateMachine:order-fulfillment"),
                       ("OrdersLogs", "AWS::Logs::LogGroup", "/aws/lambda/orders-sync"),
                       ("DbUrl", "AWS::SSM::Parameter", "/app/db/url")],
         "events": [(0, "orders-api", s, "UPDATE_IN_PROGRESS", "User Initiated"),
                    (9, "OrdersFunction", "AWS::Lambda::Function", "UPDATE_IN_PROGRESS", ""),
                    (21, "OrdersFunction", "AWS::Lambda::Function", "UPDATE_COMPLETE", ""),
                    (25, "orders-api", s, "UPDATE_COMPLETE_CLEANUP_IN_PROGRESS", ""),
                    (27, "orders-api", s, "UPDATE_COMPLETE", "")]},
        {"name": "payments", "status": "UPDATE_ROLLBACK_COMPLETE", "created": T0 - 200 * 86400, "updated": t,
         "description": "Payments service", "template": CFN_PAYMENTS_TEMPLATE,
         "params": [("Stage", "dev"), ("StripeKey", "****")], "outputs": [("ApiId", "p4y5x9", None, None)],
         "resources": [("WorkerStack", s, cfn_id(worker)), ("PaymentsApi", "AWS::ApiGateway::RestApi", "p4y5x9")],
         "events": [(0, "payments", s, "UPDATE_IN_PROGRESS", "User Initiated"),
                    (3, "WorkerStack", s, "UPDATE_IN_PROGRESS", ""),
                    (4, "PaymentsApi", "AWS::ApiGateway::RestApi", "UPDATE_IN_PROGRESS", ""),
                    (19, "WorkerStack", s, "UPDATE_FAILED",
                     f"Embedded stack {cfn_id(worker)} was not successfully updated. Currently in UPDATE_ROLLBACK_IN_PROGRESS "
                     "with reason: The following resource(s) failed to update: [WorkerFunction]."),
                    (20, "PaymentsApi", "AWS::ApiGateway::RestApi", "UPDATE_FAILED", "Resource update cancelled"),
                    (21, "payments", s, "UPDATE_ROLLBACK_IN_PROGRESS",
                     "The following resource(s) failed to update: [PaymentsApi, WorkerStack]."),
                    (48, "WorkerStack", s, "UPDATE_COMPLETE", ""),
                    (50, "payments", s, "UPDATE_ROLLBACK_COMPLETE_CLEANUP_IN_PROGRESS", ""),
                    (52, "payments", s, "UPDATE_ROLLBACK_COMPLETE", "")]},
        {"name": worker, "status": "UPDATE_ROLLBACK_COMPLETE", "created": T0 - 200 * 86400, "updated": t + 3,
         "parent": "payments", "template": CFN_WORKER_TEMPLATE, "params": [("Stage", "dev")], "outputs": [],
         "resources": [("WorkerRole", "AWS::IAM::Role", "payments-worker-role"),
                       ("WorkerFunction", "AWS::Lambda::Function", "payments-worker-dev"),
                       ("WorkerQueue", "AWS::SQS::Queue", "https://sqs.us-east-1.amazonaws.com/123456789012/payments-worker")],
         "events": [(0, worker, s, "UPDATE_IN_PROGRESS", ""),
                    (2, "WorkerRole", "AWS::IAM::Role", "UPDATE_IN_PROGRESS", ""),
                    (6, "WorkerRole", "AWS::IAM::Role", "UPDATE_COMPLETE", ""),
                    (7, "WorkerFunction", "AWS::Lambda::Function", "UPDATE_IN_PROGRESS", ""),
                    (14, "WorkerFunction", "AWS::Lambda::Function", "UPDATE_FAILED",
                     'Resource handler returned message: "The runtime parameter of nodejs12.x is no longer supported for '
                     'creating or updating AWS Lambda functions. We recommend you use a supported runtime while creating '
                     'or updating functions. (Service: Lambda, Status Code: 400, Request ID: 6c1e0f2a)" '
                     '(RequestToken: 3f7a, HandlerErrorCode: InvalidRequest)'),
                    (15, "WorkerQueue", "AWS::SQS::Queue", "UPDATE_FAILED", "Resource update cancelled"),
                    (16, worker, s, "UPDATE_ROLLBACK_IN_PROGRESS", "The following resource(s) failed to update: [WorkerFunction]."),
                    (40, worker, s, "UPDATE_ROLLBACK_COMPLETE", "")]},
        {"name": "reports-cron", "status": "ROLLBACK_COMPLETE", "created": T0 - 5 * 3600, "updated": None,
         "reason": "The following resource(s) failed to create: [CronRole]. Rollback requested by user.",
         "template": CFN_CRON_TEMPLATE, "params": [], "outputs": [], "resources": [],
         "events": [(0, "reports-cron", s, "CREATE_IN_PROGRESS", "User Initiated"),
                    (2, "CronRole", "AWS::IAM::Role", "CREATE_IN_PROGRESS", ""),
                    (3, "CronRole", "AWS::IAM::Role", "CREATE_FAILED",
                     "Resource handler returned message: \"User: arn:aws:sts::123456789012:assumed-role/cfn-deployer/session "
                     "is not authorized to perform: iam:CreateRole on resource: arn:aws:iam::123456789012:role/reports-cron "
                     "because no identity-based policy allows the iam:CreateRole action (Service: Iam, Status Code: 403)\""),
                    (4, "CronSchedule", "AWS::Events::Rule", "CREATE_FAILED", "Resource creation cancelled"),
                    (5, "reports-cron", s, "ROLLBACK_IN_PROGRESS",
                     "The following resource(s) failed to create: [CronRole, CronSchedule]. Rollback requested by user."),
                    (12, "CronSchedule", "AWS::Events::Rule", "DELETE_COMPLETE", ""),
                    (13, "reports-cron", s, "ROLLBACK_COMPLETE", "")]},
        {"name": "analytics-pipeline", "status": "UPDATE_IN_PROGRESS", "created": T0 - 30 * 86400, "updated": T0 - 30,
         "template": CFN_ANALYTICS_TEMPLATE, "params": [], "outputs": [],
         "resources": [("StreamProcessor", "AWS::Lambda::Function", "report-builder"),
                       ("Dashboard", "AWS::CloudWatch::Dashboard", "analytics")],
         "events": [(0, "analytics-pipeline", s, "UPDATE_IN_PROGRESS", "User Initiated"),
                    (4, "StreamProcessor", "AWS::Lambda::Function", "UPDATE_IN_PROGRESS", ""),
                    (40, "StreamProcessor", "AWS::Lambda::Function", "UPDATE_COMPLETE", ""),
                    (45, "Dashboard", "AWS::CloudWatch::Dashboard", "UPDATE_IN_PROGRESS", "")]},
        {"name": "shared-network", "status": "CREATE_COMPLETE", "created": T0 - 400 * 86400, "updated": None,
         "drift": "DRIFTED", "drift_checked": T0 - 3 * 86400, "template": CFN_NETWORK_TEMPLATE, "params": [],
         "outputs": [("SecurityGroupId", "sg-0a1b2c3d", None, "app-sg")],
         "resources": [("AppSecurityGroup", "AWS::EC2::SecurityGroup", "sg-0a1b2c3d"),
                       ("LogsBucket", "AWS::S3::Bucket", "acme-logs-archive")],
         "events": [(0, "shared-network", s, "CREATE_IN_PROGRESS", "User Initiated"),
                    (40, "shared-network", s, "CREATE_COMPLETE", "")]},
    ]


# Drift detections started: id → (stack name, when). Each takes 4 seconds.
CFN_DRIFTS = {}


def cfn_find(stacks, name):
    return next((x for x in stacks if name in (x["name"], cfn_id(x["name"]))), None)


def cfn_drift(st):
    """(drift status, last check) after the detections run so far."""
    done = [at for (n, at) in CFN_DRIFTS.values() if n == st["name"] and time.time() - at >= 4]
    if done:
        return ("DRIFTED" if st["name"] == "shared-network" else "IN_SYNC"), max(done) + 4
    return st.get("drift", "NOT_CHECKED"), st.get("drift_checked")


def cfn_resource_drifts(st):
    out = []
    for lid, rtype, phys in st["resources"]:
        if st["name"] == "shared-network" and lid == "AppSecurityGroup":
            out.append((lid, rtype, phys, "MODIFIED", [
                ("/SecurityGroupIngress/1", None, '{"CidrIp":"0.0.0.0/0","FromPort":22,"IpProtocol":"tcp","ToPort":22}', "ADD"),
                ("/GroupDescription", "App servers", "App servers (temp ssh)", "NOT_EQUAL")]))
        else:
            out.append((lid, rtype, phys, "IN_SYNC", []))
    return out


def cfn_iso(t):
    return time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(t))


def cloudformation(params):
    """Query protocol: form-encoded request, XML response."""
    action = params.get("Action", [""])[0]
    stack = params.get("StackName", [""])[0]
    if action == "DescribeStackDriftDetectionStatus":
        return cfn_drift_status(params)
    if action == "DescribeStackEvents" and stack.removesuffix("-stack") in PIPES:
        return cfn_pipeline_events(stack, stack.removesuffix("-stack"))
    stacks = cfn_stacks()
    st = cfn_find(stacks, stack)
    if action == "DescribeStacks":
        if stack and st is None:
            return cfn_missing(stack)
        return 200, cfn_wrap("DescribeStacks", f"<Stacks>{''.join(cfn_stack_xml(c) for c in ([st] if stack else stacks))}</Stacks>")
    if st is None:
        return cfn_missing(stack)
    handler = {"DescribeStackEvents": cfn_events, "ListStackResources": cfn_resources, "GetTemplate": cfn_template,
               "DetectStackDrift": cfn_detect, "ListChangeSets": cfn_change_sets, "DescribeChangeSet": cfn_change_set,
               "DescribeStackResourceDrifts": cfn_resource_drifts_xml}.get(action)
    if handler is None:
        return 400, query_error("InvalidAction", f"{action} is not in the mock")
    return 200, cfn_wrap(action, handler(st, params))


def cfn_wrap(action, body):
    return f"<{action}Response><{action}Result>{body}</{action}Result></{action}Response>"


def cfn_missing(stack):
    return 400, query_error("ValidationError", f"Stack with id {xml_escape(stack)} does not exist")


def cfn_stack_xml(c):
    x = lambda v: xml_escape(str(v))
    drift, checked = cfn_drift(c)
    parent = cfn_id(c["parent"]) if c.get("parent") else None
    return (f"<member><StackId>{cfn_id(c['name'])}</StackId><StackName>{c['name']}</StackName>"
            + (f"<Description>{x(c['description'])}</Description>" if c.get("description") else "")
            + "<Parameters>" + "".join(f"<member><ParameterKey>{k}</ParameterKey><ParameterValue>{x(v)}</ParameterValue></member>"
                                       for k, v in c["params"]) + "</Parameters>"
            + f"<CreationTime>{cfn_iso(c['created'])}</CreationTime>"
            + (f"<LastUpdatedTime>{cfn_iso(c['updated'])}</LastUpdatedTime>" if c.get("updated") else "")
            + f"<StackStatus>{c['status']}</StackStatus>"
            + (f"<StackStatusReason>{x(c['reason'])}</StackStatusReason>" if c.get("reason") else "")
            + "<Outputs>" + "".join(
                f"<member><OutputKey>{k}</OutputKey><OutputValue>{x(v)}</OutputValue>"
                + (f"<Description>{x(d)}</Description>" if d else "") + (f"<ExportName>{x(e)}</ExportName>" if e else "")
                + "</member>" for k, v, d, e in c["outputs"]) + "</Outputs>"
            + (f"<ParentId>{parent}</ParentId><RootId>{parent}</RootId>" if parent else "")
            + f"<DriftInformation><StackDriftStatus>{drift}</StackDriftStatus>"
            + (f"<LastCheckTimestamp>{cfn_iso(checked)}</LastCheckTimestamp>" if checked else "") + "</DriftInformation>"
            + f"<EnableTerminationProtection>{'true' if c['name'] == 'shared-network' else 'false'}</EnableTerminationProtection>"
            + f"<RoleARN>{CFN_ROLE}</RoleARN></member>")


def cfn_events(st, params):
    x = lambda v: xml_escape(str(v))
    base = st.get("updated") or st["created"]
    phys = {lid: p for lid, _, p in st["resources"]}
    events = [e for e in st["events"] if base + e[0] <= time.time()][::-1]
    members = "".join(
        f"<member><StackId>{cfn_id(st['name'])}</StackId><EventId>{lid}-{i}</EventId><StackName>{st['name']}</StackName>"
        f"<LogicalResourceId>{lid}</LogicalResourceId>"
        f"<PhysicalResourceId>{x(cfn_id(st['name']) if lid == st['name'] else phys.get(lid, ''))}</PhysicalResourceId>"
        f"<ResourceType>{rtype}</ResourceType><Timestamp>{cfn_iso(base + dt)}</Timestamp><ResourceStatus>{status}</ResourceStatus>"
        + (f"<ResourceStatusReason>{x(reason)}</ResourceStatusReason>" if reason else "") + "</member>"
        for i, (dt, lid, rtype, status, reason) in enumerate(events))
    return f"<StackEvents>{members}</StackEvents>"


def cfn_resources(st, params):
    x = lambda v: xml_escape(str(v))
    drifts = {d[0]: d[3] for d in cfn_resource_drifts(st)} if cfn_drift(st)[0] != "NOT_CHECKED" else {}
    in_progress = st["status"] == "UPDATE_IN_PROGRESS" and time.time() - st["updated"] < 40
    status = {"CREATE_COMPLETE": "CREATE_COMPLETE"}.get(st["status"], "UPDATE_COMPLETE")
    members = "".join(
        f"<member><LogicalResourceId>{lid}</LogicalResourceId><PhysicalResourceId>{x(p)}</PhysicalResourceId>"
        f"<ResourceType>{rtype}</ResourceType><LastUpdatedTimestamp>{cfn_iso(st.get('updated') or st['created'])}</LastUpdatedTimestamp>"
        f"<ResourceStatus>{'UPDATE_IN_PROGRESS' if (in_progress and lid == 'StreamProcessor') or lid == 'Dashboard' and st['status'] == 'UPDATE_IN_PROGRESS' else status}</ResourceStatus>"
        f"<DriftInformation><StackResourceDriftStatus>{drifts.get(lid, 'NOT_CHECKED')}</StackResourceDriftStatus></DriftInformation></member>"
        for lid, rtype, p in st["resources"])
    return f"<StackResourceSummaries>{members}</StackResourceSummaries>"


def cfn_template(st, params):
    return f"<TemplateBody>{xml_escape(st['template'])}</TemplateBody><StagesAvailable><member>Original</member></StagesAvailable>"


def cfn_detect(st, params):
    did = str(uuid.uuid4())
    CFN_DRIFTS[did] = (st["name"], time.time())
    return f"<StackDriftDetectionId>{did}</StackDriftDetectionId>"


def cfn_drift_status(params):
    did = params.get("StackDriftDetectionId", [""])[0]
    name, at = CFN_DRIFTS.get(did, (None, 0))
    if name is None:
        return 400, query_error("ValidationError", f"No drift detection {did}")
    done = time.time() - at >= 4
    drift = "DRIFTED" if name == "shared-network" else "IN_SYNC"
    body = (f"<StackId>{cfn_id(name)}</StackId><StackDriftDetectionId>{did}</StackDriftDetectionId>"
            f"<DetectionStatus>{'DETECTION_COMPLETE' if done else 'DETECTION_IN_PROGRESS'}</DetectionStatus>"
            + (f"<StackDriftStatus>{drift}</StackDriftStatus><DriftedStackResourceCount>{1 if drift == 'DRIFTED' else 0}"
               "</DriftedStackResourceCount>" if done else "")
            + f"<Timestamp>{cfn_iso(time.time())}</Timestamp>")
    return 200, cfn_wrap("DescribeStackDriftDetectionStatus", body)


def cfn_resource_drifts_xml(st, params):
    x = lambda v: xml_escape(str(v))
    members = "".join(
        f"<member><StackId>{cfn_id(st['name'])}</StackId><LogicalResourceId>{lid}</LogicalResourceId>"
        f"<PhysicalResourceId>{x(p)}</PhysicalResourceId><ResourceType>{rtype}</ResourceType>"
        f"<ExpectedProperties>{{}}</ExpectedProperties><ActualProperties>{{}}</ActualProperties>"
        f"<StackResourceDriftStatus>{status}</StackResourceDriftStatus><PropertyDifferences>"
        + "".join(f"<member><PropertyPath>{path}</PropertyPath>"
                  + (f"<ExpectedValue>{x(e)}</ExpectedValue>" if e is not None else "")
                  + (f"<ActualValue>{x(a)}</ActualValue>" if a is not None else "")
                  + f"<DifferenceType>{kind}</DifferenceType></member>" for path, e, a, kind in diffs)
        + f"</PropertyDifferences><Timestamp>{cfn_iso(time.time())}</Timestamp></member>"
        for lid, rtype, p, status, diffs in cfn_resource_drifts(st))
    return f"<StackResourceDrifts>{members}</StackResourceDrifts>"


CFN_CHANGE_SETS = [("add-dlq", "CREATE_COMPLETE", "AVAILABLE", T0 - 1800, "Adds a dead-letter queue to orders"),
                   ("bump-memory", "CREATE_COMPLETE", "EXECUTE_COMPLETE", T0 - 3 * 86400, "")]


def cfn_change_sets(st, params):
    sets = CFN_CHANGE_SETS if st["name"] == "orders-api" else []
    members = "".join(
        f"<member><StackId>{cfn_id(st['name'])}</StackId><StackName>{st['name']}</StackName>"
        f"<ChangeSetId>arn:aws:cloudformation:us-east-1:123456789012:changeSet/{n}/1</ChangeSetId><ChangeSetName>{n}</ChangeSetName>"
        f"<ExecutionStatus>{ex}</ExecutionStatus><Status>{status}</Status><CreationTime>{cfn_iso(t)}</CreationTime>"
        + (f"<Description>{xml_escape(d)}</Description>" if d else "") + "</member>"
        for n, status, ex, t, d in sets)
    return f"<Summaries>{members}</Summaries>"


def cfn_change_set(st, params):
    x = lambda v: xml_escape(str(v))
    changes = [("Add", "OrdersDLQ", "AWS::SQS::Queue", None, "", []),
               ("Modify", "OrdersQueue", "AWS::SQS::Queue", "https://sqs.us-east-1.amazonaws.com/123456789012/orders", "False",
                [("Properties", "RedrivePolicy", "Never")]),
               ("Modify", "OrdersTable", "AWS::DynamoDB::Table", "orders", "True", [("Properties", "KeySchema", "Always")]),
               ("Modify", "OrdersFunction", "AWS::Lambda::Function", "orders-sync", "False",
                [("Properties", "Environment", "Never"), ("Properties", "Environment", "Never")])]
    if "bump-memory" in params.get("ChangeSetName", [""])[0]:
        changes = [("Modify", "OrdersFunction", "AWS::Lambda::Function", "orders-sync", "False", [("Properties", "MemorySize", "Never")])]
    members = "".join(
        f"<member><Type>Resource</Type><ResourceChange><Action>{a}</Action><LogicalResourceId>{lid}</LogicalResourceId>"
        + (f"<PhysicalResourceId>{x(p)}</PhysicalResourceId>" if p else "")
        + f"<ResourceType>{rtype}</ResourceType>" + (f"<Replacement>{rep}</Replacement>" if rep else "")
        + "<Scope>" + "".join(f"<member>{attr}</member>" for attr, _, _ in details) + "</Scope>"
        + "<Details>" + "".join(
            f"<member><Target><Attribute>{attr}</Attribute><Name>{name}</Name><RequiresRecreation>{rr}</RequiresRecreation></Target>"
            "<Evaluation>Static</Evaluation><ChangeSource>DirectModification</ChangeSource></member>"
            for attr, name, rr in details) + "</Details></ResourceChange></member>"
        for a, lid, rtype, p, rep, details in changes)
    return (f"<StackName>{st['name']}</StackName><Status>CREATE_COMPLETE</Status>"
            f"<ExecutionStatus>AVAILABLE</ExecutionStatus><Changes>{members}</Changes>")


def cfn_pipeline_events(stack, name):
    """The events of a pipeline's deploy stack, for its failure report."""
    start = timeline(PIPES[name])["Production"][1] or time.time()
    iso = lambda t: time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(t))
    events = [  # newest first, as CloudFormation returns them
        (start + 44, stack, "AWS::CloudFormation::Stack", "UPDATE_ROLLBACK_COMPLETE", ""),
        (start + 40, "WorkerFunction", "AWS::Lambda::Function", "UPDATE_COMPLETE", ""),
        (start + 30, stack, "AWS::CloudFormation::Stack", "UPDATE_ROLLBACK_IN_PROGRESS",
         "The following resource(s) failed to update: [WorkerFunction]."),
        (start + 29, "ApiRole", "AWS::IAM::Role", "UPDATE_FAILED", "Resource update cancelled"),
        (start + 28, "WorkerFunction", "AWS::Lambda::Function", "UPDATE_FAILED",
         'Resource handler returned message: "Unzipped size must be smaller than 262144000 bytes &amp; layers count less than 6 '
         '(Service: Lambda, Status Code: 400, Request ID: 7f3c)" (HandlerErrorCode: InvalidRequest)'),
        (start + 10, "OrdersQueue", "AWS::SQS::Queue", "UPDATE_COMPLETE", ""),
        (start + 2, stack, "AWS::CloudFormation::Stack", "UPDATE_IN_PROGRESS", "User Initiated"),
        (start - 86400, stack, "AWS::CloudFormation::Stack", "UPDATE_COMPLETE", ""),
    ]
    members = "".join(
        f"<member><StackName>{stack}</StackName><LogicalResourceId>{lid}</LogicalResourceId>"
        f"<ResourceType>{rtype}</ResourceType><Timestamp>{iso(t)}</Timestamp><ResourceStatus>{st}</ResourceStatus>"
        + (f"<ResourceStatusReason>{reason}</ResourceStatusReason>" if reason else "") + "</member>"
        for t, lid, rtype, st, reason in events)
    return 200, (f"<DescribeStackEventsResponse><DescribeStackEventsResult><StackEvents>{members}</StackEvents>"
                 "</DescribeStackEventsResult></DescribeStackEventsResponse>")


def codebuild(op, body):
    bid = body["ids"][0]
    name = bid.split("-build:")[0]
    st = timeline(PIPES[name])["Build"][0]
    status = {"InProgress": "IN_PROGRESS", "Failed": "FAILED"}.get(st, "SUCCEEDED")
    phases = [{"phaseType": t, "phaseStatus": "SUCCEEDED", "durationInSeconds": d}
              for t, d in [("SUBMITTED", 0), ("PROVISIONING", 8), ("DOWNLOAD_SOURCE", 3), ("INSTALL", 21)]]
    if status == "FAILED":
        phases.append({"phaseType": "BUILD", "phaseStatus": "FAILED", "durationInSeconds": 38, "contexts": [
            {"statusCode": "COMMAND_EXECUTION_ERROR", "message": "Error while executing command: npm test. Reason: exit status 1"}]})
    return {"builds": [{"id": bid, "buildStatus": status, "currentPhase": "BUILD" if status == "IN_PROGRESS" else "COMPLETED",
                        "phases": phases,
                        "logs": {"groupName": "/aws/codebuild/" + name, "streamName": bid.split(":")[1],
                                 "deepLink": "https://console.aws.amazon.com/cloudwatch/home?region=us-east-1#logEvent:group=/aws/codebuild/" + name}}]}


LOG = ["[Container] 2026/10/19 20:25:01 Running command npm ci",
       "added 812 packages in 14s", "\x1b[32m✓\x1b[0m lint passed",
       "[Container] 2026/10/19 20:25:20 Running command npm test",
       "PASS src/payments/retry.test.ts", "PASS src/orders/api.test.ts",
       "WARN deprecated uuid@8.3.2", "[Container] 2026/10/19 20:26:02 Running command npm run build",
       "dist/index.js  412.3kb", "Error: something failed in a test fixture (expected)"]


# A finished, failed build (worker-dev): install noise, a failing test in the middle, the
# test summary and CodeBuild's verdict at the end.
FAILED_LOG = (
    ["[Container] 2026/10/19 20:25:01.112 Running command npm ci"]
    + [f"npm http fetch GET 200 https://registry.npmjs.org/pkg-{i} {i % 90}ms (cache hit)" for i in range(400)]
    + ["added 812 packages, and audited 813 packages in 14s", "found 0 vulnerabilities",
       "[Container] 2026/10/19 20:25:20.410 Running command npm test", "> worker@3.4.1 test", "> jest --ci"]
    + [f"PASS src/jobs/job-{i}.test.ts" for i in range(30)]
    + ["FAIL src/queue/retry.test.ts", "  ● Queue › retries a failed job three times", "",
       "    expect(received).toBe(expected) // Object.is equality", "", "    Expected: 3", "    Received: 1", "",
       "      41 |     await queue.process(job);", "      42 |     await flush();", "    > 43 |     expect(job.attempts).toBe(3);",
       "         |                          ^", "      44 |   });", "",
       "      at Object.<anonymous> (src/queue/retry.test.ts:43:26)"]
    + [f"PASS src/api/route-{i}.test.ts" for i in range(150)]
    + ["", "Test Suites: 1 failed, 56 passed, 57 total", "Tests:       1 failed, 211 passed, 212 total",
       "Snapshots:   0 total", "Time:        31.4 s", "Ran all test suites.",
       "[Container] 2026/10/19 20:25:58.902 Command did not exit successfully npm test exit status 1",
       "[Container] 2026/10/19 20:25:58.907 Phase complete: BUILD State: FAILED",
       "[Container] 2026/10/19 20:25:58.907 Phase context status code: COMMAND_EXECUTION_ERROR Message: Error while executing command: npm test. Reason: exit status 1"]
)


GiB = 1024 ** 3
# Log group → (stored bytes, retention days or 0 for "never expire").
LOG_STORAGE = {"/ecs/orders-api": (int(1300 * GiB), 0), "/aws/lambda/report-builder": (int(180 * GiB), 0),
               "/aws/codebuild/app-dev": (int(0.4 * GiB), 0), "/aws/lambda/cron-cleanup": (int(0.2 * GiB), 0)}


def log_groups():
    return [f"/aws/lambda/{f['FunctionName']}" for f in FUNCS] + [
        "/aws/codebuild/app-dev", "/ecs/orders-api", "API-Gateway-Execution-Logs_a1b2c3/prod"]
LAMBDA_LINES = [
    "START RequestId: {rid} Version: $LATEST",
    "INFO processing order {n}",
    "INFO order {n} charged in 182 ms",
    "WARN retrying payment provider (attempt 2)",
    "ERROR Unhandled exception: TimeoutError: payment provider did not answer in 3000 ms\n    at charge (/var/task/payments.js:42:11)\n    at handler (/var/task/index.js:17:5)",
    "END RequestId: {rid}",
    "REPORT RequestId: {rid}  Duration: 211.40 ms  Billed Duration: 212 ms  Memory Size: {mem} MB  Max Memory Used: {used} MB",
]


ORDERS_SYNC_LINES = [
    "START RequestId: {uuid} Version: $LATEST",
    "[INFO]\t2026-10-21T10:00:00.000Z\t{uuid}\tsyncing {n} orders",
    "[ERROR]\t2026-10-21T10:00:01.000Z\t{uuid}\tTraceback (most recent call last):\n  File \"/var/task/app.py\", line 41, in handler\n    rows = db.fetch_pending()\n  File \"/var/task/db.py\", line 12, in fetch_pending\n    return conn.execute(SQL)\nConnectionError: orders-db.internal:5432 refused the connection",
    "END RequestId: {uuid}",
    "REPORT RequestId: {uuid}\tDuration: 1840.22 ms\tBilled Duration: 1841 ms\tMemory Size: 1024 MB\tMax Memory Used: 402 MB\tInit Duration: 612.40 ms",
    "START RequestId: {uuid} Version: $LATEST",
    "2026-10-21T10:05:00.031Z {uuid} Task timed out after 300.03 seconds",
    "END RequestId: {uuid}",
    "REPORT RequestId: {uuid}\tDuration: 300030.00 ms\tBilled Duration: 300000 ms\tMemory Size: 1024 MB\tMax Memory Used: 988 MB\tStatus: timeout",
]
PAYMENTS_LINES = [
    "START RequestId: {uuid} Version: $LATEST",
    "2026-10-21T10:00:00.000Z\t{uuid}\tINFO\tprocessing order {n}",
    "2026-10-21T10:00:00.182Z\t{uuid}\tINFO\torder {n} charged in 182 ms",
    "END RequestId: {uuid}",
    "REPORT RequestId: {uuid}\tDuration: 211.40 ms\tBilled Duration: 212 ms\tMemory Size: 512 MB\tMax Memory Used: 96 MB",
    "START RequestId: {uuid} Version: $LATEST",
    "2026-10-21T10:00:03.010Z\t{uuid}\tERROR\tInvoke Error \t{{\"errorType\":\"TimeoutError\",\"errorMessage\":\"payment provider did not answer in 3000 ms\",\"stack\":[\"TimeoutError: payment provider did not answer in 3000 ms\",\"    at charge (/var/task/payments.js:42:11)\",\"    at handler (/var/task/index.js:17:5)\"]}}",
    "END RequestId: {uuid}",
    "REPORT RequestId: {uuid}\tDuration: 3012.88 ms\tBilled Duration: 3013 ms\tMemory Size: 512 MB\tMax Memory Used: 101 MB\tInit Duration: 233.10 ms",
]
LINES_BY_GROUP = {"/aws/lambda/orders-sync": ORDERS_SYNC_LINES, "/aws/lambda/payments-webhook": PAYMENTS_LINES}


def synthetic_events(group, start_ms, pattern):
    """An event every 7 s for the last 2 hours, the same on every call (by timestamp)."""
    now_ms = int(time.time() * 1000)
    first = max(start_ms, now_ms - 2 * 3600 * 1000)
    t = first - first % 7000 + 7000
    terms = [w.strip('"?') for w in pattern.split()] if pattern else []
    out = []
    while t <= now_ms:
        k = t // 7000
        lines = LINES_BY_GROUP.get(group, LAMBDA_LINES)
        # One request id per invocation (the lines from START to REPORT).
        uuid_ = f"{(k // len(lines)) * 2654435761 % 2**32:08x}-2b7a-4c55-9a0e-1f2d3c4b{(k // len(lines)) % 9999:04d}"
        fn = group.rsplit("/", 1)[-1]
        mem = next((f["MemorySize"] for f in FUNCS if f["FunctionName"] == fn), 512)
        used = round(LAMBDA_MEMORY_USED.get(fn, 96) * (0.85 + 0.15 * (k * 7919 % 100) / 99))
        line = lines[k % len(lines)].format(rid=f"{k:08x}-7f3c", n=1000 + k % 977, uuid=uuid_, mem=mem, used=used)
        if not terms or any(term.lower() in line.lower() for term in terms):
            out.append({"logStreamName": f"2026/10/20/[$LATEST]{k % 3:04x}", "timestamp": t, "message": line + "\n",
                        "ingestionTime": t + 300, "eventId": f"{k}-{group}"})
        t += 7000
    return out


INSIGHTS_SAVED = [
    {"queryDefinitionId": "q-1", "name": "Errors/Recent errors",
     "queryString": "fields @timestamp, @message\n| filter @message like /ERROR/\n| sort @timestamp desc\n| limit 50",
     "logGroupNames": ["/aws/lambda/payments-webhook"], "lastModified": int(T0 * 1000)},
    {"queryDefinitionId": "q-2", "name": "Errors/Errors per 5 minutes",
     "queryString": "filter @message like /ERROR/\n| stats count(*) by bin(5m)", "logGroupNames": [], "lastModified": int(T0 * 1000)},
    {"queryDefinitionId": "q-3", "name": "Slowest invocations",
     "queryString": "filter @type = \"REPORT\"\n| fields @timestamp, @requestId, @duration\n| sort @duration desc\n| limit 20",
     "logGroupNames": ["/aws/lambda/orders-sync", "/aws/lambda/payments-webhook"], "lastModified": int(T0 * 1000)},
]
INSIGHTS_QUERIES = {}
INSIGHTS_RECORDS = {}


def insights_ts(ms):
    return time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(ms // 1000)) + f".{ms % 1000:03d}"


def insights_record(group, e):
    """Every field Insights knows of an event (what GetLogRecord returns)."""
    message = e["message"].rstrip("\n")
    record = {"@timestamp": insights_ts(e["timestamp"]), "@message": message, "@logStream": e["logStreamName"],
              "@log": f"123456789012:{group}", "@ingestionTime": insights_ts(e["ingestionTime"])}
    rid = re.search(r"RequestId: (\S+)", message)
    if rid:
        record["@requestId"] = rid[1]
    if message.startswith("REPORT"):
        record["@type"] = "REPORT"
        for field, label in [("@duration", "Duration"), ("@billedDuration", "Billed Duration"), ("@memorySize", "Memory Size"),
                             ("@maxMemoryUsed", "Max Memory Used")]:
            value = re.search(label + r": ([\d.]+)", message)
            if value:
                record[field] = value[1]
    return record


def insights_run(q):
    """Rows of a query, from the synthetic events of its groups. Understands a little of the
    language: `filter @message like /x/`, `filter @type = "REPORT"`, `fields a, b`, `sort f desc`,
    `limit N` and `stats count(*) by bin(Nm)`."""
    query = q["queryString"]
    events = [(g, e) for g in q["logGroupNames"] for e in synthetic_events(g, q["startTime"] * 1000, "")
              if e["timestamp"] <= q["endTime"] * 1000]
    scanned = len(events)
    records = [(g, e, insights_record(g, e)) for g, e in events]
    for pattern in re.findall(r"filter\s+@message\s+like\s+/([^/]*)/", query):
        records = [r for r in records if re.search(pattern, r[2]["@message"], re.I)]
    if re.search(r'filter\s+@type\s*=\s*"REPORT"', query):
        records = [r for r in records if r[2].get("@type") == "REPORT"]
    by_log = re.search(r"stats\s+(.+?)\s+by\s+@log", query, re.S)
    if by_log:
        groups = {}
        for g, _, record in records:
            groups.setdefault(g, []).append(record)
        rows = []
        for g, recs in sorted(groups.items()):
            row = [{"field": "@log", "value": f"123456789012:{g}"}]
            for fn, field, arg, alias in re.findall(r"(count|max|min|avg|sum|pct)\((\*|@\w+)(?:,\s*(\d+))?\)\s+as\s+(\w+)", by_log[1]):
                values = sorted(float(r[field]) for r in recs if field in r) if field != "*" else []
                if fn == "count":
                    v = len(recs)
                elif not values:
                    continue
                elif fn == "pct":
                    v = values[min(len(values) - 1, int(len(values) * int(arg) / 100))]
                else:
                    v = {"max": max, "min": min, "sum": sum, "avg": lambda xs: sum(xs) / len(xs)}[fn](values)
                row.append({"field": alias, "value": str(v)})
            rows.append(row)
        return rows, len(records), scanned
    stats = re.search(r"stats\s+count\(\*\)\s+by\s+bin\((\d+)([smh])\)", query)
    if stats:
        size = int(stats[1]) * {"s": 1, "m": 60, "h": 3600}[stats[2]]
        bins = {}
        for _, e, _ in records:
            b = e["timestamp"] // 1000 // size * size
            bins[b] = bins.get(b, 0) + 1
        label = f"bin({stats[1]}{stats[2]})"
        rows = [[{"field": label, "value": insights_ts(b * 1000)}, {"field": "count(*)", "value": str(n)}] for b, n in sorted(bins.items())]
        return rows, len(records), scanned
    sort = re.search(r"sort\s+(@\w+)\s*(desc|asc)?", query)
    key = sort[1] if sort else "@timestamp"
    numeric = key in ("@duration", "@billedDuration", "@maxMemoryUsed")
    records.sort(key=lambda r: float(r[2].get(key, 0)) if numeric else r[2].get(key, ""), reverse=not sort or sort[2] != "asc")
    limit = re.search(r"limit\s+(\d+)", query)
    limit = int(limit[1]) if limit else q.get("limit") or 1000
    fields = re.search(r"fields\s+([^|]+)", query)
    fields = [f.strip() for f in fields[1].split(",")] if fields else ["@timestamp", "@message"]
    rows = []
    for g, e, record in records[:limit]:
        ptr = f"CmAKJgoiMTIzNDU2Nzg5MDEy{len(INSIGHTS_RECORDS)}"
        INSIGHTS_RECORDS[ptr] = record
        rows.append([{"field": f, "value": record[f]} for f in fields if f in record] + [{"field": "@ptr", "value": ptr}])
    return rows, len(records), scanned


def insights(op, body):
    if op == "StartQuery":
        missing = [g for g in body.get("logGroupNames", []) if g not in log_groups()]
        if missing or not body.get("logGroupNames"):
            raise KeyError(f"ResourceNotFoundException: Log group '{(missing or ['(none)'])[0]}' does not exist")
        if "|" in body["queryString"] and body["queryString"].strip().startswith("|"):
            raise KeyError("MalformedQueryException: unexpected symbol found | at line 1 and position 1")
        qid = uuid.uuid4().hex
        INSIGHTS_QUERIES[qid] = {**body, "polls": 0, "status": "Running"}
        return {"queryId": qid}
    if op == "GetQueryResults":
        q = INSIGHTS_QUERIES.get(body["queryId"])
        if q is None:
            raise KeyError(f"ResourceNotFoundException: query {body['queryId']} does not exist")
        rows, matched, scanned = insights_run(q)
        q["polls"] += 1
        if q["status"] == "Running" and q["polls"] >= 2:
            q["status"] = "Complete"
        if q["status"] == "Running":
            rows, matched, scanned = rows[:len(rows) // 2], matched // 2, scanned // 2
        return {"status": q["status"], "results": rows,
                "statistics": {"recordsMatched": float(matched), "recordsScanned": float(scanned), "bytesScanned": float(scanned * 183)}}
    if op == "StopQuery":
        q = INSIGHTS_QUERIES.get(body["queryId"])
        if q is None or q["status"] != "Running":
            raise KeyError("InvalidParameterException: the query is not running")
        q["status"] = "Cancelled"
        return {"success": True}
    if op == "DescribeQueryDefinitions":
        return {"queryDefinitions": INSIGHTS_SAVED}
    if op == "GetLogRecord":
        record = INSIGHTS_RECORDS.get(body["logRecordPointer"])
        if record is None:
            raise KeyError("InvalidParameterException: unknown log record pointer")
        return {"logRecord": record}
    raise KeyError(op)


def cwlogs(op, body):
    if op in ("StartQuery", "GetQueryResults", "StopQuery", "DescribeQueryDefinitions", "GetLogRecord"):
        return insights(op, body)
    if op == "DescribeLogGroups":
        prefix = body.get("logGroupNamePrefix", "")
        groups = [g for g in log_groups() if g.startswith(prefix)]
        out = []
        for i, g in enumerate(groups):
            stored, retention = LOG_STORAGE.get(g, (1_000_000 * (i + 1), 30))
            out.append({"logGroupName": g, "storedBytes": stored, "creationTime": int((T0 - 86400 * 300) * 1000),
                        **({"retentionInDays": retention} if retention else {})})
        return {"logGroups": out}
    if op == "PutRetentionPolicy":
        group = body["logGroupName"]
        if group not in log_groups():
            raise KeyError(f"ResourceNotFoundException: The specified log group does not exist: {group}")
        if body.get("retentionInDays") not in (1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192,
                                               2557, 2922, 3288, 3653):
            raise KeyError(f"InvalidParameterException: retentionInDays {body.get('retentionInDays')} is not allowed")
        LOG_STORAGE[group] = (LOG_STORAGE.get(group, (1_000_000, 0))[0], body["retentionInDays"])
        return {}
    if op == "FilterLogEvents":
        group = body["logGroupName"]
        if group not in log_groups():
            raise KeyError(f"ResourceNotFoundException: log group {group} does not exist")
        events = synthetic_events(group, body.get("startTime", 0), body.get("filterPattern", ""))
        offset = int(body.get("nextToken") or 0)
        limit = body.get("limit", 1000)
        page = events[offset:offset + limit]
        out = {"events": page, "searchedLogStreams": []}
        if offset + limit < len(events):
            out["nextToken"] = str(offset + limit)
        return out
    if body.get("logGroupName", "").endswith("/worker-dev"):
        start = int((body.get("nextToken") or "f/0").split("/")[1])
        end = min(len(FAILED_LOG), start + 1000)
        events = [{"timestamp": int(T0 * 1000), "message": line} for line in FAILED_LOG[start:end]]
        return {"events": events, "nextForwardToken": f"f/{end}"}
    start = int((body.get("nextToken") or "f/0").split("/")[1])
    avail = min(400, int((time.time() - T0) * 3) + 25)
    end = min(avail, start + 50)
    events = [{"timestamp": int(time.time() * 1000), "message": LOG[i % len(LOG)] + f"  (line {i})"} for i in range(start, end)]
    return {"events": events, "nextForwardToken": f"f/{end}"}


PARAMS = []
for app in ["payments", "orders", "users"]:
    for env in ["dev", "prod"]:
        PARAMS += [
            {"Name": f"/{app}/{env}/db/url", "Type": "String", "Value": f"postgres://{app}-{env}.internal:5432/{app}"},
            {"Name": f"/{app}/{env}/db/password", "Type": "SecureString", "Value": "s3cr3t-" + app, "KeyId": "alias/aws/ssm"},
            {"Name": f"/{app}/{env}/feature-flags", "Type": "String", "Value": json.dumps({"newCheckout": True, "retries": 3, "regions": ["us-east-1", "eu-west-1"]})},
            {"Name": f"/{app}/{env}/allowed-ips", "Type": "StringList", "Value": "10.0.0.1,10.0.0.2"},
        ]
PARAMS.append({"Name": "legacy-api-key", "Type": "SecureString", "Value": "abc123", "KeyId": "alias/aws/ssm"})


def ssm_meta(p):
    return {"Name": p["Name"], "Type": p["Type"], "Version": p.get("Version", 3), "Tier": "Standard",
            "LastModifiedDate": p.get("Modified", time.time() - 86400 * 3), "DataType": "text",
            "LastModifiedUser": "arn:aws:sts::123456789012:assumed-role/Admin/dev-user",
            **({"Description": p["Description"]} if p.get("Description") else {}),
            **({"KeyId": p["KeyId"]} if "KeyId" in p else {})}


def ssm(op, body):
    if op == "DescribeParameters" and body.get("ParameterFilters"):
        names = body["ParameterFilters"][0]["Values"]
        return {"Parameters": [ssm_meta(p) for p in PARAMS if p["Name"] in names]}
    if op == "PutParameter":
        existing = next((x for x in PARAMS if x["Name"] == body["Name"]), None)
        if existing and not body.get("Overwrite"):
            raise KeyError("ParameterAlreadyExists")
        if existing is None:
            existing = {"Name": body["Name"], "Version": 0}
            PARAMS.append(existing)
        existing.update({"Type": body["Type"], "Value": body["Value"], "Version": existing.get("Version", 3) + 1,
                         "Modified": time.time(), "Description": body.get("Description", existing.get("Description"))})
        if body["Type"] == "SecureString":
            existing["KeyId"] = body.get("KeyId", existing.get("KeyId", "alias/aws/ssm"))
        return {"Version": existing["Version"], "Tier": "Standard"}
    if op == "DescribeParameters":
        start = int(body.get("NextToken") or 0)
        page = PARAMS[start:start + 7]
        time.sleep(0.15)
        out = {"Parameters": [ssm_meta(p) for p in page]}
        if start + 7 < len(PARAMS):
            out["NextToken"] = str(start + 7)
        return out
    if op == "GetParameter":
        p = next(x for x in PARAMS if x["Name"] == body["Name"])
        value = p["Value"] if (p["Type"] != "SecureString" or body.get("WithDecryption")) else "AQICAHh...ciphertext"
        return {"Parameter": {"Name": p["Name"], "Type": p["Type"], "Value": value, "Version": p.get("Version", 3)}}
    raise KeyError(op)


SECRETS = [
    {"Name": n, "Description": d, "Value": v, "Created": time.time() - 86400 * 40, "Changed": time.time() - 86400 * k,
     "Tags": [{"Key": "team", "Value": "platform"}], "Versions": 2}
    for k, (n, d, v) in enumerate([
        ("prod/payments/stripe", "Stripe API keys", json.dumps({"publishable": "pk_live_abc", "secret": "sk_live_xyz"})),
        ("prod/orders/db", "Orders database credentials", json.dumps({"username": "orders", "password": "s3cr3t", "host": "orders.cluster.internal", "port": 5432})),
        ("dev/payments/stripe", "Stripe test keys", json.dumps({"publishable": "pk_test_abc", "secret": "sk_test_xyz"})),
        ("shared/slack-webhook", "", "https://hooks.slack.com/services/T000/B000/XXXX"),
    ], start=1)]
SECRETS.append({"Name": "legacy/partner-api", "Description": "Partner API key (old integration)", "Value": json.dumps({"key": "pk_old"}),
                "Created": time.time() - 86400 * 400, "Changed": time.time() - 86400 * 380, "Accessed": time.time() - 86400 * 140,
                "Tags": [], "Versions": 1})


def secret_meta(sec):
    return {"ARN": f"arn:aws:secretsmanager:us-east-1:123456789012:secret:{sec['Name']}-AbCdEf", "Name": sec["Name"],
            **({"Description": sec["Description"]} if sec["Description"] else {}),
            "LastChangedDate": sec["Changed"], "LastAccessedDate": sec.get("Accessed", time.time() - 86400), "CreatedDate": sec["Created"],
            "RotationEnabled": False, "Tags": sec["Tags"]}


def secretsmanager(op, body):
    find = lambda: next((x for x in SECRETS if x["Name"] == body.get("SecretId") or secret_meta(x)["ARN"] == body.get("SecretId")), None)
    if op == "ListSecrets":
        return {"SecretList": [secret_meta(x) for x in SECRETS]}
    if op in ("DescribeSecret", "GetSecretValue", "PutSecretValue"):
        sec = find()
        if sec is None:
            raise KeyError("ResourceNotFoundException")
        if op == "DescribeSecret":
            return secret_meta(sec)
        if op == "GetSecretValue":
            return {**{k: secret_meta(sec)[k] for k in ("ARN", "Name")}, "SecretString": sec["Value"],
                    "VersionId": f"v{sec['Versions']}", "VersionStages": ["AWSCURRENT"], "CreatedDate": sec["Changed"]}
        assert body.get("ClientRequestToken"), "ClientRequestToken required"
        sec.update({"Value": body["SecretString"], "Changed": time.time(), "Versions": sec["Versions"] + 1})
        return {"ARN": secret_meta(sec)["ARN"], "Name": sec["Name"], "VersionId": f"v{sec['Versions']}"}
    if op == "CreateSecret":
        assert body.get("ClientRequestToken"), "ClientRequestToken required"
        if any(x["Name"] == body["Name"] for x in SECRETS):
            raise KeyError("ResourceExistsException")
        sec = {"Name": body["Name"], "Description": body.get("Description", ""), "Value": body["SecretString"],
               "Created": time.time(), "Changed": time.time(), "Tags": [], "Versions": 1}
        SECRETS.append(sec)
        return {"ARN": secret_meta(sec)["ARN"], "Name": sec["Name"], "VersionId": "v1"}
    raise KeyError(op)


FUNCS = [{"FunctionName": n, "FunctionArn": f"arn:aws:lambda:us-east-1:123456789012:function:{n}", "Runtime": r,
          "MemorySize": m, "Timeout": t, "Handler": "index.handler", "CodeSize": 1_400_000 + i * 1000,
          "LastModified": "2026-10-1%dT10:00:00.000+0000" % (i % 9), "Architectures": ["arm64"], "PackageType": "Zip",
          "Role": "arn:aws:iam::123456789012:role/service-role/" + n + "-role", "State": "Active",
          "Environment": {"Variables": {"DB_URL": "postgres://db", "API_KEY": "sk_live_123", "LOG_LEVEL": "info"}},
          "Layers": [{"Arn": "arn:aws:lambda:us-east-1:123456789012:layer:shared-utils:7", "CodeSize": 52000}],
          "LoggingConfig": {"LogFormat": "JSON", "LogGroup": "/aws/lambda/" + n},
          "VpcConfig": {"VpcId": "vpc-0abc", "SubnetIds": ["s1", "s2"], "SecurityGroupIds": ["sg1"]},
          "EphemeralStorage": {"Size": 512}, "TracingConfig": {"Mode": "Active"}}
         for i, (n, r, m, t) in enumerate([("payments-webhook", "nodejs22.x", 512, 30), ("orders-sync", "python3.13", 1024, 300),
                                           ("users-auth", "nodejs22.x", 256, 10), ("image-resizer", "provided.al2023", 2048, 60),
                                           ("report-builder", "java21", 3008, 900), ("cron-cleanup", "python3.13", 128, 120)])]


FUNCS[0]["DeadLetterConfig"] = {"TargetArn": "arn:aws:sqs:us-east-1:123456789012:payments-dlq"}
# x86 functions (arm64 candidates for the Savings checks) and the memory each one uses (MB).
for _f in FUNCS:
    if _f["FunctionName"] in ("report-builder", "orders-sync", "cron-cleanup"):
        _f["Architectures"] = ["x86_64"]
LAMBDA_MEMORY_USED = {"report-builder": 410, "users-auth": 70, "image-resizer": 1730, "cron-cleanup": 61}
# Provisioned concurrency: function → [(qualifier, allocated)].
LAMBDA_PROVISIONED = {"payments-webhook": [("live", 50)]}


def lambda_provisioned_concurrency(path):
    """GET /2019-09-30/functions/{name}/provisioned-concurrency?List=ALL: (status, body)."""
    name = [p for p in path.split("?")[0].split("/") if p][2]
    if not any(f["FunctionName"] == name for f in FUNCS):
        return 404, {"Type": "User", "message": f"Function not found: {name}"}
    arn = f"arn:aws:lambda:us-east-1:123456789012:function:{name}"
    return 200, {"ProvisionedConcurrencyConfigs": [
        {"FunctionArn": f"{arn}:{q}", "RequestedProvisionedConcurrentExecutions": n, "AvailableProvisionedConcurrentExecutions": n,
         "AllocatedProvisionedConcurrentExecutions": n, "Status": "READY", "LastModified": "2026-09-01T10:00:00+0000"}
        for q, n in LAMBDA_PROVISIONED.get(name, [])]}
EVENT_INVOKE_CONFIG = {"payments-webhook": {
    "FunctionArn": FUNCS[0]["FunctionArn"], "MaximumRetryAttempts": 2, "MaximumEventAgeInSeconds": 3600,
    "DestinationConfig": {"OnFailure": {"Destination": "arn:aws:sqs:us-east-1:123456789012:payments-failures"}, "OnSuccess": {}}}}


def lambda_event_invoke_config(path):
    """GET /2019-09-25/functions/{name}/event-invoke-config: (status, body)."""
    name = [p for p in path.split("?")[0].split("/") if p][2]
    if name not in EVENT_INVOKE_CONFIG:
        return 404, {"Type": "User", "message": f"The function {name} doesn't have an EventInvokeConfig"}
    return 200, EVENT_INVOKE_CONFIG[name]


def lambda_invoke(path, headers, raw):
    """POST /2015-03-31/functions/{name}/invocations: (status, headers, body). payments-webhook
    fails when the payload asks for it ({"fail": true})."""
    name = [p for p in path.split("?")[0].split("/") if p][2]
    if not any(f["FunctionName"] == name for f in FUNCS):
        return 404, {"content-type": "application/json", "x-amzn-errortype": "ResourceNotFoundException"}, json.dumps(
            {"Type": "User", "message": f"Function not found: {name}"}).encode()
    try:
        payload = json.loads(raw or b"{}")
    except ValueError:
        return 400, {"content-type": "application/json", "x-amzn-errortype": "InvalidRequestContentException"}, json.dumps(
            {"Type": "User", "message": "Could not parse request body into json"}).encode()
    rid = str(uuid.uuid4())
    failing = name == "payments-webhook" and isinstance(payload, dict) and payload.get("fail")
    log = [f"START RequestId: {rid} Version: $LATEST", f"2026-10-21T10:00:00.000Z\t{rid}\tINFO\tgot {json.dumps(payload)[:200]}"]
    out_headers = {"content-type": "application/json", "x-amz-executed-version": "$LATEST"}
    if failing:
        error = {"errorType": "TimeoutError", "errorMessage": "payment provider did not answer in 3000 ms",
                 "trace": ["TimeoutError: payment provider did not answer in 3000 ms", "    at charge (/var/task/payments.js:42:11)"]}
        log.append(f"2026-10-21T10:00:03.010Z\t{rid}\tERROR\tInvoke Error \t{json.dumps(error)}")
        body = error
        out_headers["x-amz-function-error"] = "Unhandled"
    else:
        body = {"statusCode": 200, "body": json.dumps({"ok": True, "function": name, "received": payload})}
    log += [f"END RequestId: {rid}", f"REPORT RequestId: {rid}\tDuration: 182.33 ms\tBilled Duration: 183 ms\tMemory Size: 512 MB\tMax Memory Used: 88 MB"]
    if headers.get("x-amz-log-type") == "Tail":
        import base64
        out_headers["x-amz-log-result"] = base64.b64encode("\n".join(log).encode()[-4096:]).decode()
    return 200, out_headers, json.dumps(body).encode()


def lambda_rest(path):
    parts = [p for p in path.split("?")[0].split("/") if p]
    if parts == ["2015-03-31", "functions"]:
        # Like the real API, ListFunctions sends absent fields as explicit nulls.
        nulls = {"Layers": None, "State": None, "StateReason": None, "KMSKeyArn": None, "MasterArn": None,
                 "FileSystemConfigs": None, "SigningProfileVersionArn": None, "RuntimeVersionConfig": None}
        return {"Functions": [{**f, **nulls, **({"Environment": None} if i % 2 else {})} for i, f in enumerate(FUNCS)],
                "NextMarker": None}
    name = parts[2]
    f = next(x for x in FUNCS if x["FunctionName"] == name)
    return {"Configuration": f, "Tags": {"team": "platform", "service": name.split("-")[0]},
            "Concurrency": {"ReservedConcurrentExecutions": 50}}


# ---- S3 (restXml, path-style) --------------------------------------------------------------
import gzip  # noqa: E402  (S3 mock data)
import re  # noqa: E402
from email.utils import formatdate  # noqa: E402
from urllib.parse import unquote  # noqa: E402
from xml.sax.saxutils import escape as xml_escape  # noqa: E402


def s3_obj(body, ctype="text/plain", meta=None, storage="STANDARD", age=86400.0):
    data = body if isinstance(body, bytes) else body.encode()
    return {"body": data, "type": ctype, "meta": meta or {}, "storage": storage, "modified": time.time() - age}


# Bucket → region. acme-logs-archive gets no BucketRegion in ListBuckets, like older APIs,
# so dcli has to ask GetBucketLocation.
S3_BUCKETS = {"acme-app-assets": "us-east-1", "acme-reports.eu": "eu-west-1", "acme-logs-archive": "us-west-2"}
S3_OBJECTS = {
    "acme-app-assets": {
        "index.html": s3_obj("<!doctype html>\n<html><body><h1>Acme</h1></body></html>\n", "text/html"),
        "config/app.json": s3_obj(json.dumps({"api": "https://api.acme.dev", "retries": 3, "regions": ["us-east-1", "eu-west-1"]}),
                                  "application/json", {"owner": "platform", "deployed-by": "pipeline"}),
        "config/feature-flags.json": s3_obj(json.dumps({"newCheckout": True, "betaSearch": False}), "application/json"),
        "images/logo.png": s3_obj(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR" + bytes(range(256)) * 8, "image/png"),
        "docs/READ ME.md": s3_obj("# Acme assets\n\nStatic files served by the CDN.\n", "text/markdown", {"owner": "web"}),
        "exports/": s3_obj(b"", "application/x-directory"),
        **{f"exports/orders-{i:04}.csv": s3_obj(f"id,total\n{i},{i * 3.5:.2f}\n", "text/csv", age=3600.0 * (i % 48))
           for i in range(2500)},
    },
    "acme-reports.eu": {
        **{f"2026/10/sales-2026-10-{d:02}.csv": s3_obj("day,store,amount\n" + "".join(f"{d},store-{s},{s * 97 % 1000}\n" for s in range(40)),
                                                      "text/csv", storage="STANDARD_IA") for d in range(1, 20)},
        "2026/09/summary.json": s3_obj(json.dumps({"month": "2026-09", "total": 1234567.89}), "application/json"),
    },
    "acme-logs-archive": {
        "app/2026-10-19.log": s3_obj("".join(f"2026-10-19T{i // 3600 % 24:02}:{i // 60 % 60:02}:{i % 60:02}Z INFO request {i} handled in {i % 300} ms\n"
                                             for i in range(3000)), "text/plain", storage="GLACIER_IR"),
        "app/2026-10-18.log.gz": s3_obj(gzip.compress(b"compressed log line\n" * 200), "application/gzip"),
    },
}


# Lifecycle rules, versioning and abandoned multipart uploads per bucket (the Savings checks).
S3_LIFECYCLE = {
    "acme-reports.eu": "<Rule><ID>age</ID><Filter><Prefix></Prefix></Filter><Status>Enabled</Status>"
                       "<Transition><Days>30</Days><StorageClass>STANDARD_IA</StorageClass></Transition>"
                       "<AbortIncompleteMultipartUpload><DaysAfterInitiation>7</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule>",
    "acme-logs-archive": "<Rule><ID>expire</ID><Filter><Prefix>app/</Prefix></Filter><Status>Enabled</Status>"
                         "<Expiration><Days>400</Days></Expiration></Rule>",
}
S3_VERSIONING = {"acme-app-assets": "Enabled", "acme-logs-archive": "Suspended"}
S3_STALE_UPLOADS = {"acme-app-assets": 14}
# Bucket → {BucketSizeBytes storage type: bytes}.
S3_SIZES = {"acme-app-assets": {"StandardStorage": 620 * 1024 ** 3, "StandardIAStorage": 3 * 1024 ** 3},
            "acme-reports.eu": {"StandardStorage": 80 * 1024 ** 3, "StandardIAStorage": 300 * 1024 ** 3},
            "acme-logs-archive": {"GlacierInstantRetrievalStorage": 2048 * 1024 ** 3}}


def s3_error(status, code, message, extra=None):
    body = f"<Error><Code>{code}</Code><Message>{xml_escape(message)}</Message></Error>".encode()
    return status, {"content-type": "application/xml", **(extra or {})}, body


def s3_signed_region(query, headers):
    """The region the request was signed for (header or presigned query)."""
    cred = query.get("X-Amz-Credential") or headers.get("authorization", "")
    m = re.search(r"/\d{8}/([a-z0-9-]+)/s3/", cred)
    return m.group(1) if m else None


def s3_iso(t):
    return time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(t))


def s3_rest(method, path, query, headers):
    """S3 requests: GET/HEAD on /, /{bucket}, /{bucket}/{key}. Returns (status, headers, body bytes)."""
    xml = {"content-type": "application/xml"}
    bucket, _, key = unquote(path.lstrip("/")).partition("/")
    if not bucket:  # ListBuckets
        with_region = "max-buckets" in query
        items = "".join(
            f"<Bucket><Name>{b}</Name><CreationDate>{s3_iso(T0 - 86400 * 400)}</CreationDate>"
            + (f"<BucketRegion>{r}</BucketRegion>" if with_region and b != "acme-logs-archive" else "") + "</Bucket>"
            for b, r in S3_BUCKETS.items())
        return 200, xml, f"<ListAllMyBucketsResult><Owner><ID>abc</ID></Owner><Buckets>{items}</Buckets></ListAllMyBucketsResult>".encode()
    if bucket not in S3_BUCKETS:
        return s3_error(404, "NoSuchBucket", f"The specified bucket does not exist: {bucket}")
    region = S3_BUCKETS[bucket]
    if not key and "location" in query:  # GetBucketLocation: answered from any region
        value = {"us-east-1": "", "eu-west-1": "EU"}.get(region, region)
        ns = 'xmlns="http://s3.amazonaws.com/doc/2006-03-01/"'
        body = f"<LocationConstraint {ns}/>" if not value else f"<LocationConstraint {ns}>{value}</LocationConstraint>"
        return 200, xml, body.encode()
    if s3_signed_region(query, headers) not in (None, region):
        return s3_error(301, "PermanentRedirect", "The bucket you are attempting to access must be addressed using the specified endpoint.",
                        {"x-amz-bucket-region": region})
    objects = S3_OBJECTS[bucket]
    if not key and "lifecycle" in query:
        rules = S3_LIFECYCLE.get(bucket)
        if rules is None:
            return s3_error(404, "NoSuchLifecycleConfiguration", "The lifecycle configuration does not exist")
        return 200, xml, f"<LifecycleConfiguration>{rules}</LifecycleConfiguration>".encode()
    if not key and "versioning" in query:
        status = S3_VERSIONING.get(bucket)
        body = f"<VersioningConfiguration><Status>{status}</Status></VersioningConfiguration>" if status else "<VersioningConfiguration/>"
        return 200, xml, body.encode()
    if not key and "uploads" in query:
        uploads = "".join(f"<Upload><Key>exports/big-{i}.zip</Key><UploadId>u{i}</UploadId><Initiated>{s3_iso(T0 - 86400 * (10 + i))}</Initiated>"
                          f"<StorageClass>STANDARD</StorageClass></Upload>" for i in range(S3_STALE_UPLOADS.get(bucket, 0)))
        return 200, xml, f"<ListMultipartUploadsResult><Bucket>{bucket}</Bucket>{uploads}<IsTruncated>false</IsTruncated></ListMultipartUploadsResult>".encode()
    if not key:
        if method == "HEAD":  # HeadBucket
            return 200, {"x-amz-bucket-region": region}, b""
        if query.get("list-type") != "2":
            return s3_error(400, "InvalidRequest", "mock only supports ListObjectsV2")
        prefix, delim = query.get("prefix", ""), query.get("delimiter", "")
        max_keys = int(query.get("max-keys", "1000"))
        entries = []  # (sort key, kind, value)
        seen = set()
        for k in sorted(objects):
            if not k.startswith(prefix):
                continue
            rest = k[len(prefix):]
            if delim and delim in rest:
                folder = prefix + rest[:rest.index(delim) + len(delim)]
                if folder not in seen:
                    seen.add(folder)
                    entries.append((folder, "prefix", folder))
            else:
                entries.append((k, "key", k))
        entries.sort()
        start = int(query.get("continuation-token") or 0)
        page = entries[start:start + max_keys]
        truncated = start + max_keys < len(entries)
        out = [f"<ListBucketResult><Name>{bucket}</Name><Prefix>{xml_escape(prefix)}</Prefix><KeyCount>{len(page)}</KeyCount>",
               f"<MaxKeys>{max_keys}</MaxKeys><IsTruncated>{'true' if truncated else 'false'}</IsTruncated>"]
        if truncated:
            out.append(f"<NextContinuationToken>{start + max_keys}</NextContinuationToken>")
        for _, kind, value in page:
            if kind == "prefix":
                out.append(f"<CommonPrefixes><Prefix>{xml_escape(value)}</Prefix></CommonPrefixes>")
            else:
                o = objects[value]
                out.append(f"<Contents><Key>{xml_escape(value)}</Key><LastModified>{s3_iso(o['modified'])}</LastModified>"
                           f"<ETag>&quot;{uuid.uuid5(uuid.NAMESPACE_URL, value).hex}&quot;</ETag><Size>{len(o['body'])}</Size>"
                           f"<StorageClass>{o['storage']}</StorageClass></Contents>")
        out.append("</ListBucketResult>")
        return 200, xml, "".join(out).encode()
    o = objects.get(key)
    if o is None:
        return s3_error(404, "NoSuchKey", "The specified key does not exist.")
    head = {"content-type": o["type"], "last-modified": formatdate(o["modified"], usegmt=True),
            "etag": f'"{uuid.uuid5(uuid.NAMESPACE_URL, key).hex}"', "accept-ranges": "bytes",
            "x-amz-server-side-encryption": "AES256", **{f"x-amz-meta-{k}": v for k, v in o["meta"].items()}}
    if o["storage"] != "STANDARD":
        head["x-amz-storage-class"] = o["storage"]
    body = o["body"]
    if method == "HEAD":
        return 200, {**head, "content-length": str(len(body))}, b""
    m = re.match(r"bytes=(\d+)-(\d*)", headers.get("range", "") or "")
    if m:
        first = int(m.group(1))
        last = min(int(m.group(2)) if m.group(2) else len(body) - 1, len(body) - 1)
        part = body[first:last + 1]
        return 206, {**head, "content-range": f"bytes {first}-{first + len(part) - 1}/{len(body)}"}, part
    return 200, head, body


# ---- Step Functions (awsJson1_0) ------------------------------------------------------------
SFN_ARN = "arn:aws:states:us-east-1:123456789012"
SFN_MACHINES = [
    {"name": "order-fulfillment", "type": "STANDARD", "created": T0 - 86400 * 120, "logging": None,
     "description": "Validates, charges and ships an order"},
    {"name": "invoice-generation", "type": "STANDARD", "created": T0 - 86400 * 60, "logging": None,
     "description": "Monthly invoices (legacy Lambda tasks)"},
    {"name": "report-export", "type": "STANDARD", "created": T0 - 86400 * 45, "logging": None,
     "description": "Exports a report with Athena and waits for it"},
    {"name": "events-ingest", "type": "EXPRESS", "created": T0 - 86400 * 30,
     "logging": f"arn:aws:logs:us-east-1:123456789012:log-group:/aws/vendedlogs/states/events-ingest-Logs:*",
     "description": "High-volume event ingestion"},
]


def _fn(name):
    return f"arn:aws:lambda:us-east-1:123456789012:function:{name}"


def _invoke(name):
    return {"Type": "Task", "Resource": "arn:aws:states:::lambda:invoke",
            "Parameters": {"FunctionName": f"{_fn(name)}:live", "Payload.$": "$"}}


# Definitions: order-fulfillment has a Choice, a retry and a catch (optimized Lambda
# integration); invoice-generation a Map and a Parallel (legacy Lambda resources);
# report-export a polling loop.
SFN_DEFS = {
    "order-fulfillment": {"Comment": "Validates, charges and ships an order", "StartAt": "ValidateOrder", "States": {
        "ValidateOrder": {**_invoke("orders-sync"), "Next": "InStock?"},
        "InStock?": {"Type": "Choice", "Choices": [{"Variable": "$.inStock", "BooleanEquals": True, "Next": "ChargePayment"}],
                     "Default": "Backorder"},
        "Backorder": {"Type": "Task", "Resource": "arn:aws:states:::sqs:sendMessage",
                      "Parameters": {"QueueUrl": "https://sqs.us-east-1.amazonaws.com/123456789012/backorders", "MessageBody.$": "$"},
                      "Next": "NotifyCustomer"},
        "ChargePayment": {**_invoke("payments-webhook"), "Retry": [{"ErrorEquals": ["TimeoutError"], "MaxAttempts": 1}],
                          "Catch": [{"ErrorEquals": ["PaymentDeclined"], "Next": "PaymentDeclined"}], "Next": "ShipOrder"},
        "PaymentDeclined": {"Type": "Fail", "Error": "PaymentDeclined", "Cause": "The card was declined"},
        "ShipOrder": {**_invoke("report-builder"), "Next": "NotifyCustomer"},
        "NotifyCustomer": {"Type": "Pass", "End": True}}},
    "invoice-generation": {"StartAt": "CollectUsage", "States": {
        "CollectUsage": {"Type": "Task", "Resource": _fn("report-builder"), "Next": "RenderInvoices"},
        "RenderInvoices": {"Type": "Map", "ItemsPath": "$.customers", "MaxConcurrency": 3, "Next": "Notify",
                           "ItemProcessor": {"ProcessorConfig": {"Mode": "INLINE"}, "StartAt": "RenderPdf", "States": {
                               "RenderPdf": {"Type": "Task", "Resource": _fn("image-resizer"), "Next": "SendInvoice"},
                               "SendInvoice": {"Type": "Task", "Resource": _fn("users-auth"), "End": True}}}},
        "Notify": {"Type": "Parallel", "End": True, "Branches": [
            {"StartAt": "EmailFinance", "States": {"EmailFinance": {"Type": "Task", "Resource": "arn:aws:states:::sns:publish", "End": True}}},
            {"StartAt": "UpdateLedger", "States": {"UpdateLedger": {"Type": "Task", "Resource": "arn:aws:states:::dynamodb:putItem", "End": True}}}]}}},
    "report-export": {"StartAt": "StartExport", "States": {
        "StartExport": {"Type": "Task", "Resource": "arn:aws:states:::aws-sdk:athena:startQueryExecution", "Next": "WaitForExport"},
        "WaitForExport": {"Type": "Wait", "Seconds": 10, "Next": "GetStatus"},
        "GetStatus": {"Type": "Task", "Resource": "arn:aws:states:::aws-sdk:athena:getQueryExecution", "Next": "Done?"},
        "Done?": {"Type": "Choice", "Choices": [
            {"Variable": "$.state", "StringEquals": "RUNNING", "Next": "WaitForExport"},
            {"Variable": "$.state", "StringEquals": "FAILED", "Next": "ExportFailed"}], "Default": "Finish"},
        "ExportFailed": {"Type": "Fail", "Error": "ExportFailed"},
        "Finish": {"Type": "Succeed"}}},
    "events-ingest": {"StartAt": "Store", "States": {"Store": {"Type": "Task", "Resource": "arn:aws:states:::dynamodb:putItem", "End": True}}},
}
SFN_INPUT = {"orderId": "ORD-1042", "customerId": "C-77", "amount": 129.9, "currency": "USD", "inStock": True,
             "items": [{"sku": "SKU-1", "qty": 2}, {"sku": "SKU-9", "qty": 1}]}
SFN_CAUSE = json.dumps({"errorType": "TimeoutError", "errorMessage": "payment provider did not answer in 3000 ms",
                        "trace": ["TimeoutError: payment provider did not answer in 3000 ms",
                                  "    at charge (/var/task/payments.js:42:11)",
                                  "    at handler (/var/task/index.js:17:5)"]})
# scenario: success, fail, running, aborted; "started" ones (StartExecution) run for 20 s,
# then succeed.
SFN_EXECUTIONS = []
for _k, (_scenario, _ago) in enumerate([("running", 40), ("fail", 3600), ("success", 7200), ("backorder", 3 * 3600),
                                         ("aborted", 5 * 3600), ("fail", 86400), ("success", 2 * 86400)]):
    SFN_EXECUTIONS.append({"machine": "order-fulfillment", "name": f"ord-{1042 - _k}-{uuid.uuid4().hex[:6]}",
                           "scenario": _scenario, "start": time.time() - _ago, "redrives": 0,
                           "input": json.dumps({**SFN_INPUT, "orderId": f"ORD-{1042 - _k}",
                                                "inStock": _scenario != "backorder"})})
for _k, _scenario in enumerate(["success", "fail", "success"]):
    SFN_EXECUTIONS.append({"machine": "invoice-generation", "name": f"invoices-2026-{9 - _k:02d}",
                           "scenario": _scenario, "start": time.time() - 86400 * (1 + 30 * _k), "redrives": 0,
                           "input": json.dumps({"month": f"2026-{9 - _k:02d}", "customers": ["C-1", "C-2", "C-3", "C-4"]})})
for _k, (_scenario, _ago) in enumerate([("running", 25), ("success", 4 * 3600), ("fail", 30 * 3600)]):
    SFN_EXECUTIONS.append({"machine": "report-export", "name": f"export-{300 - _k}", "scenario": _scenario,
                           "start": time.time() - _ago, "redrives": 0, "input": json.dumps({"report": "sales", "month": "2026-09"})})


def sfn_exec_arn(e):
    return f"{SFN_ARN}:execution:{e['machine']}:{e['name']}"


def sfn_status(e):
    if e["scenario"] == "started":
        return "SUCCEEDED" if time.time() - e["start"] > 20 else "RUNNING"
    return {"success": "SUCCEEDED", "backorder": "SUCCEEDED", "fail": "FAILED", "running": "RUNNING",
            "aborted": "ABORTED", "redriven": "SUCCEEDED"}[e["scenario"]]


def sfn_script(e):
    """What an execution did, as a list of steps:
    ("task", name, lambda function or service:action, outcome), ("pass" | "choice", name),
    ("wait", name, seconds), ("succeed", name), ("map", name, [script per item]),
    ("parallel", name, [script per branch]). Outcomes: ok, fail (after one retry), running, abort."""
    scenario = e["scenario"]
    if scenario == "started":
        scenario = "success" if sfn_status(e) == "SUCCEEDED" else "running"
    machine = e["machine"]
    if machine == "order-fulfillment":
        head = [("task", "ValidateOrder", "orders-sync", "ok"), ("choice", "InStock?")]
        if scenario == "backorder":
            return head + [("task", "Backorder", "sqs:sendMessage", "ok"), ("pass", "NotifyCustomer")]
        charge = {"fail": "fail", "running": "running"}.get(scenario, "ok")
        script = head + [("task", "ChargePayment", "payments-webhook", charge)]
        if charge != "ok":
            return script
        ship = "abort" if scenario == "aborted" else "ok"
        script.append(("task", "ShipOrder", "report-builder", ship))
        return script if ship != "ok" else script + [("pass", "NotifyCustomer")]
    if machine == "invoice-generation":
        items = [[("task", "RenderPdf", "image-resizer", "ok"), ("task", "SendInvoice", "users-auth", "ok")] for _ in range(4)]
        if scenario == "fail":
            items[3][1] = ("task", "SendInvoice", "users-auth", "fail")
        script = [("task", "CollectUsage", "report-builder", "ok"), ("map", "RenderInvoices", items)]
        if scenario == "fail":
            return script
        return script + [("parallel", "Notify", [[("task", "EmailFinance", "sns:publish", "ok")],
                                                 [("task", "UpdateLedger", "dynamodb:putItem", "ok")]])]
    if machine == "report-export":
        polls = {"running": 1, "success": 3, "fail": 2}[scenario]
        script = [("task", "StartExport", "athena:startQueryExecution", "ok")]
        for i in range(polls):
            script += [("wait", "WaitForExport", 10)]
            if scenario == "running" and i == polls - 1:
                script[-1] = ("wait", "WaitForExport", "running")
                return script
            script += [("task", "GetStatus", "athena:getQueryExecution", "ok"), ("choice", "Done?")]
        return script + ([("fail", "ExportFailed")] if scenario == "fail" else [("succeed", "Finish")])
    return [("task", "Store", "dynamodb:putItem", "ok")]


def sfn_history(e):
    """Events of an execution, oldest first, linked by previousEventId like the real ones."""
    legacy = e["machine"] == "invoice-generation"
    events, clock = [], [e["start"]]

    def add(kind, prev, **details):
        events.append({"id": len(events) + 1, "previousEventId": prev, "timestamp": clock[0], "type": kind, **details})
        clock[0] += 0.05
        return len(events)

    def task(prev, name, target, outcome):
        entered = add("TaskStateEntered", prev, stateEnteredEventDetails={"name": name, "input": e["input"]})
        service = ":" in target
        attempts = 2 if outcome == "fail" else 1
        last = entered
        for attempt in range(attempts):
            if service:
                rtype, action = target.split(":", 1)
                last = add("TaskScheduled", last, taskScheduledEventDetails={"resourceType": rtype, "resource": action,
                                                                              "region": "us-east-1", "parameters": e["input"]})
            elif legacy:
                last = add("LambdaFunctionScheduled", last, lambdaFunctionScheduledEventDetails={"resource": _fn(target), "input": e["input"]})
                last = add("LambdaFunctionStarted", last)
            else:
                params = json.dumps({"FunctionName": f"{_fn(target)}:live", "Payload": json.loads(e["input"])})
                last = add("TaskScheduled", last, taskScheduledEventDetails={"resourceType": "lambda", "resource": "invoke",
                                                                              "region": "us-east-1", "parameters": params})
                last = add("TaskStarted", last, taskStartedEventDetails={"resourceType": "lambda", "resource": "invoke"})
            clock[0] += 1.5 + len(name) % 3
            if outcome == "running":
                return last, "running"
            if outcome == "abort":
                return last, "aborted"
            if outcome == "fail":
                failed = {"error": "TimeoutError", "cause": SFN_CAUSE}
                if legacy:
                    last = add("LambdaFunctionFailed", last, lambdaFunctionFailedEventDetails=failed)
                else:
                    last = add("TaskFailed", last, taskFailedEventDetails={"resourceType": "lambda", "resource": "invoke", **failed})
                if attempt == attempts - 1:
                    return last, "failed"
                clock[0] += 2
                continue
            ok = json.dumps({"ok": True, "state": name})
            if legacy and not service:
                last = add("LambdaFunctionSucceeded", last, lambdaFunctionSucceededEventDetails={"output": ok})
            else:
                last = add("TaskSucceeded", last, taskSucceededEventDetails={"resourceType": "lambda", "resource": "invoke", "output": ok})
        return add("TaskStateExited", last, stateExitedEventDetails={"name": name, "output": json.dumps({"ok": True})}), None

    def run(prev, script):
        for step in script:
            kind, name = step[0], step[1]
            if kind == "task":
                prev, stop = task(prev, name, step[2], step[3])
            elif kind in ("pass", "choice", "succeed"):
                state = kind.capitalize()
                entered = add(f"{state}StateEntered", prev, stateEnteredEventDetails={"name": name, "input": e["input"]})
                prev, stop = add(f"{state}StateExited", entered, stateExitedEventDetails={"name": name, "output": e["input"]}), None
            elif kind == "fail":
                return add("FailStateEntered", prev, stateEnteredEventDetails={"name": name, "input": e["input"]}), "failed"
            elif kind == "wait":
                entered = add("WaitStateEntered", prev, stateEnteredEventDetails={"name": name, "input": e["input"]})
                if step[2] == "running":
                    return entered, "running"
                clock[0] += step[2]
                prev, stop = add("WaitStateExited", entered, stateExitedEventDetails={"name": name, "output": e["input"]}), None
            else:  # map / parallel: items or branches overlap in time
                state = "Map" if kind == "map" else "Parallel"
                entered = add(f"{state}StateEntered", prev, stateEnteredEventDetails={"name": name, "input": e["input"]})
                started = add(f"{state}StateStarted", entered)
                base, end, stop = clock[0], clock[0], None
                for i, inner in enumerate(step[2]):
                    clock[0] = base + i * 0.8
                    head = add("MapIterationStarted", started, mapIterationStartedEventDetails={"name": name, "index": i}) if kind == "map" else started
                    last, stop = run(head, inner)
                    end = max(end, clock[0])
                    if kind == "map":
                        add("MapIterationFailed" if stop else "MapIterationSucceeded", last, mapIterationSucceededEventDetails={"name": name, "index": i})
                    if stop:
                        break
                clock[0] = end
                if stop:
                    return add(f"{state}StateFailed", started), stop
                prev = add(f"{state}StateExited", add(f"{state}StateSucceeded", started),
                           stateExitedEventDetails={"name": name, "output": json.dumps({"ok": True})})
            if stop:
                return prev, stop
        return prev, None

    add("ExecutionStarted", 0, executionStartedEventDetails={"input": e["input"], "roleArn": "arn:aws:iam::123456789012:role/sfn"})
    last, stop = run(1, sfn_script(e))
    if stop == "aborted":
        add("ExecutionAborted", last, executionAbortedEventDetails={"error": "Manual.Stop", "cause": "Stopped from the console"})
    elif stop == "failed":
        add("ExecutionFailed", last, executionFailedEventDetails={"error": "TimeoutError", "cause": SFN_CAUSE})
    elif stop is None:
        if e["scenario"] == "redriven":
            last = add("ExecutionRedriven", last, executionRedrivenEventDetails={"redriveCount": e["redrives"]})
        add("ExecutionSucceeded", last, executionSucceededEventDetails={"output": json.dumps({"status": "DONE"})})
    return events


def sfn_summary(e):
    status = sfn_status(e)
    hist = sfn_history(e)
    out = {"executionArn": sfn_exec_arn(e), "stateMachineArn": f"{SFN_ARN}:stateMachine:{e['machine']}",
           "name": e["name"], "status": status, "startDate": e["start"]}
    if status != "RUNNING":
        out["stopDate"] = hist[-1]["timestamp"]
    if e["redrives"]:
        out["redriveCount"] = e["redrives"]
    return out


def sfn_find_exec(arn):
    e = next((x for x in SFN_EXECUTIONS if sfn_exec_arn(x) == arn), None)
    if e is None:
        raise KeyError(f"ExecutionDoesNotExist: {arn}")
    return e


def stepfunctions(op, body):
    if op == "ListStateMachines":
        return {"stateMachines": [{"stateMachineArn": f"{SFN_ARN}:stateMachine:{m['name']}", "name": m["name"],
                                   "type": m["type"], "creationDate": m["created"]} for m in SFN_MACHINES]}
    if op == "DescribeStateMachine":
        name = body["stateMachineArn"].rsplit(":", 1)[-1]
        m = next((x for x in SFN_MACHINES if x["name"] == name), None)
        if m is None:
            raise KeyError(f"StateMachineDoesNotExist: {name}")
        out = {"stateMachineArn": body["stateMachineArn"], "name": name, "status": "ACTIVE", "type": m["type"],
               "roleArn": f"arn:aws:iam::123456789012:role/service-role/{name}-role", "creationDate": m["created"],
               "description": m["description"], "definition": json.dumps(SFN_DEFS[name], indent=2)}
        if m["logging"]:
            out["loggingConfiguration"] = {"level": "ALL", "includeExecutionData": True,
                                           "destinations": [{"cloudWatchLogsLogGroup": {"logGroupArn": m["logging"]}}]}
        else:
            out["loggingConfiguration"] = {"level": "OFF", "includeExecutionData": False}
        return out
    if op == "ListExecutions":
        name = body["stateMachineArn"].rsplit(":", 1)[-1]
        if next((x for x in SFN_MACHINES if x["name"] == name), {}).get("type") == "EXPRESS":
            raise KeyError("StateMachineTypeNotSupported: ListExecutions does not support EXPRESS state machines")
        items = sorted((sfn_summary(e) for e in SFN_EXECUTIONS if e["machine"] == name), key=lambda x: -x["startDate"])
        if body.get("statusFilter"):
            items = [x for x in items if x["status"] == body["statusFilter"]]
        start = int(body.get("nextToken") or 0)
        size = body.get("maxResults") or 100
        out = {"executions": items[start:start + size]}
        if start + size < len(items):
            out["nextToken"] = str(start + size)
        return out
    if op == "DescribeStateMachineForExecution":
        e = sfn_find_exec(body["executionArn"])
        return {"stateMachineArn": f"{SFN_ARN}:stateMachine:{e['machine']}", "name": e["machine"],
                "definition": json.dumps(SFN_DEFS[e["machine"]], indent=2), "updateDate": T0 - 86400}
    if op == "DescribeExecution":
        e = sfn_find_exec(body["executionArn"])
        out = sfn_summary(e)
        out["input"] = e["input"]
        status = out["status"]
        if status == "SUCCEEDED":
            out["output"] = json.dumps({"status": "DONE", "orderId": json.loads(e["input"]).get("orderId")})
        if status == "FAILED":
            out.update({"error": "TimeoutError", "cause": SFN_CAUSE, "redriveStatus": "REDRIVABLE"})
        elif status == "ABORTED":
            out.update({"error": "Manual.Stop", "cause": "Stopped from the console", "redriveStatus": "REDRIVABLE"})
        else:
            out.update({"redriveStatus": "NOT_REDRIVABLE",
                        "redriveStatusReason": "Execution is RUNNING and cannot be redriven" if status == "RUNNING"
                        else "Execution is SUCCEEDED and cannot be redriven"})
        return out
    if op == "GetExecutionHistory":
        events = sfn_history(sfn_find_exec(body["executionArn"]))
        if body.get("reverseOrder"):
            events = events[::-1]
        start = int(body.get("nextToken") or 0)
        size = body.get("maxResults") or 1000
        out = {"events": events[start:start + size]}
        if start + size < len(events):
            out["nextToken"] = str(start + size)
        return out
    if op == "RedriveExecution":
        e = sfn_find_exec(body["executionArn"])
        if sfn_status(e) not in ("FAILED", "ABORTED", "TIMED_OUT"):
            raise KeyError(f"ExecutionNotRedrivable: {sfn_status(e)}")
        e["scenario"], e["redrives"] = "redriven", e["redrives"] + 1
        return {"redriveDate": time.time()}
    if op == "StartExecution":
        name = body["stateMachineArn"].rsplit(":", 1)[-1]
        if name not in SFN_DEFS:
            raise KeyError(f"StateMachineDoesNotExist: {name}")
        json.loads(body.get("input") or "{}")
        e = {"machine": name, "name": body.get("name") or uuid.uuid4().hex[:12], "scenario": "started",
             "start": time.time(), "redrives": 0, "input": body.get("input") or "{}"}
        if any(sfn_exec_arn(x) == sfn_exec_arn(e) for x in SFN_EXECUTIONS):
            raise KeyError(f"ExecutionAlreadyExists: {e['name']}")
        SFN_EXECUTIONS.append(e)
        return {"executionArn": sfn_exec_arn(e), "startDate": e["start"]}
    if op == "StopExecution":
        e = sfn_find_exec(body["executionArn"])
        if sfn_status(e) != "RUNNING":
            raise KeyError("ValidationException: execution is not running")
        e["scenario"] = "aborted"
        return {"stopDate": time.time()}
    raise KeyError(op)


# ---- DynamoDB (awsJson1_0) -----------------------------------------------------------------
def ddb_value(v):
    """Plain Python value → DynamoDB JSON."""
    if isinstance(v, bool):
        return {"BOOL": v}
    if v is None:
        return {"NULL": True}
    if isinstance(v, (int, float)):
        return {"N": str(v)}
    if isinstance(v, str):
        return {"S": v}
    if isinstance(v, set):
        return {"NS": sorted(str(x) for x in v)} if all(isinstance(x, (int, float)) for x in v) else {"SS": sorted(v)}
    if isinstance(v, dict):
        return {"M": {k: ddb_value(x) for k, x in v.items()}}
    if isinstance(v, list):
        return {"L": [ddb_value(x) for x in v]}
    raise TypeError(v)


def ddb_plain(av):
    """DynamoDB JSON key value → comparable Python value."""
    kind, v = next(iter(av.items()))
    if kind == "N":
        return float(v) if "." in v else int(v)
    return v


DDB_USERS = [
    {"userId": f"u-{n:03d}", "email": f"user{n}@example.com", "name": name, "active": n % 3 != 0,
     "age": 20 + n * 3, "nickname": None, "roles": ["viewer"] + (["admin"] if n == 1 else []),
     "tags": {"beta", "early"} if n % 2 else {"standard"}, "address": {"city": city, "zip": 110110 + n},
     "lastLogin": f"2026-10-{10 + n:02d}T08:{n:02d}:00Z"}
    for n, (name, city) in enumerate([("Ana", "Bogotá"), ("Luis", "Medellín"), ("Sofía", "Cali"), ("Diego", "Bogotá"),
                                      ("Valentina", "Barranquilla"), ("Mateo", "Cartagena"), ("Camila", "Bogotá"),
                                      ("Juan", "Pereira")], start=1)]
DDB_ORDERS = [
    {"customerId": f"c-{1000 + c}", "createdAt": f"2026-{1 + (k % 10):02d}-{1 + (k * 7) % 27:02d}T{(k * 5) % 24:02d}:15:00Z",
     "orderId": f"o-{c}{k:03d}", "status": ["PENDING", "SHIPPED", "DELIVERED", "CANCELLED"][(c + k) % 4],
     "total": round(19.9 + (c * 31 + k * 17) % 400 + 0.5, 2), "currency": "COP",
     "items": [{"sku": f"SKU-{(k + i) % 9}", "qty": 1 + i, "price": 9.95 + i} for i in range(1 + k % 3)],
     "shipping": {"carrier": "Servientrega", "tracking": f"ST{c}{k:05d}"} if (c + k) % 4 else None,
     "gift": k % 5 == 0, "labels": {"vip"} if c == 1 else {"web", "mobile"} if k % 2 else {"web"}}
    for c in range(1, 5) for k in range(30)]
DDB_INVOICES = [{"invoiceNo": 1000 + n, "amount": 120.5 * n, "paid": n % 2 == 0, "issued": f"2026-09-{n:02d}"} for n in range(1, 16)]
DDB_TABLES = {
    "users": {"keys": [("userId", "S", "HASH")], "items": DDB_USERS, "gsis": []},
    "orders": {"keys": [("customerId", "S", "HASH"), ("createdAt", "S", "RANGE")], "items": DDB_ORDERS,
               "gsis": [("status-index", [("status", "S", "HASH"), ("createdAt", "S", "RANGE")])]},
    "invoices": {"keys": [("invoiceNo", "N", "HASH")], "items": DDB_INVOICES, "gsis": [], "provisioned": (500, 500)},
    "sessions-legacy": {"keys": [("sessionId", "S", "HASH")], "items": [], "gsis": [], "provisioned": (100, 50)},
    "events-archive": {"keys": [("eventId", "S", "HASH")], "items": [], "gsis": [], "size": int(380 * 1024 ** 3)},
}


def ddb_table(name):
    if name not in DDB_TABLES:
        raise KeyError(f"ResourceNotFoundException: Requested resource not found: Table: {name} not found")
    return DDB_TABLES[name]


def ddb_describe(name, t):
    types = {a: k for keys in [t["keys"]] + [g[1] for g in t["gsis"]] for a, k, _ in keys}
    return {"Table": {
        "TableName": name, "TableStatus": "ACTIVE", "TableArn": f"arn:aws:dynamodb:us-east-1:123456789012:table/{name}",
        "KeySchema": [{"AttributeName": a, "KeyType": r} for a, _, r in t["keys"]],
        "AttributeDefinitions": [{"AttributeName": a, "AttributeType": k} for a, k in types.items()],
        "GlobalSecondaryIndexes": [{"IndexName": g, "KeySchema": [{"AttributeName": a, "KeyType": r} for a, _, r in ks],
                                    "Projection": {"ProjectionType": "ALL"}, "IndexStatus": "ACTIVE",
                                    "ItemCount": len(t["items"])} for g, ks in t["gsis"]],
        "ItemCount": len(t["items"]), "TableSizeBytes": t.get("size", len(json.dumps(t["items"], default=sorted))),
        "BillingModeSummary": {"BillingMode": "PROVISIONED" if "provisioned" in t else "PAY_PER_REQUEST"},
        "ProvisionedThroughput": {"ReadCapacityUnits": t.get("provisioned", (0, 0))[0], "WriteCapacityUnits": t.get("provisioned", (0, 0))[1]},
        "TableClassSummary": {"TableClass": "STANDARD"},
        "StreamSpecification": {"StreamEnabled": True, "StreamViewType": "NEW_AND_OLD_IMAGES"} if name == "orders" else None,
        "CreationDateTime": T0 - 86400 * 400, "DeletionProtectionEnabled": name == "orders"}}


def ddb_page(t, items, key_names, body):
    """Pagination by ExclusiveStartKey/Limit; LastEvaluatedKey carries the table and index keys."""
    start = 0
    if body.get("ExclusiveStartKey"):
        want = {k: ddb_plain(v) for k, v in body["ExclusiveStartKey"].items()}
        start = next(i + 1 for i, it in enumerate(items) if all(it.get(k) == v for k, v in want.items()))
    limit = body.get("Limit", 1000)
    page = items[start:start + limit]
    out = {"Items": [{k: ddb_value(v) for k, v in it.items()} for it in page], "Count": len(page), "ScannedCount": len(page)}
    if start + limit < len(items) and page:
        out["LastEvaluatedKey"] = {k: ddb_value(page[-1][k]) for k in key_names}
    return out


def dynamodb(op, body):
    import re
    if op == "ListTables":
        names = sorted(DDB_TABLES)
        start = names.index(body["ExclusiveStartTableName"]) + 1 if body.get("ExclusiveStartTableName") else 0
        limit = body.get("Limit", 100)
        out = {"TableNames": names[start:start + limit]}
        if start + limit < len(names):
            out["LastEvaluatedTableName"] = names[start + limit - 1]
        return out
    t = ddb_table(body["TableName"])
    table_keys = [a for a, _, _ in t["keys"]]
    if op == "DescribeTable":
        return ddb_describe(body["TableName"], t)
    if op == "GetItem":
        want = {k: ddb_plain(v) for k, v in body["Key"].items()}
        hit = next((it for it in t["items"] if all(it.get(k) == v for k, v in want.items())), None)
        return {"Item": {k: ddb_value(v) for k, v in hit.items()}} if hit else {}
    if op == "Scan":
        return ddb_page(t, t["items"], table_keys, body)
    if op == "Query":
        keys = t["keys"]
        if body.get("IndexName"):
            keys = dict(t["gsis"])[body["IndexName"]]
        pk = keys[0][0]
        sk = keys[1][0] if len(keys) > 1 else None
        values = {k: ddb_plain(v) for k, v in body["ExpressionAttributeValues"].items()}
        expr = body["KeyConditionExpression"]
        assert body["ExpressionAttributeNames"]["#pk"] == pk, "#pk must name the partition key"
        tests = {
            r"#pk = :pk": lambda v: True,
            r"#pk = :pk AND #sk = :sk": lambda v: v == values[":sk"],
            r"#pk = :pk AND begins_with\(#sk, :sk\)": lambda v: str(v).startswith(str(values[":sk"])),
            r"#pk = :pk AND #sk BETWEEN :sk1 AND :sk2": lambda v: values[":sk1"] <= v <= values[":sk2"],
            r"#pk = :pk AND #sk < :sk": lambda v: v < values[":sk"],
            r"#pk = :pk AND #sk <= :sk": lambda v: v <= values[":sk"],
            r"#pk = :pk AND #sk > :sk": lambda v: v > values[":sk"],
            r"#pk = :pk AND #sk >= :sk": lambda v: v >= values[":sk"],
        }
        test = next((f for pattern, f in tests.items() if re.fullmatch(pattern, expr)), None)
        if test is None:
            raise KeyError(f"ValidationException: unsupported KeyConditionExpression {expr}")
        hits = [it for it in t["items"] if it.get(pk) == values[":pk"] and (sk is None or (sk in it and test(it[sk])))]
        if sk:
            hits.sort(key=lambda it: it[sk], reverse=body.get("ScanIndexForward") is False)
        names = list(dict.fromkeys(table_keys + [a for a, _, _ in keys]))
        return ddb_page(t, hits, names, body)
    raise KeyError(op)


# ---- EventBridge (awsJson1_1) --------------------------------------------------------------
EB_ACCOUNT = "123456789012"
EVENT_BUSES = [{"Name": "default", "Arn": f"arn:aws:events:us-east-1:{EB_ACCOUNT}:event-bus/default"},
               {"Name": "orders-bus", "Arn": f"arn:aws:events:us-east-1:{EB_ACCOUNT}:event-bus/orders-bus",
                "Description": "Domain events of the orders service"}]
EB_RULES = [
    {"Name": "nightly-report", "EventBusName": "default", "State": "ENABLED", "ScheduleExpression": "cron(0 3 * * ? *)",
     "Description": "Builds the daily sales report"},
    {"Name": "ec2-state-change", "EventBusName": "default", "State": "ENABLED",
     "EventPattern": json.dumps({"source": ["aws.ec2"], "detail-type": ["EC2 Instance State-change Notification"],
                                 "detail": {"state": ["stopped", "terminated"]}})},
    {"Name": "order-paid", "EventBusName": "orders-bus", "State": "ENABLED", "Description": "Charges and fulfils paid orders",
     "EventPattern": json.dumps({"source": ["orders.service"], "detail-type": ["OrderPaid"],
                                 "detail": {"amount": [{"numeric": [">", 100]}], "customer": {"tier": ["gold", "platinum"]}}})},
    {"Name": "order-cancelled", "EventBusName": "orders-bus", "State": "DISABLED",
     "EventPattern": json.dumps({"source": [{"prefix": "orders."}], "detail-type": ["OrderCancelled"],
                                 "detail": {"reason": [{"exists": True}]}})},
]
for _r in EB_RULES:
    _bus = "" if _r["EventBusName"] == "default" else _r["EventBusName"] + "/"
    _r["Arn"] = f"arn:aws:events:us-east-1:{EB_ACCOUNT}:rule/{_bus}{_r['Name']}"
EB_TARGETS = {
    "nightly-report": [{"Id": "report", "Arn": f"arn:aws:lambda:us-east-1:{EB_ACCOUNT}:function:report-builder"}],
    "ec2-state-change": [{"Id": "alerts", "Arn": f"arn:aws:sqs:us-east-1:{EB_ACCOUNT}:ops-alerts",
                          "InputTransformer": {"InputPathsMap": {"id": "$.detail.instance-id", "state": "$.detail.state"},
                                               "InputTemplate": "\"Instance <id> is now <state>\""}}],
    "order-paid": [{"Id": "charge", "Arn": f"arn:aws:lambda:us-east-1:{EB_ACCOUNT}:function:payments-webhook", "InputPath": "$.detail"},
                   {"Id": "fulfil", "Arn": f"arn:aws:states:us-east-1:{EB_ACCOUNT}:stateMachine:order-fulfillment",
                    "RoleArn": f"arn:aws:iam::{EB_ACCOUNT}:role/events-invoke-sfn",
                    "DeadLetterConfig": {"Arn": f"arn:aws:sqs:us-east-1:{EB_ACCOUNT}:order-events-dlq"},
                    "RetryPolicy": {"MaximumRetryAttempts": 3, "MaximumEventAgeInSeconds": 3600}}],
    "order-cancelled": [{"Id": "audit", "Arn": f"arn:aws:logs:us-east-1:{EB_ACCOUNT}:log-group:/aws/events/orders-audit"}],
}
SENT_EVENTS = []


def eb_value_matches(element, present, value):
    """One element of a pattern array against the event's value (present=False: key missing)."""
    if not isinstance(element, dict):
        return present and (element in value if isinstance(value, list) else element == value)
    op, arg = next(iter(element.items()))
    if op == "exists":
        return present == bool(arg)
    if not present:
        return False
    values = value if isinstance(value, list) else [value]
    if op == "prefix":
        return any(isinstance(v, str) and v.startswith(arg) for v in values)
    if op == "suffix":
        return any(isinstance(v, str) and v.endswith(arg) for v in values)
    if op == "equals-ignore-case":
        return any(isinstance(v, str) and v.lower() == arg.lower() for v in values)
    if op == "anything-but":
        banned = arg if isinstance(arg, list) else [arg]
        return all(v not in banned for v in values)
    if op == "wildcard":
        import fnmatch
        return any(isinstance(v, str) and fnmatch.fnmatchcase(v, arg) for v in values)
    if op == "numeric":
        def in_range(v):
            if not isinstance(v, (int, float)) or isinstance(v, bool):
                return False
            checks = {"<": lambda n: v < n, "<=": lambda n: v <= n, "=": lambda n: v == n,
                      ">=": lambda n: v >= n, ">": lambda n: v > n}
            return all(checks[cmp](n) for cmp, n in zip(arg[::2], arg[1::2]))
        return any(in_range(v) for v in values)
    raise KeyError(f"InvalidEventPatternException: unsupported filter {op}")


def eb_matches(pattern, event):
    for key, filt in pattern.items():
        present = isinstance(event, dict) and key in event
        value = event.get(key) if present else None
        if isinstance(filt, dict):
            if not (present and isinstance(value, dict) and eb_matches(filt, value)):
                return False
        elif isinstance(filt, list):
            if not any(eb_value_matches(el, present, value) for el in filt):
                return False
        else:
            raise KeyError("InvalidEventPatternException: pattern values must be arrays or objects")
    return True


def eb_rule(body):
    bus = body.get("EventBusName") or "default"
    name = body.get("Name", body.get("Rule"))
    rule = next((r for r in EB_RULES if r["Name"] == name and r["EventBusName"] == bus), None)
    if rule is None:
        raise KeyError(f"ResourceNotFoundException: Rule {name} does not exist on EventBus {bus}.")
    return rule


def eventbridge(op, body):
    if op == "ListEventBuses":
        return {"EventBuses": EVENT_BUSES}
    if op == "ListRules":
        bus = body.get("EventBusName") or "default"
        return {"Rules": [r for r in EB_RULES if r["EventBusName"] == bus and r["Name"].startswith(body.get("NamePrefix", ""))]}
    if op == "DescribeRule":
        return {**eb_rule(body), "CreatedBy": EB_ACCOUNT}
    if op == "ListTargetsByRule":
        return {"Targets": EB_TARGETS.get(eb_rule(body)["Name"], [])}
    if op == "TestEventPattern":
        try:
            pattern, event = json.loads(body["EventPattern"]), json.loads(body["Event"])
        except ValueError as e:
            raise KeyError(f"InvalidEventPatternException: {e}")
        missing = [k for k in ("id", "account", "source", "time", "region", "detail-type") if k not in event]
        if missing:
            raise KeyError(f"InvalidEventPatternException: event is missing {missing}")
        return {"Result": eb_matches(pattern, event)}
    if op == "PutEvents":
        entries = []
        for e in body.get("Entries", []):
            try:
                detail = json.loads(e.get("Detail") or "")
                ok = isinstance(detail, dict) and e.get("Source") and e.get("DetailType")
            except ValueError:
                ok = False
            if ok:
                entries.append({"EventId": str(uuid.uuid4())})
                SENT_EVENTS.append(e)
            else:
                entries.append({"ErrorCode": "MalformedDetail", "ErrorMessage": "Detail is malformed."})
        return {"FailedEntryCount": sum("ErrorCode" in x for x in entries), "Entries": entries}
    raise KeyError(op)


# ---- CloudWatch alarms (awsQuery, Version 2010-08-01) ----------------------------------------
CW_SNS = "arn:aws:sns:us-east-1:123456789012:ops-alerts"


def cw_alarm_defs():
    """The same alarms in every environment (the mock cannot tell them apart). `history`:
    (seconds ago, old state, new state), newest first."""
    now = time.time()
    fn = lambda name: [("FunctionName", name)]
    return [
        {"name": "payments-webhook-errors", "state": "ALARM", "since": now - 720, "desc": "The payment webhook is failing",
         "ns": "AWS/Lambda", "metric": "Errors", "dims": fn("payments-webhook"), "stat": "Sum", "period": 60,
         "evals": 3, "dta": 3, "threshold": 2.0, "op": "GreaterThanThreshold", "missing": "notBreaching", "actions": [CW_SNS],
         "reason": "Threshold Crossed: 3 out of the last 3 datapoints [3.0 (21/10/26 15:40:00), 3.0 (21/10/26 15:39:00), "
                   "3.0 (21/10/26 15:38:00)] were greater than the threshold (2.0) (minimum 3 datapoints for OK -> ALARM transition).",
         "history": [(720, "OK", "ALARM"), (3 * 3600, "ALARM", "OK"), (3.5 * 3600, "OK", "ALARM"), (2 * 86400, "INSUFFICIENT_DATA", "OK")]},
        {"name": "orders-dlq-not-empty", "state": "ALARM", "since": now - 7200, "desc": "Orders failed and wait in the DLQ",
         "ns": "AWS/SQS", "metric": "ApproximateNumberOfMessagesVisible", "dims": [("QueueName", "orders-dlq")], "stat": "Maximum",
         "period": 300, "evals": 1, "dta": 1, "threshold": 0.0, "op": "GreaterThanThreshold", "missing": "missing", "actions": [CW_SNS],
         "reason": "Threshold Crossed: 1 datapoint [12.0 (21/10/26 13:50:00)] was greater than the threshold (0.0).",
         "history": [(7200, "OK", "ALARM"), (5 * 86400, "INSUFFICIENT_DATA", "OK")]},
        {"name": "orders-table-read-throttles", "state": "OK", "since": now - 3 * 86400,
         "ns": "AWS/DynamoDB", "metric": "ReadThrottleEvents", "dims": [("TableName", "orders")], "stat": "Sum", "period": 300,
         "evals": 2, "dta": 2, "threshold": 50.0, "op": "GreaterThanOrEqualToThreshold", "missing": "notBreaching", "actions": [CW_SNS],
         "reason": "Threshold Crossed: 2 out of the last 2 datapoints were not greater than or equal to the threshold (50.0).",
         "history": [(3 * 86400, "ALARM", "OK"), (3 * 86400 + 900, "OK", "ALARM")]},
        {"name": "users-auth-throttles", "state": "OK", "since": now - 10 * 86400,
         "ns": "AWS/Lambda", "metric": "Throttles", "dims": fn("users-auth"), "stat": "Sum", "period": 60,
         "evals": 5, "dta": 5, "threshold": 10.0, "op": "GreaterThanThreshold", "missing": "notBreaching", "actions": [],
         "reason": "Threshold Crossed: no datapoints were greater than the threshold (10.0).",
         "history": [(10 * 86400, "INSUFFICIENT_DATA", "OK")]},
        {"name": "legacy-\x1b[31mjobs-age", "state": "INSUFFICIENT_DATA", "since": now - 45 * 86400,
         "ns": "AWS/SQS", "metric": "ApproximateAgeOfOldestMessage", "dims": [("QueueName", "legacy-jobs")], "stat": "Maximum",
         "period": 60, "evals": 5, "dta": 5, "threshold": 900.0, "op": "GreaterThanThreshold", "missing": "missing",
         "actions": [CW_SNS], "reason": "Insufficient Data: 5 datapoints were unknown.",
         "history": [(45 * 86400, "OK", "INSUFFICIENT_DATA")]},
        {"name": "order-fulfillment-failures", "state": "INSUFFICIENT_DATA", "since": now - 86400,
         "ns": "AWS/States", "metric": "ExecutionsFailed", "stat": "Sum", "period": 300,
         "dims": [("StateMachineArn", "arn:aws:states:us-east-1:123456789012:stateMachine:order-fulfillment")],
         "evals": 1, "dta": 1, "threshold": 0.0, "op": "GreaterThanThreshold", "missing": "missing", "actions": [CW_SNS],
         "reason": "Insufficient Data: 1 datapoint was unknown.", "history": [(86400, "OK", "INSUFFICIENT_DATA")]},
        {"name": "orders-sync-error-rate", "state": "OK", "since": now - 6 * 3600, "desc": "Errors per 100 calls",
         "math": [("errors", "Errors", False), ("calls", "Invocations", False)], "expr": ("rate", "100 * errors / calls"),
         "dims": fn("orders-sync"), "period": 300, "evals": 3, "dta": 2, "threshold": 5.0, "op": "GreaterThanThreshold",
         "missing": "notBreaching", "actions": [CW_SNS], "reason": "Threshold Crossed: 2 out of the last 3 datapoints were not greater than the threshold (5.0).",
         "history": [(6 * 3600, "ALARM", "OK"), (7 * 3600, "OK", "ALARM")]},
        {"name": "checkout-degraded", "state": "ALARM", "since": now - 720, "composite": True, "desc": "Payments or orders broken",
         "rule": 'ALARM("payments-webhook-errors") OR ALARM("orders-dlq-not-empty")', "actions": [CW_SNS],
         "reason": "arn:aws:cloudwatch:us-east-1:123456789012:alarm:payments-webhook-errors transitioned to ALARM",
         "history": [(720, "OK", "ALARM")]},
    ]


def cw_esc(s):
    return str(s).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")


def cw_dims_xml(dims):
    return "".join(f"<member><Name>{cw_esc(k)}</Name><Value>{cw_esc(v)}</Value></member>" for k, v in dims)


def cw_alarm_xml(a):
    arn = f"arn:aws:cloudwatch:us-east-1:123456789012:alarm:{a['name']}"
    when = cw_iso(a["since"])
    parts = [f"<AlarmName>{cw_esc(a['name'])}</AlarmName>", f"<AlarmArn>{cw_esc(arn)}</AlarmArn>",
             f"<StateValue>{a['state']}</StateValue>", f"<StateReason>{cw_esc(a['reason'])}</StateReason>",
             f"<StateUpdatedTimestamp>{when}</StateUpdatedTimestamp>",
             f"<StateTransitionedTimestamp>{when}</StateTransitionedTimestamp>",
             "<ActionsEnabled>true</ActionsEnabled>",
             "<AlarmActions>" + "".join(f"<member>{cw_esc(x)}</member>" for x in a["actions"]) + "</AlarmActions>",
             "<OKActions/><InsufficientDataActions/>"]
    if a.get("desc"):
        parts.append(f"<AlarmDescription>{cw_esc(a['desc'])}</AlarmDescription>")
    if a.get("composite"):
        parts.append(f"<AlarmRule>{cw_esc(a['rule'])}</AlarmRule>")
        return "<member>" + "".join(parts) + "</member>"
    if a.get("math"):
        queries = "".join(
            f"<member><Id>{qid}</Id><MetricStat><Metric><Namespace>AWS/Lambda</Namespace><MetricName>{metric}</MetricName>"
            f"<Dimensions>{cw_dims_xml(a['dims'])}</Dimensions></Metric><Period>{a['period']}</Period><Stat>Sum</Stat>"
            f"</MetricStat><ReturnData>{str(ret).lower()}</ReturnData></member>" for qid, metric, ret in a["math"])
        queries += (f"<member><Id>{a['expr'][0]}</Id><Expression>{cw_esc(a['expr'][1])}</Expression>"
                    f"<Label>Error rate</Label><ReturnData>true</ReturnData></member>")
        parts.append(f"<Metrics>{queries}</Metrics>")
    else:
        parts += [f"<Namespace>{a['ns']}</Namespace>", f"<MetricName>{a['metric']}</MetricName>",
                  f"<Statistic>{a['stat']}</Statistic>", f"<Dimensions>{cw_dims_xml(a['dims'])}</Dimensions>",
                  f"<Period>{a['period']}</Period>"]
    parts += [f"<EvaluationPeriods>{a['evals']}</EvaluationPeriods>", f"<DatapointsToAlarm>{a['dta']}</DatapointsToAlarm>",
              f"<Threshold>{a['threshold']}</Threshold>", f"<ComparisonOperator>{a['op']}</ComparisonOperator>",
              f"<TreatMissingData>{a['missing']}</TreatMissingData>"]
    return "<member>" + "".join(parts) + "</member>"


def cw_alarms(action, params):
    """DescribeAlarms and DescribeAlarmHistory: XML response (status, body)."""
    get = lambda k, d="": params.get(k, [d])[0]
    members = lambda prefix: [v for k, v in sorted(params.items()) if k.startswith(prefix + ".member.") for v in v]
    types = members("AlarmTypes") or ["MetricAlarm"]
    alarms = [a for a in cw_alarm_defs() if ("CompositeAlarm" if a.get("composite") else "MetricAlarm") in types]
    if action == "DescribeAlarms":
        names = members("AlarmNames")
        alarms = [a for a in alarms if (not names or a["name"] in names) and a["name"].startswith(get("AlarmNamePrefix"))
                  and (not get("StateValue") or a["state"] == get("StateValue"))]
        start, size = int(get("NextToken") or 0), 3  # small pages, to exercise NextToken
        page = alarms[start:start + size]
        token = f"<NextToken>{start + size}</NextToken>" if start + size < len(alarms) else ""
        metric = "".join(cw_alarm_xml(a) for a in page if not a.get("composite"))
        composite = "".join(cw_alarm_xml(a) for a in page if a.get("composite"))
        return 200, (f'<DescribeAlarmsResponse xmlns="http://monitoring.amazonaws.com/doc/2010-08-01/"><DescribeAlarmsResult>'
                     f"<MetricAlarms>{metric}</MetricAlarms><CompositeAlarms>{composite}</CompositeAlarms>{token}"
                     f"</DescribeAlarmsResult><ResponseMetadata><RequestId>mock</RequestId></ResponseMetadata></DescribeAlarmsResponse>")
    if action == "DescribeAlarmHistory":
        now, items = time.time(), []
        for a in alarms:
            if get("AlarmName") and a["name"] != get("AlarmName"):
                continue
            for ago, old, new in a["history"]:
                data = json.dumps({"version": "1.0", "oldState": {"stateValue": old}, "newState": {"stateValue": new, "stateReason": a["reason"]}})
                items.append((now - ago, f"<member><AlarmName>{cw_esc(a['name'])}</AlarmName><Timestamp>{cw_iso(now - ago)}</Timestamp>"
                                         f"<HistoryItemType>StateUpdate</HistoryItemType>"
                                         f"<HistorySummary>Alarm updated from {old} to {new}</HistorySummary>"
                                         f"<HistoryData>{cw_esc(data)}</HistoryData></member>"))
        items.sort(key=lambda x: -x[0])
        body = "".join(x for _, x in items[:int(get("MaxRecords") or 100)])
        return 200, (f"<DescribeAlarmHistoryResponse><DescribeAlarmHistoryResult><AlarmHistoryItems>{body}</AlarmHistoryItems>"
                     f"</DescribeAlarmHistoryResult></DescribeAlarmHistoryResponse>")
    return 400, query_error("InvalidAction", f"{action} is not in the mock yet")


# ---- CloudWatch metrics (awsQuery, Version 2010-08-01) ---------------------------------------
def cw_iso(t):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t))


def cw_parse(s):
    return calendar.timegm(time.strptime(s[:19], "%Y-%m-%dT%H:%M:%S"))


# Metric synthesizers of other sections, tried before cw_metric_value: each is called as
# hook(namespace, name, dims, stat, period, ts) and returns a value, CW_NO_DATA (no
# datapoint at ts) or None (not its metric). Each section appends its own.
CW_METRIC_HOOKS = []
CW_NO_DATA = object()


def cw_metric_value(name, dims, ts):
    """A plausible, repeatable value of a metric at `ts`: daily waves, a little noise, and
    error spikes for payments-webhook."""
    seed = int(hashlib.sha256(f"{name}{sorted(dims.items())}{int(ts)}".encode()).hexdigest()[:8], 16) / 0xFFFFFFFF
    wave = 1 + 0.6 * math.sin(ts / 86400 * 2 * math.pi)
    fn = dims.get("FunctionName", "")
    if name == "Errors":
        return float(int(seed * 4)) if fn == "payments-webhook" and seed > 0.6 else 0.0
    if name == "Throttles":
        return 1.0 if seed > 0.97 else 0.0
    if name == "Invocations":
        return float(int(40 * wave + 20 * seed))
    if name == "Duration":
        return round(180 * wave + 90 * seed, 1)
    if name == "ConcurrentExecutions":
        return float(int(3 * wave + 3 * seed))
    return round(10 * wave + 5 * seed, 2)


# ---- metrics read by the Savings checks ----------------------------------------------------------
# Per day: Lambda invocations (multiples of the default 5-minute value), log bytes received,
# empty receives, consumed capacity (units per second), executions.
SAVINGS_INVOCATIONS = {"report-builder": 20.0, "orders-sync": 4.0, "image-resizer": 6.0}
SAVINGS_LOG_BYTES = {"/ecs/orders-api": 12 * GiB, "/aws/lambda/report-builder": 6 * GiB, "/aws/lambda/orders-sync": 1.5 * GiB,
                     "/aws/lambda/payments-webhook": 0.4 * GiB}
SAVINGS_EMPTY_RECEIVES = {"notifications": (900_000, 30_000), "payments.fifo": (200_000, 50_000)}
# Table → ((read mean, read peak), (write mean, write peak)) units per second; spiky tables peak rarely.
SAVINGS_CAPACITY = {"invoices": ((10, 20), (12, 26)), "orders": ((500, 700), (100, 200)), "users": ((5, 400), (1, 2)),
                    "events-archive": ((2, 9), (1, 3)), "sessions-legacy": ((0, 0), (0, 0))}
SAVINGS_SFN = {"order-fulfillment": (150_000, 1800, 9000), "invoice-generation": (1, 40_000, 90_000),
               "report-export": (300, 240_000, 1_300_000)}


def savings_metric(namespace, name, dims, stat, period, ts):
    seed = int(hashlib.sha256(f"{name}{sorted(dims.items())}{int(ts)}".encode()).hexdigest()[:8], 16) / 0xFFFFFFFF
    wave = 1 + 0.2 * math.sin(ts / 86400 * 2 * math.pi)
    if namespace == "AWS/Lambda" and name == "Invocations" and period >= 86400:
        fn = dims.get("FunctionName", "")
        return cw_metric_value(name, dims, ts) * period / 300 * SAVINGS_INVOCATIONS.get(fn, 1.0)
    if namespace == "AWS/Lambda" and name == "ProvisionedConcurrencyUtilization":
        return round(0.06 + 0.06 * seed, 3)
    if namespace == "AWS/Logs" and name == "IncomingBytes":
        return SAVINGS_LOG_BYTES.get(dims.get("LogGroupName"), 0.02 * GiB) * period / 86400 * wave
    if namespace == "AWS/SQS" and name in ("NumberOfEmptyReceives", "NumberOfMessagesReceived") and period >= 3600:
        empty, received = SAVINGS_EMPTY_RECEIVES.get(dims.get("QueueName"), (0, 2_000))
        return (empty if name == "NumberOfEmptyReceives" else received) * period / 86400 * wave
    if namespace == "AWS/DynamoDB" and name in ("ConsumedReadCapacityUnits", "ConsumedWriteCapacityUnits"):
        read, write = SAVINGS_CAPACITY.get(dims.get("TableName"), ((1, 2), (1, 2)))
        mean, top = read if "Read" in name else write
        if not mean:
            return 0.0
        # The busiest hour once in a while; around the mean the rest of the time, spiky
        # tables (peaks over four times the mean) more unevenly.
        spread = 1.0 if top / mean > 4 else 0.2
        rate = top if int(ts // 3600) % 97 == 0 else mean * (1 - spread / 2 + spread * seed)
        return rate * period
    if namespace == "AWS/States" and name in ("ExecutionsStarted", "ExecutionTime") and period >= 86400:
        machine = dims.get("StateMachineArn", "").rsplit(":", 1)[-1]
        per_day, avg_ms, max_ms = SAVINGS_SFN.get(machine, (0, 0, 0))
        if not per_day:
            return CW_NO_DATA
        if name == "ExecutionsStarted":
            return per_day * period / 86400 * wave
        return max_ms if stat == "Maximum" else avg_ms * wave
    if namespace == "AWS/S3" and name == "BucketSizeBytes":
        size = S3_SIZES.get(dims.get("BucketName"), {}).get(dims.get("StorageType"))
        return float(size) if size else CW_NO_DATA
    return None


CW_METRIC_HOOKS.append(savings_metric)


def cw_metrics(action, params):
    """GetMetricData: XML response (status, body)."""
    if action != "GetMetricData":
        return 400, query_error("InvalidAction", f"{action} is not in the mock yet")
    get = lambda k, d="": params.get(k, [d])[0]
    start, end = cw_parse(get("StartTime")), cw_parse(get("EndTime"))
    results, i = [], 1
    while get(f"MetricDataQueries.member.{i}.Id"):
        p = f"MetricDataQueries.member.{i}"
        name = get(f"{p}.MetricStat.Metric.MetricName")
        period = int(get(f"{p}.MetricStat.Period", "300"))
        dims, j = {}, 1
        while get(f"{p}.MetricStat.Metric.Dimensions.member.{j}.Name"):
            dims[get(f"{p}.MetricStat.Metric.Dimensions.member.{j}.Name")] = get(f"{p}.MetricStat.Metric.Dimensions.member.{j}.Value")
            j += 1
        namespace, stat = get(f"{p}.MetricStat.Metric.Namespace"), get(f"{p}.MetricStat.Stat")
        times = list(range(int(start) // period * period, int(end), period))[-1440:]
        scale = {"p95": 1.7, "p99": 2.4, "Maximum": 2.6}.get(stat, 1.0) if name == "Duration" else 1.0
        points = []
        for t in times:
            v = next((r for r in (h(namespace, name, dims, stat, period, t) for h in CW_METRIC_HOOKS) if r is not None), None)
            if v is None:
                v = cw_metric_value(name, dims, t) * scale
            if v is not CW_NO_DATA:
                points.append((t, round(v, 2)))
        stamps = "".join(f"<member>{cw_iso(t)}</member>" for t, _ in points)
        values = "".join(f"<member>{v}</member>" for _, v in points)
        results.append(f"<member><Timestamps>{stamps}</Timestamps><Values>{values}</Values><Label>{name}</Label>"
                       f"<Id>{get(p + '.Id')}</Id><StatusCode>Complete</StatusCode></member>")
        i += 1
    return 200, (f"<GetMetricDataResponse><GetMetricDataResult><MetricDataResults>{''.join(results)}</MetricDataResults>"
                 f"<Messages/></GetMetricDataResult></GetMetricDataResponse>")


def cloudwatch(params):
    action = params.get("Action", [""])[0]
    if "Alarm" in action:
        return cw_alarms(action, params)
    return cw_metrics(action, params)


def query_error(code, message):
    return (f"<ErrorResponse><Error><Type>Sender</Type><Code>{code}</Code><Message>{message}</Message></Error>"
            f"<RequestId>mock</RequestId></ErrorResponse>")


# ---- SQS (awsJson1_0, X-Amz-Target AmazonSQS.*) ----------------------------------------------
SQS_BASE = "https://sqs.us-east-1.amazonaws.com/123456789012"
SQS_ARN = "arn:aws:sqs:us-east-1:123456789012"


def _sqs_dlq_message(i):
    order = f"ORD-{1000 + i * 7}"
    detail = {"orderId": order, "customerId": f"C-{40 + i}", "amount": round(19.9 + i * 13.5, 2), "currency": "USD"}
    body = json.dumps({"version": "0", "id": str(uuid.UUID(int=i + 1)), "detail-type": "OrderPaid", "source": "orders.service",
                       "account": "123456789012", "time": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(T0 - 3600 * (40 - i))),
                       "region": "us-east-1", "resources": [], "detail": detail})
    sent = int((T0 - 3600 * (40 - i)) * 1000)
    attrs = {}
    if i % 3 == 0:
        attrs = {"ErrorCode": {"DataType": "String", "StringValue": "200"},
                 "ErrorMessage": {"DataType": "String", "StringValue": "Task timed out after 30.03 seconds"},
                 "RequestID": {"DataType": "String", "StringValue": str(uuid.UUID(int=500 + i))}}
    elif i % 3 == 1:
        attrs = {"ErrorMessage": {"DataType": "String", "StringValue": f"PaymentDeclined: card ending {4000 + i} was declined"}}
    return {"MessageId": str(uuid.UUID(int=900 + i)), "Body": body, "MD5OfBody": hashlib.md5(body.encode()).hexdigest(),
            "Attributes": {"SentTimestamp": str(sent), "ApproximateReceiveCount": "5",
                           "ApproximateFirstReceiveTimestamp": str(sent + 2000),
                           "SenderId": "AROAEXAMPLEID:orders-api"},
            "MessageAttributes": attrs}


def _sqs_queue(name, visible=0, in_flight=0, delayed=0, **extra):
    q = {"name": name, "messages": [], "counts": [visible, in_flight, delayed],
         "attrs": {"QueueArn": f"{SQS_ARN}:{name}", "CreatedTimestamp": str(int(T0 - 86400 * 200)),
                   "LastModifiedTimestamp": str(int(T0 - 86400 * 9)), "VisibilityTimeout": "30",
                   "MaximumMessageSize": "262144", "MessageRetentionPeriod": "345600", "DelaySeconds": "0",
                   "ReceiveMessageWaitTimeSeconds": "0", "SqsManagedSseEnabled": "true"}}
    q["attrs"].update(extra)
    return q


SQS_QUEUES = {q["name"]: q for q in [
    _sqs_queue("orders", 37, 4, 0, ReceiveMessageWaitTimeSeconds="20",
               RedrivePolicy=json.dumps({"deadLetterTargetArn": f"{SQS_ARN}:orders-dlq", "maxReceiveCount": 5})),
    _sqs_queue("orders-dlq", MessageRetentionPeriod="1209600",
               RedriveAllowPolicy=json.dumps({"redrivePermission": "byQueue", "sourceQueueArns": [f"{SQS_ARN}:orders"]})),
    _sqs_queue("payments.fifo", 3, 0, 0, FifoQueue="true", ContentBasedDeduplication="true",
               RedrivePolicy=json.dumps({"deadLetterTargetArn": f"{SQS_ARN}:payments-dlq.fifo", "maxReceiveCount": "3"})),
    _sqs_queue("payments-dlq.fifo", FifoQueue="true", ContentBasedDeduplication="true", MessageRetentionPeriod="1209600"),
    _sqs_queue("notifications", 0, 2, 1, KmsMasterKeyId="alias/aws/sqs", SqsManagedSseEnabled="false",
               DelaySeconds="5"),
]}
SQS_QUEUES["orders-dlq"]["messages"] = [_sqs_dlq_message(i) for i in range(12)]
SQS_TASKS = []  # message move tasks: {handle, source, destination, to_move, moved, status, started, cancelled_at}


def _sqs_by_url(url):
    q = SQS_QUEUES.get(url.rstrip("/").rsplit("/", 1)[-1])
    if q is None:
        raise KeyError(f"QueueDoesNotExist: The specified queue does not exist: {url}")
    return q


def _sqs_by_arn(arn):
    q = SQS_QUEUES.get(arn.rsplit(":", 1)[-1])
    if q is None:
        raise KeyError(f"ResourceNotFoundException: {arn}")
    return q


def _sqs_advance():
    """Moves messages of running tasks: 2 per second."""
    now = time.time()
    for t in SQS_TASKS:
        if t["status"] not in ("RUNNING", "CANCELLING"):
            continue
        target = min(t["to_move"], int((now - t["started"]) * 2))
        src = _sqs_by_arn(t["source"])
        while t["moved"] < target and src["messages"]:
            m = src["messages"].pop(0)
            if t["destination"]:
                dests = [_sqs_by_arn(t["destination"])]
            else:
                dests = [q for q in SQS_QUEUES.values()
                         if json.loads(q["attrs"].get("RedrivePolicy") or "{}").get("deadLetterTargetArn") == t["source"]]
            if dests:
                dests[0]["counts"][0] += 1
            t["moved"] += 1
        if t["status"] == "CANCELLING":
            t["status"] = "CANCELLED"
        elif t["moved"] >= t["to_move"]:
            t["status"] = "COMPLETED"


def _sqs_attrs(q):
    visible = len(q["messages"]) if q["messages"] or q["name"].endswith(("-dlq", "-dlq.fifo")) else q["counts"][0]
    return {**q["attrs"], "ApproximateNumberOfMessages": str(visible),
            "ApproximateNumberOfMessagesNotVisible": str(q["counts"][1]),
            "ApproximateNumberOfMessagesDelayed": str(q["counts"][2])}


def sqs(op, body):
    _sqs_advance()
    if op == "ListQueues":
        prefix = body.get("QueueNamePrefix", "")
        names = sorted(n for n in SQS_QUEUES if n.startswith(prefix))
        start, size = int(body.get("NextToken") or 0), body.get("MaxResults") or 1000
        out = {"QueueUrls": [f"{SQS_BASE}/{n}" for n in names[start:start + size]]}
        if start + size < len(names):
            out["NextToken"] = str(start + size)
        return out
    if op == "GetQueueUrl":
        if body["QueueName"] not in SQS_QUEUES:
            raise KeyError(f"QueueDoesNotExist: The specified queue does not exist: {body['QueueName']}")
        return {"QueueUrl": f"{SQS_BASE}/{body['QueueName']}"}
    if op == "GetQueueAttributes":
        return {"Attributes": _sqs_attrs(_sqs_by_url(body["QueueUrl"]))}
    if op == "ListDeadLetterSourceQueues":
        arn = _sqs_by_url(body["QueueUrl"])["attrs"]["QueueArn"]
        return {"queueUrls": [f"{SQS_BASE}/{q['name']}" for q in SQS_QUEUES.values()
                              if json.loads(q["attrs"].get("RedrivePolicy") or "{}").get("deadLetterTargetArn") == arn]}
    if op == "ReceiveMessage":
        q = _sqs_by_url(body["QueueUrl"])
        # Short polling samples: some of the messages each time.
        sample = [m for m in q["messages"] if hash((m["MessageId"], time.time_ns() // 1000)) % 3]
        out = []
        for m in sample[:body.get("MaxNumberOfMessages") or 1]:
            m["Attributes"]["ApproximateReceiveCount"] = str(int(m["Attributes"]["ApproximateReceiveCount"]) + 1)
            out.append({**m, "ReceiptHandle": uuid.uuid4().hex})
        return {"Messages": out} if out else {}
    if op == "StartMessageMoveTask":
        src = _sqs_by_arn(body["SourceArn"])
        if any(t["source"] == body["SourceArn"] and t["status"] == "RUNNING" for t in SQS_TASKS):
            raise KeyError("UnsupportedOperation: a message move task is already running for this queue")
        if body.get("DestinationArn"):
            _sqs_by_arn(body["DestinationArn"])
        task = {"handle": uuid.uuid4().hex, "source": body["SourceArn"], "destination": body.get("DestinationArn"),
                "to_move": len(src["messages"]), "moved": 0, "status": "RUNNING", "started": time.time()}
        SQS_TASKS.insert(0, task)
        return {"TaskHandle": task["handle"]}
    if op == "ListMessageMoveTasks":
        _sqs_by_arn(body["SourceArn"])
        out = []
        for t in [t for t in SQS_TASKS if t["source"] == body["SourceArn"]][:body.get("MaxResults") or 1]:
            r = {"Status": t["status"], "SourceArn": t["source"], "ApproximateNumberOfMessagesMoved": t["moved"],
                 "ApproximateNumberOfMessagesToMove": t["to_move"], "StartedTimestamp": int(t["started"] * 1000)}
            if t["destination"]:
                r["DestinationArn"] = t["destination"]
            if t["status"] == "RUNNING":
                r["TaskHandle"] = t["handle"]
            out.append(r)
        return {"Results": out}
    if op == "SetQueueAttributes":
        q = _sqs_by_url(body["QueueUrl"])
        for k, v in body.get("Attributes", {}).items():
            if k == "ReceiveMessageWaitTimeSeconds" and not (str(v).isdigit() and 0 <= int(v) <= 20):
                raise KeyError(f"InvalidAttributeValue: Invalid value for the parameter ReceiveMessageWaitTimeSeconds: {v}")
            q["attrs"][k] = str(v)
        return {}
    if op == "CancelMessageMoveTask":
        t = next((t for t in SQS_TASKS if t["handle"] == body["TaskHandle"]), None)
        if t is None or t["status"] != "RUNNING":
            raise KeyError("ResourceNotFoundException: no running task with that handle")
        t["status"] = "CANCELLING"
        return {"ApproximateNumberOfMessagesMoved": t["moved"]}
    raise KeyError(op)


# ---- AppConfig (restJson1; hosted configurations are raw documents) --------------------------
AC_APPS = [{"Id": "ckout01", "Name": "checkout", "Description": "Checkout service"},
           {"Id": "store02", "Name": "storefront", "Description": "Web storefront"}]
AC_ENVS = {"ckout01": [{"Id": "envdev", "Name": "Development"}, {"Id": "envbeta", "Name": "Beta"},
                       {"Id": "envprod", "Name": "Production"}],
           "store02": [{"Id": "stdev", "Name": "Development"}, {"Id": "stprod", "Name": "Production"}]}
AC_PROFILES = {"ckout01": [{"Id": "flags01", "Name": "checkout-flags", "Type": "AWS.AppConfig.FeatureFlags", "LocationUri": "hosted"},
                           {"Id": "free01", "Name": "service-config", "Type": "AWS.Freeform", "LocationUri": "hosted"}],
               "store02": [{"Id": "flags02", "Name": "storefront-flags", "Type": "AWS.AppConfig.FeatureFlags", "LocationUri": "hosted"}]}


def ac_flags(values, extra=()):
    """A feature flags document: values {key: (enabled, {attr: value})}."""
    defs = {
        "new_checkout": {"name": "New checkout", "description": "One-page checkout",
                         "attributes": {"rollout_percent": {"constraints": {"type": "number"}}}},
        "dark_mode": {"name": "Dark mode", "attributes": {"theme": {"constraints": {"type": "string"}}}},
        "express_shipping": {"name": "Express shipping", "description": "Same-day delivery in big cities",
                             "attributes": {"max_items": {"constraints": {"type": "number"}},
                                            "carriers": {"constraints": {"type": "string[]"}}}},
        "beta_banner": {"name": "Beta banner", "attributes": {"message": {"constraints": {"type": "string"}}},
                        "_createdAt": "2026-10-01T10:00:00.000Z"},
    }
    keys = list(values) + list(extra)
    return json.dumps({"version": "1", "flags": {k: defs[k] for k in keys},
                       "values": {k: {"enabled": on, **attrs} for k, (on, attrs) in values.items()}})


AC_VERSIONS = {
    ("ckout01", "flags01"): {
        1: ac_flags({"new_checkout": (False, {"rollout_percent": 0}), "dark_mode": (False, {"theme": "day"})}),
        2: ac_flags({"new_checkout": (True, {"rollout_percent": 10}), "dark_mode": (False, {"theme": "day"})}),
        3: ac_flags({"new_checkout": (True, {"rollout_percent": 50}), "dark_mode": (True, {"theme": "night"}),
                     "express_shipping": (False, {"max_items": 3, "carriers": ["dhl"]})}),
        4: ac_flags({"new_checkout": (True, {"rollout_percent": 100}), "dark_mode": (True, {"theme": "night"}),
                     "express_shipping": (True, {"max_items": 5, "carriers": ["dhl", "fedex"]}),
                     "beta_banner": (False, {"message": "Try the new checkout!"})}),
    },
    ("ckout01", "free01"): {
        1: json.dumps({"timeoutMs": 3000, "retries": 2}),
        2: json.dumps({"timeoutMs": 2500, "retries": 3, "endpoints": {"payments": "https://pay.internal", "tax": "https://tax.internal"}}),
    },
    ("store02", "flags02"): {1: ac_flags({"dark_mode": (True, {"theme": "auto"}), "beta_banner": (True, {"message": "Hi"})})},
}
AC_STRATEGIES = [
    {"Id": "AppConfig.AllAtOnce", "Name": "AppConfig.AllAtOnce", "Description": "Quick",
     "DeploymentDurationInMinutes": 0, "GrowthType": "LINEAR", "GrowthFactor": 100, "FinalBakeTimeInMinutes": 0},
    {"Id": "AppConfig.Linear50PercentEvery30Seconds", "Name": "AppConfig.Linear50PercentEvery30Seconds",
     "Description": "Testing/demonstration", "DeploymentDurationInMinutes": 1, "GrowthType": "LINEAR",
     "GrowthFactor": 50, "FinalBakeTimeInMinutes": 1},
    {"Id": "AppConfig.Canary10Percent20Minutes", "Name": "AppConfig.Canary10Percent20Minutes",
     "Description": "AWS recommended", "DeploymentDurationInMinutes": 20, "GrowthType": "EXPONENTIAL",
     "GrowthFactor": 10, "FinalBakeTimeInMinutes": 10},
]


def ac_history(app, env, rows):
    """rows: (profile id, version, state, days ago); numbered oldest first."""
    profiles = {p["Id"]: p for p in AC_PROFILES[app]}
    out = []
    for n, (pid, version, state, days) in enumerate(rows, start=1):
        start = time.time() - 86400 * days
        out.append({"DeploymentNumber": n, "ConfigurationProfileId": pid, "ConfigurationName": profiles[pid]["Name"],
                    "ConfigurationVersion": str(version), "DeploymentStrategyId": "AppConfig.AllAtOnce",
                    "Description": "", "Started": start, "Duration": 0, "Fixed": state})
    return out


AC_DEPLOYMENTS = {
    ("ckout01", "envdev"): ac_history("ckout01", "envdev", [("flags01", 1, "COMPLETE", 30), ("free01", 1, "COMPLETE", 29),
                                                            ("flags01", 2, "COMPLETE", 20), ("free01", 2, "COMPLETE", 12),
                                                            ("flags01", 3, "COMPLETE", 8), ("flags01", 4, "COMPLETE", 1)]),
    ("ckout01", "envbeta"): ac_history("ckout01", "envbeta", [("flags01", 1, "COMPLETE", 28), ("free01", 1, "COMPLETE", 27),
                                                              ("flags01", 3, "COMPLETE", 6), ("free01", 2, "COMPLETE", 5)]),
    ("ckout01", "envprod"): ac_history("ckout01", "envprod", [("flags01", 1, "COMPLETE", 26), ("free01", 1, "COMPLETE", 25),
                                                              ("flags01", 2, "COMPLETE", 15), ("flags01", 3, "ROLLED_BACK", 4)]),
    ("store02", "stdev"): ac_history("store02", "stdev", [("flags02", 1, "COMPLETE", 3)]),
    ("store02", "stprod"): ac_history("store02", "stprod", [("flags02", 1, "COMPLETE", 2)]),
}


def ac_deployment(app, env, d):
    """A deployment as AppConfig returns it; started ones progress over d["Duration"] seconds, then bake 3 s."""
    iso = lambda t: time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(t))
    elapsed = time.time() - d["Started"]
    if d.get("Fixed"):
        state, pct = d["Fixed"], 100.0
    elif elapsed < d["Duration"]:
        state, pct = "DEPLOYING", round(100.0 * elapsed / d["Duration"], 1)
    elif elapsed < d["Duration"] + 3:
        state, pct = "BAKING", 100.0
    else:
        state, pct = "COMPLETE", 100.0
    out = {"ApplicationId": app, "EnvironmentId": env, "DeploymentNumber": d["DeploymentNumber"],
           "ConfigurationProfileId": d["ConfigurationProfileId"], "ConfigurationName": d["ConfigurationName"],
           "ConfigurationVersion": d["ConfigurationVersion"], "DeploymentStrategyId": d["DeploymentStrategyId"],
           "Description": d["Description"], "State": state, "PercentageComplete": pct,
           "DeploymentDurationInMinutes": round(d["Duration"] / 60, 2), "StartedAt": iso(d["Started"])}
    if state in ("COMPLETE", "ROLLED_BACK"):
        out["CompletedAt"] = iso(d["Started"] + d["Duration"] + 3)
    return out


def appconfig_rest(method, path, query, headers, body):
    """Returns (status, headers, body bytes)."""
    def ok(obj, status=200):
        return status, {"content-type": "application/json"}, json.dumps(obj).encode()

    def err(status, code, message):
        return status, {"content-type": "application/json", "x-amzn-ErrorType": code}, json.dumps({"Message": message}).encode()

    parts = [p for p in path.split("/") if p]
    if parts == ["deploymentstrategies"]:
        return ok({"Items": AC_STRATEGIES})
    if parts == ["applications"]:
        return ok({"Items": AC_APPS})
    if len(parts) < 3 or parts[1] not in AC_ENVS:
        return err(404, "ResourceNotFoundException", f"Application {parts[1:2]} not found")
    app = parts[1]
    if parts[2:] == ["environments"]:
        return ok({"Items": [{**e, "ApplicationId": app, "State": "READY_FOR_DEPLOYMENT"} for e in AC_ENVS[app]]})
    if parts[2:] == ["configurationprofiles"]:
        return ok({"Items": [{**p, "ApplicationId": app} for p in AC_PROFILES[app]]})
    if parts[2] == "environments" and len(parts) >= 5 and parts[4] == "deployments":
        env = parts[3]
        if (app, env) not in AC_DEPLOYMENTS:
            return err(404, "ResourceNotFoundException", f"Environment {env} not found")
        history = AC_DEPLOYMENTS[(app, env)]
        if len(parts) == 5 and method == "GET":
            return ok({"Items": [ac_deployment(app, env, d) for d in reversed(history)]})
        if len(parts) == 5 and method == "POST":
            req = json.loads(body or b"{}")
            busy = [d for d in history if ac_deployment(app, env, d)["State"] in ("DEPLOYING", "BAKING")]
            if busy:
                return err(409, "ConflictException", f"Deployment #{busy[0]['DeploymentNumber']} is in progress in {env}")
            profile = next((p for p in AC_PROFILES[app] if p["Id"] == req["ConfigurationProfileId"]), None)
            if profile is None or int(req["ConfigurationVersion"]) not in AC_VERSIONS.get((app, profile["Id"]), {}):
                return err(400, "BadRequestException", "Unknown profile or version")
            fast = req["DeploymentStrategyId"] == "AppConfig.AllAtOnce"
            d = {"DeploymentNumber": len(history) + 1, "ConfigurationProfileId": profile["Id"],
                 "ConfigurationName": profile["Name"], "ConfigurationVersion": str(req["ConfigurationVersion"]),
                 "DeploymentStrategyId": req["DeploymentStrategyId"], "Description": req.get("Description", ""),
                 "Started": time.time(), "Duration": 4 if fast else 15, "Fixed": None}
            history.append(d)
            return ok(ac_deployment(app, env, d), 201)
        d = next((x for x in history if str(x["DeploymentNumber"]) == parts[5]), None) if len(parts) == 6 else None
        if d is None:
            return err(404, "ResourceNotFoundException", "Deployment not found")
        if method == "DELETE":
            if ac_deployment(app, env, d)["State"] not in ("DEPLOYING", "BAKING"):
                return err(400, "BadRequestException", "The deployment is not in progress")
            d["Fixed"] = "ROLLED_BACK"
            return ok(ac_deployment(app, env, d), 202)
        return ok(ac_deployment(app, env, d))
    if parts[2] == "configurationprofiles" and len(parts) >= 5 and parts[4] == "hostedconfigurationversions":
        versions = AC_VERSIONS.setdefault((app, parts[3]), {})
        if len(parts) == 5 and method == "GET":
            return ok({"Items": [{"ApplicationId": app, "ConfigurationProfileId": parts[3], "VersionNumber": n,
                                  "ContentType": "application/json"} for n in sorted(versions, reverse=True)]})
        if len(parts) == 5 and method == "POST":
            latest = max(versions) if versions else 0
            lock = headers.get("latest-version-number")
            if lock is not None and int(lock) != latest:
                return err(409, "ConflictException", f"Latest version is {latest}, not {lock}")
            json.loads(body)  # the mock, like AppConfig, rejects invalid JSON for this profile
            versions[latest + 1] = body.decode()
            return 201, {"content-type": "application/json", "Version-Number": str(latest + 1),
                         "Application-Id": app, "Configuration-Profile-Id": parts[3]}, body
        if len(parts) == 6 and parts[5].isdigit() and int(parts[5]) in versions:
            return 200, {"content-type": "application/json", "Version-Number": parts[5],
                         "Application-Id": app, "Configuration-Profile-Id": parts[3]}, versions[int(parts[5])].encode()
        return err(404, "ResourceNotFoundException", "Hosted configuration version not found")
    return err(404, "ResourceNotFoundException", f"mock: no route for {method} {path}")


# ---- API Gateway (restJson1: REST APIs under /restapis, HTTP and WebSocket APIs under /v2) ---
# REST APIs answer with epoch seconds and `item` lists, HTTP/WebSocket APIs with ISO dates and
# `items`. /__execute/{apiId}/… plays an HTTP API's invoke endpoint, for test calls.
def is_apigateway(path):
    return path.split("?")[0].split("/")[1:3] == ["v2", "apis"] or any(
        path.startswith(p) for p in ("/restapis", "/domainnames", "/v2/domainnames", "/apikeys", "/usageplans", "/__execute/"))


ACCOUNT = "123456789012"
APIGW_PORT = sys.argv[1] if len(sys.argv) > 1 else "47123"


def _lambda_uri(fn):
    return f"arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/arn:aws:lambda:us-east-1:{ACCOUNT}:function:{fn}/invocations"


def _method(auth="NONE", authorizer=None, integration=None, key=False, validator=None, params=None, models=None):
    m = {"authorizationType": auth, "apiKeyRequired": key, "methodIntegration": integration}
    if authorizer:
        m["authorizerId"] = authorizer
    if validator:
        m["requestValidatorId"] = validator
    if params:
        m["requestParameters"] = params
    if models:
        m["requestModels"] = models
    return m


def _proxy(fn, timeout=29000):
    return {"type": "AWS_PROXY", "httpMethod": "POST", "uri": _lambda_uri(fn), "timeoutInMillis": timeout,
            "passthroughBehavior": "WHEN_NO_MATCH", "connectionType": "INTERNET"}


def _days(n):
    return T0 - 86400 * n


# The escape sequence in a mapping template proves dcli shows AWS text without letting it
# drive the terminal.
APIGW_REST = {
    "a1b2c3": {
        "api": {"id": "a1b2c3", "name": "public-api", "description": "Public orders API", "createdDate": _days(400),
                "apiKeySource": "HEADER", "endpointConfiguration": {"types": ["REGIONAL"]}, "disableExecuteApiEndpoint": False},
        "resources": {
            "root0": ("/", {}),
            "res01": ("/orders", {
                "GET": _method("COGNITO_USER_POOLS", "c0gn1t", _proxy("orders-sync")),
                "POST": _method("COGNITO_USER_POOLS", "c0gn1t", _proxy("payments-webhook", 10000), validator="v-body",
                                models={"application/json": "Order"}),
            }),
            "res02": ("/orders/{id}", {
                "GET": _method("NONE", integration=_proxy("orders-sync"), params={"method.request.path.id": True}),
            }),
            "res03": ("/orders/{id}/ship", {
                "POST": _method("AWS_IAM", integration={
                    "type": "AWS", "httpMethod": "POST", "uri": f"arn:aws:apigateway:us-east-1:sqs:path/{ACCOUNT}/orders",
                    "credentials": f"arn:aws:iam::{ACCOUNT}:role/apigw-sqs", "passthroughBehavior": "NEVER", "timeoutInMillis": 29000,
                    "requestParameters": {"integration.request.header.Content-Type": "'application/x-www-form-urlencoded'"},
                    "requestTemplates": {"application/json": "Action=SendMessage&MessageBody=$util.urlEncode($input.body)"
                                                             "&MessageAttribute.1.Name=orderId\u001b]0;pwned\u0007"
                                                             "&MessageAttribute.1.Value.StringValue=$input.params('id')"}},
                    params={"method.request.path.id": True}),
            }),
            "res05": ("/checkout", {
                "POST": _method("COGNITO_USER_POOLS", "c0gn1t", {
                    "type": "AWS", "httpMethod": "POST", "uri": "arn:aws:apigateway:us-east-1:states:action/StartExecution",
                    "credentials": f"arn:aws:iam::{ACCOUNT}:role/apigw-sfn", "passthroughBehavior": "NEVER", "timeoutInMillis": 29000,
                    "requestTemplates": {"application/json": "{\n  \"input\": \"$util.escapeJavaScript($input.json('$'))\",\n"
                                                             f"  \"stateMachineArn\": \"arn:aws:states:us-east-1:{ACCOUNT}:stateMachine:order-fulfillment\"\n}}"}}),
            }),
            "res04": ("/health", {
                "GET": _method("NONE", integration={"type": "MOCK", "requestTemplates": {"application/json": "{\"statusCode\": 200}"},
                                                     "passthroughBehavior": "WHEN_NO_MATCH", "timeoutInMillis": 29000}),
            }),
            "res06": ("/users/{proxy+}", {
                "ANY": _method("CUSTOM", "t0k3n", {"type": "HTTP_PROXY", "httpMethod": "ANY", "uri": "https://legacy-users.internal.example.com/{proxy}",
                                                    "connectionType": "INTERNET", "timeoutInMillis": 8000}),
            }),
        },
        "authorizers": [
            {"id": "c0gn1t", "name": "orders-users", "type": "COGNITO_USER_POOLS", "identitySource": "method.request.header.Authorization",
             "providerARNs": [f"arn:aws:cognito-idp:us-east-1:{ACCOUNT}:userpool/us-east-1_AbC123"]},
            {"id": "t0k3n", "name": "legacy-token", "type": "TOKEN", "identitySource": "method.request.header.Authorization",
             "authorizerUri": _lambda_uri("users-auth"), "authorizerResultTtlInSeconds": 300},
        ],
        "validators": [{"id": "v-body", "name": "Validate body", "validateRequestBody": True, "validateRequestParameters": False}],
        "deployments": [
            {"id": "e7f8a9", "description": "add /checkout", "createdDate": T0 - 7200},
            {"id": "d5e6f7", "description": "orders/{id}/ship", "createdDate": _days(1)},
            {"id": "c4d5e6", "description": "release 41", "createdDate": _days(6)},
            {"id": "b3c4d5", "description": "release 40", "createdDate": _days(20)},
            {"id": "a2b3c4", "description": "initial", "createdDate": _days(45)},
        ],
        "stages": [
            {"stageName": "dev", "deploymentId": "e7f8a9", "createdDate": _days(45), "lastUpdatedDate": T0 - 7200,
             "variables": {"fn": "orders-sync", "LOG_LEVEL": "debug"},
             "methodSettings": {"*/*": {"metricsEnabled": False, "loggingLevel": "INFO", "dataTraceEnabled": True,
                                        "throttlingBurstLimit": 20, "throttlingRateLimit": 10.0}}},
            {"stageName": "qa", "deploymentId": "d5e6f7", "createdDate": _days(45), "lastUpdatedDate": _days(1),
             "variables": {"fn": "orders-sync"}},
            {"stageName": "prod", "deploymentId": "c4d5e6", "createdDate": _days(45), "lastUpdatedDate": _days(6),
             "description": "Production", "variables": {"fn": "orders-sync", "API_TOKEN": "sk_live_should_stay_masked"},
             "cacheClusterEnabled": True, "cacheClusterSize": "6.1", "cacheClusterStatus": "AVAILABLE", "tracingEnabled": True,
             "methodSettings": {"*/*": {"metricsEnabled": True, "loggingLevel": "ERROR", "dataTraceEnabled": False,
                                        "throttlingBurstLimit": 200, "throttlingRateLimit": 100.0,
                                        "cachingEnabled": True, "cacheTtlInSeconds": 300}},
             "accessLogSettings": {"destinationArn": f"arn:aws:logs:us-east-1:{ACCOUNT}:log-group:/aws/apigateway/public-api-prod",
                                   "format": "{\"requestId\":\"$context.requestId\",\"status\":\"$context.status\"}"}},
        ],
    },
    "r9s8t7": {
        "api": {"id": "r9s8t7", "name": "legacy-reports", "description": "Reports for the back office", "createdDate": _days(900),
                "apiKeySource": "HEADER", "endpointConfiguration": {"types": ["REGIONAL"]}},
        "resources": {
            "root1": ("/", {}),
            "rr01": ("/reports", {"GET": _method("AWS_IAM", integration=_proxy("report-builder"))}),
            "rr02": ("/reports/{id}", {"GET": _method("AWS_IAM", integration=_proxy("report-builder"),
                                                      params={"method.request.path.id": True})}),
        },
        "authorizers": [], "validators": [],
        "deployments": [{"id": "rd0002", "description": "nightly exports", "createdDate": _days(200)},
                        {"id": "rd0001", "description": "", "createdDate": _days(900)}],
        "stages": [{"stageName": "prod", "deploymentId": "rd0002", "createdDate": _days(900), "lastUpdatedDate": _days(200)}],
    },
    "p4q5r6": {
        "api": {"id": "p4q5r6", "name": "partner-api", "createdDate": _days(300), "apiKeySource": "HEADER",
                "endpointConfiguration": {"types": ["EDGE"]}, "policy": "{\"Version\":\"2012-10-17\"}"},
        "resources": {
            "root2": ("/", {}),
            "pp01": ("/catalog", {"GET": _method("NONE", integration=_proxy("orders-sync"), key=True)}),
        },
        "authorizers": [], "validators": [],
        "deployments": [{"id": "pd0001", "description": "v1", "createdDate": _days(300)}],
        "stages": [{"stageName": "v1", "deploymentId": "pd0001", "createdDate": _days(300)}],
    },
}

APIGW_V2 = {
    "h7x9k2": {
        "api": {"apiId": "h7x9k2", "name": "orders-http", "protocolType": "HTTP", "description": "Orders for the mobile app",
                "createdDate": s3_iso(_days(120)), "apiEndpoint": f"http://127.0.0.1:{APIGW_PORT}/__execute/h7x9k2",
                "disableExecuteApiEndpoint": False},
        "routes": [
            {"routeId": "rt1", "routeKey": "GET /orders", "target": "integrations/i1", "authorizationType": "JWT", "authorizerId": "jw1"},
            {"routeId": "rt2", "routeKey": "POST /orders", "target": "integrations/i2", "authorizationType": "JWT", "authorizerId": "jw1"},
            {"routeId": "rt3", "routeKey": "GET /orders/{id}", "target": "integrations/i1", "authorizationType": "NONE"},
            {"routeId": "rt4", "routeKey": "POST /notify", "target": "integrations/i3", "authorizationType": "NONE"},
            {"routeId": "rt5", "routeKey": "$default", "target": "integrations/i4", "authorizationType": "NONE"},
        ],
        "integrations": [
            {"integrationId": "i1", "integrationType": "AWS_PROXY", "integrationUri": f"arn:aws:lambda:us-east-1:{ACCOUNT}:function:orders-sync",
             "payloadFormatVersion": "2.0", "timeoutInMillis": 30000, "connectionType": "INTERNET"},
            {"integrationId": "i2", "integrationType": "AWS_PROXY", "integrationUri": f"arn:aws:lambda:us-east-1:{ACCOUNT}:function:payments-webhook",
             "payloadFormatVersion": "2.0", "timeoutInMillis": 10000},
            {"integrationId": "i3", "integrationType": "AWS_PROXY", "integrationSubtype": "SQS-SendMessage",
             "credentialsArn": f"arn:aws:iam::{ACCOUNT}:role/apigw-sqs", "payloadFormatVersion": "1.0",
             "requestParameters": {"QueueUrl": f"https://sqs.us-east-1.amazonaws.com/{ACCOUNT}/notifications", "MessageBody": "$request.body"}},
            {"integrationId": "i4", "integrationType": "HTTP_PROXY", "integrationUri": "https://legacy.example.com", "integrationMethod": "ANY",
             "payloadFormatVersion": "1.0", "timeoutInMillis": 15000},
        ],
        "authorizers": [{"authorizerId": "jw1", "name": "auth0", "authorizerType": "JWT", "identitySource": ["$request.header.Authorization"],
                         "jwtConfiguration": {"issuer": "https://acme.auth0.com/", "audience": ["orders"]}}],
        "deployments": [
            {"deploymentId": "dep3", "createdDate": s3_iso(T0 - 3600), "deploymentStatus": "DEPLOYED", "autoDeployed": True},
            {"deploymentId": "dep2", "createdDate": s3_iso(_days(3)), "deploymentStatus": "DEPLOYED", "description": "release 12"},
            {"deploymentId": "dep1", "createdDate": s3_iso(_days(10)), "deploymentStatus": "DEPLOYED", "description": "release 11"},
        ],
        "stages": [
            {"stageName": "$default", "deploymentId": "dep3", "autoDeploy": True, "createdDate": s3_iso(_days(120)),
             "lastUpdatedDate": s3_iso(T0 - 3600), "defaultRouteSettings": {"detailedMetricsEnabled": False},
             "lastDeploymentStatusMessage": "Successfully deployed stage with deployment ID 'dep3'"},
            {"stageName": "prod", "deploymentId": "dep2", "autoDeploy": False, "createdDate": s3_iso(_days(120)),
             "lastUpdatedDate": s3_iso(_days(3)), "stageVariables": {"tier": "gold"},
             "defaultRouteSettings": {"detailedMetricsEnabled": True, "throttlingBurstLimit": 500, "throttlingRateLimit": 250.0},
             "accessLogSettings": {"destinationArn": f"arn:aws:logs:us-east-1:{ACCOUNT}:log-group:/aws/apigateway/orders-http-prod",
                                   "format": "$context.requestId $context.routeKey $context.status"}},
        ],
    },
    "w5s4k3": {
        "api": {"apiId": "w5s4k3", "name": "chat-ws", "protocolType": "WEBSOCKET", "routeSelectionExpression": "$request.body.action",
                "createdDate": s3_iso(_days(60)), "apiEndpoint": "wss://w5s4k3.execute-api.us-east-1.amazonaws.com"},
        "routes": [
            {"routeId": "wr1", "routeKey": "$connect", "target": "integrations/wi1", "authorizationType": "CUSTOM", "authorizerId": "wa1"},
            {"routeId": "wr2", "routeKey": "$disconnect", "target": "integrations/wi1", "authorizationType": "NONE"},
            {"routeId": "wr3", "routeKey": "$default", "target": "integrations/wi1", "authorizationType": "NONE"},
            {"routeId": "wr4", "routeKey": "sendMessage", "target": "integrations/wi2", "authorizationType": "NONE"},
        ],
        "integrations": [
            {"integrationId": "wi1", "integrationType": "AWS_PROXY", "integrationUri": _lambda_uri("users-auth")},
            {"integrationId": "wi2", "integrationType": "AWS_PROXY", "integrationUri": _lambda_uri("image-resizer")},
        ],
        "authorizers": [{"authorizerId": "wa1", "name": "ws-auth", "authorizerType": "REQUEST",
                         "identitySource": ["route.request.querystring.token"], "authorizerUri": _lambda_uri("users-auth")}],
        "deployments": [{"deploymentId": "wd1", "createdDate": s3_iso(_days(15)), "deploymentStatus": "DEPLOYED"}],
        "stages": [{"stageName": "production", "deploymentId": "wd1", "createdDate": s3_iso(_days(60)),
                    "defaultRouteSettings": {"detailedMetricsEnabled": False}}],
    },
}

APIGW_DOMAINS = [
    {"domainName": "api.acme.dev", "domainNameConfigurations": [{"endpointType": "REGIONAL", "securityPolicy": "TLS_1_2",
                                                                 "domainNameStatus": "AVAILABLE",
                                                                 "apiGatewayDomainName": "d-abc123.execute-api.us-east-1.amazonaws.com"}],
     "mappings": [{"apiMappingId": "m1", "apiId": "h7x9k2", "stage": "prod", "apiMappingKey": ""},
                  {"apiMappingId": "m2", "apiId": "a1b2c3", "stage": "prod", "apiMappingKey": "v1"},
                  {"apiMappingId": "m3", "apiId": "r9s8t7", "stage": "prod", "apiMappingKey": "reports"}]},
    {"domainName": "partners.acme.dev", "domainNameConfigurations": [{"endpointType": "EDGE", "securityPolicy": "TLS_1_2",
                                                                      "domainNameStatus": "AVAILABLE",
                                                                      "apiGatewayDomainName": "d111111abcdef8.cloudfront.net"}],
     "mappings": [{"apiMappingId": "m4", "apiId": "p4q5r6", "stage": "v1", "apiMappingKey": ""}]},
]

APIGW_USAGE_PLANS = [{"id": "up1", "name": "partners-gold", "apiStages": [{"apiId": "p4q5r6", "stage": "v1"}],
                      "throttle": {"burstLimit": 50, "rateLimit": 20.0}}]

# Requests a minute per stage (the metric hook), and the route that fails now and then.
APIGW_TRAFFIC = {("public-api", "prod"): 120, ("public-api", "qa"): 5, ("public-api", "dev"): 2,
                 ("legacy-reports", "prod"): 400, ("partner-api", "v1"): 10,
                 ("h7x9k2", "prod"): 60, ("h7x9k2", "$default"): 3, ("w5s4k3", "production"): 30}
APIGW_FAILING = {"POST /orders"}

# API Gateway's own log groups join the Logs mock's: access logs (JSON) and REST execution logs.
APIGW_ACCESS_LINES = [
    '{{"requestId":"{rid}","ip":"10.0.{n}","httpMethod":"GET","resourcePath":"/orders","status":"200","responseLatency":"84"}}',
    '{{"requestId":"{rid}","ip":"10.0.{n}","httpMethod":"GET","resourcePath":"/orders/{{id}}","status":"200","responseLatency":"41"}}',
    '{{"requestId":"{rid}","ip":"10.0.{n}","httpMethod":"POST","resourcePath":"/orders","status":"502","responseLatency":"10012",'
    '"integrationError":"Lambda function timed out after 10000 ms"}}',
    '{{"requestId":"{rid}","ip":"10.0.{n}","httpMethod":"GET","resourcePath":"/health","status":"200","responseLatency":"3"}}',
    '{{"requestId":"{rid}","ip":"10.0.{n}","httpMethod":"POST","resourcePath":"/orders","status":"201","responseLatency":"230"}}',
    '{{"requestId":"{rid}","ip":"10.0.{n}","httpMethod":"GET","resourcePath":"/orders/{{id}}","status":"404","responseLatency":"39"}}',
]
APIGW_HTTP_ACCESS_LINES = ["{rid} GET /orders 200", "{rid} POST /orders 200", "{rid} GET /orders/{{id}} 200", "{rid} $default 502"]
APIGW_EXECUTION_LINES = [
    "({rid}) Extended Request Id: Xyz{n}=",
    "({rid}) HTTP Method: POST, Resource Path: /orders",
    "({rid}) Lambda execution failed with status 200 due to customer function error: payment provider did not answer in 3000 ms."
    " Lambda request id: {uuid}",
    "({rid}) Method completed with status: 502",
    "({rid}) Execution failed due to a timeout error",
    "({rid}) Method completed with status: 504",
    "({rid}) Successfully completed execution",
]
LINES_BY_GROUP.update({"/aws/apigateway/public-api-prod": APIGW_ACCESS_LINES, "/aws/apigateway/orders-http-prod": APIGW_HTTP_ACCESS_LINES,
                       "API-Gateway-Execution-Logs_a1b2c3/prod": APIGW_EXECUTION_LINES})
_log_groups_before_apigw = log_groups


def log_groups():  # noqa: F811
    return _log_groups_before_apigw() + ["/aws/apigateway/public-api-prod", "/aws/apigateway/orders-http-prod"]


def _apigw_json(status, obj, error=None):
    headers = {"content-type": "application/json"}
    if error:
        headers["x-amzn-errortype"] = error
    return status, headers, json.dumps(obj).encode()


def _apigw_not_found(what):
    return _apigw_json(404, {"message": f"Invalid {what} identifier specified"}, "NotFoundException")


def _rest_resources(api):
    out = []
    for rid, (path, methods) in api["resources"].items():
        parent = path.rsplit("/", 1)[0] or "/"
        item = {"id": rid, "path": path, "pathPart": path.rsplit("/", 1)[-1]}
        if path != "/":
            item["parentId"] = next((k for k, (p, _) in api["resources"].items() if p == parent), None)
        if methods:
            item["resourceMethods"] = {verb: {"httpMethod": verb, **m} for verb, m in methods.items()}
        out.append(item)
    return out


def _openapi(name, paths, yaml):
    doc = {"openapi": "3.0.1", "info": {"title": name, "version": "2026-10-21"}, "paths": {}}
    for method, path in paths:
        doc["paths"].setdefault(path, {})[method.lower() if method != "ANY" else "x-amazon-apigateway-any-method"] = {
            "responses": {"200": {"description": "200 response"}}}
    if not yaml:
        return json.dumps(doc, indent=2)
    lines = [f"openapi: \"{doc['openapi']}\"", "info:", f"  title: \"{name}\"", f"  version: \"{doc['info']['version']}\"", "paths:"]
    for path, methods in doc["paths"].items():
        lines.append(f"  {path}:")
        for m in methods:
            lines += [f"    {m}:", "      responses:", "        \"200\":", "          description: \"200 response\""]
    return "\n".join(lines) + "\n"


def _execution_log(rid, method, path, uri, status, latency, body):
    stamp = time.strftime("%a %b %d %H:%M:%S UTC %Y", time.gmtime())
    lines = [f"Execution log for request {rid}", f"{stamp} : Starting execution for request: {rid}",
             f"{stamp} : HTTP Method: {method}, Resource Path: {path}", f"{stamp} : Method request path: {{}}",
             f"{stamp} : Endpoint request URI: {uri}", f"{stamp} : Received response. Status: {status}, Integration latency: {latency} ms",
             f"{stamp} : Endpoint response body before transformations: {body[:200]}"]
    if status >= 500:
        lines.append(f"{stamp} : Execution failed due to configuration error: the integration returned an error")
    lines.append(f"{stamp} : Method completed with status: {status}")
    return "\n".join(lines) + "\n"


def _test_invoke(api, rid, verb, body):
    path, methods = api["resources"].get(rid, (None, {}))
    m = methods.get(verb) or methods.get("ANY")
    if m is None:
        return _apigw_not_found("Method")
    req = json.loads(body or b"{}")
    integration = m["methodIntegration"]
    request_id = str(uuid.uuid4())
    if integration["type"] == "MOCK":
        status, out = 200, '{"status": "ok"}'
    elif integration["type"] == "AWS_PROXY":
        fn = integration["uri"].split(":function:")[1].split("/")[0]
        payload = {"resource": path, "path": req.get("pathWithQueryString", path).split("?")[0], "httpMethod": verb,
                   "body": req.get("body"), "stageVariables": req.get("stageVariables")}
        if verb == "POST" and fn == "payments-webhook" and "fail" in (req.get("body") or ""):
            payload = {"fail": True}
        _, _, raw = lambda_invoke(f"/2015-03-31/functions/{fn}/invocations", {}, json.dumps(payload).encode())
        result = json.loads(raw)
        if "errorType" in result:
            status, out = 502, '{"message": "Internal server error"}'
        else:
            status, out = result.get("statusCode", 200), result.get("body", "")
    elif integration["type"] == "AWS":
        status, out = 200, json.dumps({"SendMessageResponse": {"SendMessageResult": {"MessageId": request_id}}})
    else:
        status, out = 502, '{"message": "Internal server error"}'
    latency = 182 if status < 500 else 3010
    log = _execution_log(request_id, verb, path, integration.get("uri", "MOCK"), status, latency, out)
    return _apigw_json(200, {"status": status, "body": out, "latency": latency + 14, "log": log,
                             "headers": {"Content-Type": "application/json", "X-Amzn-Trace-Id": f"Root=1-{request_id[:8]}"}})


def _execute(method, path, headers, body):
    """An HTTP API's invoke endpoint: /__execute/{apiId}[/{stage}]/path."""
    parts = path.split("/")[2:]
    api = APIGW_V2.get(parts[0]) if parts else None
    if api is None:
        return _apigw_json(404, {"message": "Not Found"})
    rest = parts[1:]
    stage = "$default"
    if rest and any(s["stageName"] == rest[0] for s in api["stages"]):
        stage, rest = rest[0], rest[1:]
    req_path = "/" + "/".join(rest)
    for r in api["routes"]:
        verb, _, template = r["routeKey"].partition(" ")
        pattern = "^" + re.sub(r"\{[^/]+\}", "[^/]+", template) + "$"
        if r["routeKey"] == "$default" or verb != method or not re.match(pattern, req_path):
            continue
        if r["authorizationType"] == "JWT" and not headers.get("authorization"):
            return _apigw_json(401, {"message": "Unauthorized"})
        if r["routeKey"] == "POST /notify":
            return _apigw_json(200, {"SendMessageResponse": {"MessageId": str(uuid.uuid4())}})
        return _apigw_json(200, {"route": r["routeKey"], "stage": stage, "path": req_path, "received": body.decode() or None})
    return _apigw_json(502, {"message": "Internal Server Error"})


def apigateway_rest(method, path, query, headers, body):
    """API Gateway requests. Returns (status, headers, body bytes)."""
    if path.startswith("/__execute/"):
        return _execute(method, path, headers, body)
    p = [unquote(x) for x in path.strip("/").split("/")]
    if p == ["restapis"]:
        return _apigw_json(200, {"item": [a["api"] for a in APIGW_REST.values()]})
    if p == ["usageplans"]:
        return _apigw_json(200, {"item": APIGW_USAGE_PLANS})
    if p[0] == "restapis":
        api = APIGW_REST.get(p[1])
        if api is None:
            return _apigw_not_found("API")
        what = p[2] if len(p) > 2 else ""
        if what == "resources" and len(p) == 3:
            return _apigw_json(200, {"item": _rest_resources(api)})
        if what == "resources" and len(p) == 6 and method == "POST":
            return _test_invoke(api, p[3], p[5], body)
        if what == "authorizers":
            return _apigw_json(200, {"item": api["authorizers"]})
        if what == "requestvalidators":
            return _apigw_json(200, {"item": api["validators"]})
        if what == "deployments" and method == "POST":
            req = json.loads(body or b"{}")
            dep = {"id": uuid.uuid4().hex[:6], "description": req.get("description", ""), "createdDate": time.time()}
            api["deployments"].insert(0, dep)
            for s in api["stages"]:
                if s["stageName"] == req.get("stageName"):
                    s.update({"deploymentId": dep["id"], "lastUpdatedDate": time.time()})
            return _apigw_json(201, dep)
        if what == "deployments":
            return _apigw_json(200, {"item": api["deployments"]})
        if what == "stages" and len(p) == 3:
            return _apigw_json(200, {"item": api["stages"]})
        stage = next((s for s in api["stages"] if len(p) > 3 and s["stageName"] == p[3]), None)
        if what == "stages" and stage is None:
            return _apigw_not_found("stage")
        if what == "stages" and method == "PATCH":
            for op in json.loads(body or b"{}").get("patchOperations", []):
                if op.get("path") == "/deploymentId":
                    if not any(d["id"] == op["value"] for d in api["deployments"]):
                        return _apigw_json(400, {"message": "Invalid deployment identifier specified"}, "BadRequestException")
                    stage.update({"deploymentId": op["value"], "lastUpdatedDate": time.time()})
            return _apigw_json(200, stage)
        if what == "stages" and len(p) == 6 and p[4] == "exports":
            paths = [(verb, rpath) for rpath, methods in api["resources"].values() for verb in methods]
            yaml = "yaml" in headers.get("accept", "")
            return 200, {"content-type": "application/yaml" if yaml else "application/json"}, _openapi(api["api"]["name"], paths, yaml).encode()
        return _apigw_json(404, {"message": f"mock: no route for {method} {path}"}, "NotFoundException")
    if p[:2] == ["v2", "domainnames"]:
        if len(p) == 2:
            return _apigw_json(200, {"items": [{k: v for k, v in d.items() if k != "mappings"} for d in APIGW_DOMAINS]})
        d = next((d for d in APIGW_DOMAINS if d["domainName"] == p[2]), None)
        if d is None:
            return _apigw_not_found("domain name")
        return _apigw_json(200, {"items": d["mappings"]})
    if p[:2] == ["v2", "apis"]:
        if len(p) == 2:
            return _apigw_json(200, {"items": [a["api"] for a in APIGW_V2.values()]})
        api = APIGW_V2.get(p[2])
        if api is None:
            return _apigw_not_found("API")
        what = p[3] if len(p) > 3 else ""
        if what in ("routes", "integrations", "authorizers") and len(p) == 4:
            return _apigw_json(200, {"items": api[what]})
        if what == "deployments" and method == "POST":
            req = json.loads(body or b"{}")
            stage = next((s for s in api["stages"] if s["stageName"] == req.get("stageName")), None)
            if stage and stage.get("autoDeploy"):
                return _apigw_json(400, {"message": "Stage with autoDeploy enabled cannot be deployed manually"}, "BadRequestException")
            dep = {"deploymentId": uuid.uuid4().hex[:6], "createdDate": s3_iso(time.time()), "deploymentStatus": "DEPLOYED",
                   "description": req.get("description", "")}
            api["deployments"].insert(0, dep)
            if stage:
                stage.update({"deploymentId": dep["deploymentId"], "lastUpdatedDate": s3_iso(time.time())})
            return _apigw_json(201, dep)
        if what == "deployments":
            return _apigw_json(200, {"items": api["deployments"]})
        if what == "stages" and len(p) == 4:
            return _apigw_json(200, {"items": api["stages"]})
        if what == "stages" and method == "PATCH":
            stage = next((s for s in api["stages"] if s["stageName"] == p[4]), None)
            if stage is None:
                return _apigw_not_found("stage")
            req = json.loads(body or b"{}")
            if "deploymentId" in req:
                if stage.get("autoDeploy"):
                    return _apigw_json(400, {"message": "DeploymentId cannot be set on a stage with autoDeploy enabled"}, "BadRequestException")
                stage.update({"deploymentId": req["deploymentId"], "lastUpdatedDate": s3_iso(time.time())})
            return _apigw_json(200, stage)
        if what == "exports":
            if api["api"]["protocolType"] != "HTTP":
                return _apigw_json(400, {"message": "Exporting WebSocket APIs is not supported"}, "BadRequestException")
            paths = [tuple(r["routeKey"].split(" ", 1)) for r in api["routes"] if " " in r["routeKey"]]
            yaml = query.get("outputType", "YAML") == "YAML"
            return 200, {"content-type": "application/octet-stream"}, _openapi(api["api"]["name"], paths, yaml).encode()
    return _apigw_json(404, {"message": f"mock: no route for {method} {path}"}, "NotFoundException")


def _apigw_metric(namespace, name, dims, stat, period, ts):
    """AWS/ApiGateway: steady traffic per stage, 0.3% server errors, and POST /orders (a slow
    payment provider) failing more, worst in the last 3 hours. Routes have numbers only on
    stages with detailed metrics."""
    if namespace != "AWS/ApiGateway":
        return None
    api_key = dims.get("ApiName") or dims.get("ApiId", "")
    stage = dims.get("Stage", "")
    route = dims.get("Route") or (f"{dims['Method']} {dims['Resource']}" if "Method" in dims else None)
    rate = APIGW_TRAFFIC.get((api_key, stage))
    if rate is None:
        return CW_NO_DATA
    if route:
        detailed = False
        for api in list(APIGW_REST.values()) + list(APIGW_V2.values()):
            if api["api"].get("name") == api_key or api["api"].get("apiId") == api_key:
                s = next((s for s in api["stages"] if s["stageName"] == stage), {})
                detailed = s.get("methodSettings", {}).get("*/*", {}).get("metricsEnabled") or \
                    s.get("defaultRouteSettings", {}).get("detailedMetricsEnabled")
        if not detailed:
            return CW_NO_DATA
        rate = rate / 4
    seed = int(hashlib.sha256(f"{name}{stat}{sorted(dims.items())}{int(ts)}".encode()).hexdigest()[:8], 16) / 0xFFFFFFFF
    wave = 1 + 0.5 * math.sin(ts / 86400 * 2 * math.pi)
    count = rate * period / 60 * wave * (0.9 + 0.2 * seed)
    failing = route in APIGW_FAILING or (route is None and api_key == "public-api" and stage == "prod")
    # The share of this period within the last 3 hours, when the failures started.
    now = time.time()
    spike = max(0.0, min(ts + period, now) - max(ts, now - 3 * 3600)) / period if failing else 0.0
    error_share = 0.003 + (0.08 if route else 0.02) * spike
    slow = 900 if (route in APIGW_FAILING) else 0
    values = {
        "Count": count, "MessageCount": count, "ConnectCount": count / 15,
        "5XXError": count * error_share, "5xx": count * error_share, "IntegrationError": count * 0.002, "ExecutionError": count * 0.001,
        "4XXError": count * 0.015, "4xx": count * 0.015, "ClientError": count * 0.01,
        "CacheHitCount": count * 0.05, "CacheMissCount": count * 0.95,
    }
    if name in values:
        return round(values[name], 2) if stat in ("Sum", "SampleCount") else 1.0
    if name in ("Latency", "IntegrationLatency"):
        base = {"p50": 60, "p90": 180, "p99": 420, "Maximum": 900}.get(stat, 85) + slow * (1 if stat in ("p99", "Maximum") else 0.3)
        own = 0 if name == "IntegrationLatency" else 18
        return round((base + own) * (0.85 + 0.3 * seed), 1)
    return None


CW_METRIC_HOOKS.append(_apigw_metric)


# ---- RDS (awsQuery, Version 2014-10-31) -------------------------------------------------------
RDS_ACCOUNT, RDS_REGION = "123456789012", "us-east-1"
RDS_NS = "http://rds.amazonaws.com/doc/2014-10-31/"
# Aurora clusters: orders-prod (busy, three zones, a custom endpoint, Performance Insights,
# heavy I/O on Standard storage), sessions-dev (Serverless v2 kept at its 4 ACU minimum)
# and analytics-old (idle, an Intel class). docs-legacy (DocumentDB) and legacy-mysql (a
# plain RDS instance) must not show up.
RDS_CLUSTERS = {
    "orders-prod": {
        "engine": "aurora-postgresql", "version": "16.4", "port": 5432, "user": "orders_admin", "db": "orders",
        "storage": "aurora", "iam": True, "protect": True, "backups": 7, "age": 420, "serverless": None,
        # (id, class, zone, writer, promotion tier, Performance Insights)
        "instances": [("orders-1", "db.r6g.xlarge", "us-east-1a", True, 1, True),
                      ("orders-2", "db.r6g.xlarge", "us-east-1b", False, 1, True),
                      ("orders-3", "db.r5.large", "us-east-1c", False, 2, True)],
        "custom": [("reporting", "READER", ["orders-3"], [])],
    },
    "sessions-dev": {
        "engine": "aurora-mysql", "version": "8.0.mysql_aurora.3.05.2", "port": 3306, "user": "admin", "db": "sessions",
        "storage": "aurora", "iam": False, "protect": False, "backups": 1, "age": 90, "serverless": (4.0, 16.0),
        "instances": [("sessions-dev-1", "db.serverless", "us-east-1a", True, 1, False)],
        "custom": [],
    },
    "analytics-old": {
        "engine": "aurora-mysql", "version": "5.7.mysql_aurora.2.11.2", "port": 3306, "user": "admin", "db": "analytics",
        "storage": "aurora", "iam": False, "protect": False, "backups": 1, "age": 1100, "serverless": None,
        "instances": [("analytics-old-1", "db.r5.large", "us-east-1b", True, 1, False)],
        "custom": [],
    },
}
# (snapshot, cluster, days old, GB)
RDS_SNAPSHOTS = [("analytics-old-final-2025", "analytics-old", 400, 120), ("orders-prod-before-migration", "orders-prod", 200, 350),
                 ("orders-prod-yesterday", "orders-prod", 1, 410)]


def rds_x(tag, value):
    """An element; None leaves it out. ESC characters travel as &#x1b; as AWS would send them."""
    if value is None:
        return ""
    if isinstance(value, bool):
        value = "true" if value else "false"
    return f"<{tag}>{xml_escape(str(value)).replace(chr(27), '&#x1b;').replace(chr(7), '&#x7;')}</{tag}>"


def rds_iso(t):
    return time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(t))


def rds_resource_id(name):
    return "db-" + hashlib.sha256(name.encode()).hexdigest()[:26].upper()


def rds_host(name, kind="cluster"):
    return f"{name}.{kind}-c7mock1x2y3z.{RDS_REGION}.rds.amazonaws.com"


def rds_cluster_arn(name):
    return f"arn:aws:rds:{RDS_REGION}:{RDS_ACCOUNT}:cluster:{name}"


def rds_family(c):
    return "aurora-postgresql16" if "postgres" in c["engine"] else "aurora-mysql8.0"


def rds_cluster_xml(name, c):
    members = "".join(
        f"<DBClusterMember>{rds_x('DBInstanceIdentifier', i)}{rds_x('IsClusterWriter', w)}"
        f"{rds_x('DBClusterParameterGroupStatus', 'pending-reboot' if i == 'orders-3' else 'in-sync')}{rds_x('PromotionTier', t)}</DBClusterMember>"
        for i, _, _, w, t, _ in c["instances"])
    serverless = (f"<ServerlessV2ScalingConfiguration>{rds_x('MinCapacity', c['serverless'][0])}{rds_x('MaxCapacity', c['serverless'][1])}"
                  f"</ServerlessV2ScalingConfiguration>" if c["serverless"] else "")
    customs = "".join(f"<member>{rds_host(n, 'cluster-custom')}</member>" for n, *_ in c["custom"])
    return (f"<DBCluster>{rds_x('AllocatedStorage', 1)}<AvailabilityZones><AvailabilityZone>us-east-1a</AvailabilityZone>"
            f"<AvailabilityZone>us-east-1b</AvailabilityZone><AvailabilityZone>us-east-1c</AvailabilityZone></AvailabilityZones>"
            f"{rds_x('BackupRetentionPeriod', c['backups'])}{rds_x('DatabaseName', c['db'])}{rds_x('DBClusterIdentifier', name)}"
            f"{rds_x('DBClusterParameterGroup', 'default.' + rds_family(c))}{rds_x('DBSubnetGroup', 'default')}{rds_x('Status', 'available')}"
            f"{rds_x('Endpoint', rds_host(name))}{rds_x('ReaderEndpoint', rds_host(name, 'cluster-ro'))}<CustomEndpoints>{customs}</CustomEndpoints>"
            f"{rds_x('MultiAZ', len(c['instances']) > 1)}{rds_x('Engine', c['engine'])}{rds_x('EngineVersion', c['version'])}"
            f"{rds_x('Port', c['port'])}{rds_x('MasterUsername', c['user'])}"
            f"<DBClusterOptionGroupMemberships><DBClusterOptionGroupMembership><DBClusterOptionGroupName>default</DBClusterOptionGroupName>"
            f"<Status>pending-apply</Status></DBClusterOptionGroupMembership></DBClusterOptionGroupMemberships>"
            f"{rds_x('PreferredBackupWindow', '07:00-07:30')}{rds_x('PreferredMaintenanceWindow', 'sun:08:00-sun:08:30')}"
            f"<DBClusterMembers>{members}</DBClusterMembers>"
            f"<VpcSecurityGroups><VpcSecurityGroupMembership><VpcSecurityGroupId>sg-0a1b2c</VpcSecurityGroupId><Status>active</Status>"
            f"</VpcSecurityGroupMembership></VpcSecurityGroups>{rds_x('StorageEncrypted', True)}"
            f"{rds_x('DbClusterResourceId', 'cluster-' + rds_resource_id(name)[3:])}{rds_x('DBClusterArn', rds_cluster_arn(name))}"
            f"{rds_x('IAMDatabaseAuthenticationEnabled', c['iam'])}{rds_x('ClusterCreateTime', rds_iso(T0 - 86400 * c['age']))}"
            f"{rds_x('EngineMode', 'provisioned')}{rds_x('DeletionProtection', c['protect'])}{rds_x('StorageType', c['storage'])}"
            f"{serverless}</DBCluster>")


def rds_instances():
    """(cluster name or None, instance XML) for every instance, Aurora or not."""
    out = []
    for name, c in RDS_CLUSTERS.items():
        for i, cls, zone, _, tier, pi in c["instances"]:
            pending = ("<PendingModifiedValues><DBInstanceClass>db.r6g.xlarge</DBInstanceClass></PendingModifiedValues>"
                       if i == "orders-3" else "<PendingModifiedValues/>")
            out.append((name, f"<DBInstance>{rds_x('DBInstanceIdentifier', i)}{rds_x('DBInstanceClass', cls)}{rds_x('Engine', c['engine'])}"
                        f"{rds_x('DBInstanceStatus', 'available')}{rds_x('MasterUsername', c['user'])}"
                        f"<Endpoint>{rds_x('Address', rds_host(i, 'instance'))}{rds_x('Port', c['port'])}<HostedZoneId>Z2R2ITUGPM61AM</HostedZoneId></Endpoint>"
                        f"{rds_x('InstanceCreateTime', rds_iso(T0 - 86400 * c['age']))}"
                        f"<DBParameterGroups><DBParameterGroup>{rds_x('DBParameterGroupName', 'default.' + rds_family(c))}"
                        f"{rds_x('ParameterApplyStatus', 'pending-reboot' if i == 'orders-3' else 'in-sync')}</DBParameterGroup></DBParameterGroups>"
                        f"{rds_x('AvailabilityZone', zone)}<DBSubnetGroup><DBSubnetGroupName>default</DBSubnetGroupName><Subnets><Subnet>"
                        f"<SubnetIdentifier>subnet-1</SubnetIdentifier><SubnetAvailabilityZone><Name>us-east-1f</Name></SubnetAvailabilityZone>"
                        f"<SubnetStatus>Active</SubnetStatus></Subnet></Subnets></DBSubnetGroup>{pending}"
                        f"{rds_x('EngineVersion', c['version'])}{rds_x('AutoMinorVersionUpgrade', True)}{rds_x('PubliclyAccessible', False)}"
                        f"<StatusInfos/>{rds_x('DBClusterIdentifier', name)}{rds_x('StorageEncrypted', True)}"
                        f"{rds_x('DbiResourceId', rds_resource_id(i))}{rds_x('CACertificateIdentifier', 'rds-ca-rsa2048-g1')}"
                        f"{rds_x('PromotionTier', tier)}{rds_x('DBInstanceArn', f'arn:aws:rds:{RDS_REGION}:{RDS_ACCOUNT}:db:{i}')}"
                        f"{rds_x('MonitoringInterval', 60 if pi else 0)}{rds_x('PerformanceInsightsEnabled', pi)}"
                        f"{rds_x('PerformanceInsightsRetentionPeriod', 7 if pi else None)}</DBInstance>"))
    out.append((None, f"<DBInstance>{rds_x('DBInstanceIdentifier', 'legacy-mysql')}{rds_x('DBInstanceClass', 'db.m5.large')}"
                      f"{rds_x('Engine', 'mysql')}{rds_x('DBInstanceStatus', 'available')}</DBInstance>"))
    return out


def rds_endpoints(name, c):
    rows = [(None, "WRITER", None, rds_host(name), [], []), (None, "READER", None, rds_host(name, "cluster-ro"), [], [])]
    rows += [(n, "CUSTOM", kind, rds_host(n, "cluster-custom"), static, excluded)
             for n, kind, static, excluded in c["custom"]]
    return "".join(
        f"<DBClusterEndpointList>{rds_x('DBClusterEndpointIdentifier', ident)}{rds_x('DBClusterIdentifier', name)}"
        f"{rds_x('Endpoint', host)}{rds_x('Status', 'available')}{rds_x('EndpointType', kind)}{rds_x('CustomEndpointType', custom)}"
        f"<StaticMembers>{''.join(f'<member>{m}</member>' for m in static)}</StaticMembers>"
        f"<ExcludedMembers>{''.join(f'<member>{m}</member>' for m in excluded)}</ExcludedMembers></DBClusterEndpointList>"
        for ident, kind, custom, host, static, excluded in rows)


# (seconds ago, source, source type, categories, message)
RDS_EVENTS = [
    (60 * 40, "orders-3", "db-instance", ["notification"], "Backup completed \x1b[31mwith warnings\x1b[0m"),
    (3600 * 5, "sessions-dev", "db-cluster", ["notification"], "Scaled DB cluster capacity from 4.0 to 4.0 ACUs (minimum capacity)."),
    (86400 * 2 + 30, "orders-prod", "db-cluster", ["failover"], "Started cross AZ failover to DB instance: orders-1"),
    (86400 * 2, "orders-prod", "db-cluster", ["failover"], "Completed failover to DB instance: orders-1"),
    (86400 * 2 - 20, "orders-2", "db-instance", ["availability"], "DB instance restarted"),
    (86400 * 3, "orders-3", "db-instance", ["configuration change"], "Updated to use DBParameterGroup default.aurora-postgresql16"),
    (86400 * 4, "analytics-old-1", "db-instance", ["backup"], "Automated backup completed"),
    (86400 * 6, "docs-legacy", "db-cluster", ["maintenance"], "DocumentDB cluster patched"),
    (86400 * 20, "orders-prod", "db-cluster", ["maintenance"], "Too old to be listed"),
]


def rds_events(params):
    minutes = int(params.get("Duration", ["60"])[0])
    rows = [e for e in RDS_EVENTS if e[0] <= minutes * 60]
    return "".join(
        f"<Event>{rds_x('SourceIdentifier', src)}{rds_x('SourceType', kind)}{rds_x('Message', msg)}"
        f"<EventCategories>{''.join(rds_x('EventCategory', c) for c in cats)}</EventCategories>"
        f"{rds_x('Date', rds_iso(T0 - ago))}{rds_x('SourceArn', f'arn:aws:rds:{RDS_REGION}:{RDS_ACCOUNT}:{src}')}</Event>"
        for ago, src, kind, cats, msg in rows)


def rds_pending():
    actions = [(f"arn:aws:rds:{RDS_REGION}:{RDS_ACCOUNT}:db:orders-2", "system-update", "New Operating System update is available", 5, None),
               (rds_cluster_arn("analytics-old"), "db-upgrade", "Upgrade to Aurora MySQL 3: version 2 is past the end of standard support",
                None, 30)]
    return "".join(
        f"<ResourcePendingMaintenanceActions>{rds_x('ResourceIdentifier', arn)}<PendingMaintenanceActionDetails><PendingMaintenanceAction>"
        f"{rds_x('Action', action)}{rds_x('Description', desc)}"
        f"{rds_x('AutoAppliedAfterDate', rds_iso(T0 + 86400 * auto) if auto else None)}"
        f"{rds_x('ForcedApplyDate', rds_iso(T0 + 86400 * forced) if forced else None)}"
        f"</PendingMaintenanceAction></PendingMaintenanceActionDetails></ResourcePendingMaintenanceActions>"
        for arn, action, desc, auto, forced in actions)


def rds_xml(action, inner):
    return (f'<{action}Response xmlns="{RDS_NS}"><{action}Result>{inner}</{action}Result>'
            f"<ResponseMetadata><RequestId>{eid()}</RequestId></ResponseMetadata></{action}Response>")


def rds(params):
    """awsQuery: form-encoded request, XML response (status, body)."""
    get = lambda k, d="": params.get(k, [d])[0]
    action = get("Action")
    if action == "DescribeDBClusters":
        only = get("DBClusterIdentifier")
        if only and only not in RDS_CLUSTERS:
            return 404, query_error("DBClusterNotFoundFault", f"DBCluster {xml_escape(only)} not found.")
        body = "".join(rds_cluster_xml(n, c) for n, c in RDS_CLUSTERS.items() if not only or n == only)
        if not only:
            body += f"<DBCluster>{rds_x('DBClusterIdentifier', 'docs-legacy')}{rds_x('Engine', 'docdb')}{rds_x('Status', 'available')}</DBCluster>"
        return 200, rds_xml(action, f"<DBClusters>{body}</DBClusters>")
    if action == "DescribeDBInstances":
        only = get("Filters.Filter.1.Values.Value.1") if get("Filters.Filter.1.Name") == "db-cluster-id" else ""
        rows = [x for c, x in rds_instances() if not only or c == only]
        # Three per page, to exercise Marker.
        start = int(get("Marker") or 0)
        marker = rds_x("Marker", start + 3) if start + 3 < len(rows) else ""
        return 200, rds_xml(action, f"<DBInstances>{''.join(rows[start:start + 3])}</DBInstances>{marker}")
    if action == "DescribeDBClusterEndpoints":
        only = get("DBClusterIdentifier")
        body = "".join(rds_endpoints(n, c) for n, c in RDS_CLUSTERS.items() if not only or n == only)
        return 200, rds_xml(action, f"<DBClusterEndpoints>{body}</DBClusterEndpoints>")
    if action == "DescribeGlobalClusters":
        members = (f"<GlobalClusterMember>{rds_x('DBClusterArn', rds_cluster_arn('orders-prod'))}{rds_x('IsWriter', True)}</GlobalClusterMember>"
                   f"<GlobalClusterMember>{rds_x('DBClusterArn', 'arn:aws:rds:eu-west-1:123456789012:cluster:orders-eu')}"
                   f"{rds_x('IsWriter', False)}</GlobalClusterMember>")
        return 200, rds_xml(action, f"<GlobalClusters><GlobalClusterMember>{rds_x('GlobalClusterIdentifier', 'orders-global')}"
                                    f"<GlobalClusterMembers>{members}</GlobalClusterMembers></GlobalClusterMember></GlobalClusters>")
    if action == "DescribeEvents":
        return 200, rds_xml(action, f"<Events>{rds_events(params)}</Events>")
    if action == "DescribePendingMaintenanceActions":
        return 200, rds_xml(action, f"<PendingMaintenanceActions>{rds_pending()}</PendingMaintenanceActions>")
    if action == "DescribeDBClusterSnapshots":
        body = "".join(
            f"<DBClusterSnapshot>{rds_x('DBClusterSnapshotIdentifier', s)}{rds_x('DBClusterIdentifier', c)}"
            f"{rds_x('SnapshotCreateTime', rds_iso(T0 - 86400 * days))}{rds_x('Engine', RDS_CLUSTERS[c]['engine'])}"
            f"{rds_x('AllocatedStorage', gb)}{rds_x('Status', 'available')}{rds_x('SnapshotType', 'manual')}</DBClusterSnapshot>"
            for s, c, days, gb in RDS_SNAPSHOTS)
        return 200, rds_xml(action, f"<DBClusterSnapshots>{body}</DBClusterSnapshots>")
    return 400, query_error("InvalidAction", f"{action} is not in the mock yet")


class RdsExact(float):
    """A value cw_metrics' rounding to 2 decimals leaves alone: latencies are in seconds."""

    def __round__(self, ndigits=None):
        return self


def rds_instance_info(name):
    """(cluster name, cluster, instance tuple) of an instance."""
    for cname, c in RDS_CLUSTERS.items():
        for inst in c["instances"]:
            if inst[0] == name:
                return cname, c, inst
    return None


def rds_metric(namespace, name, dims, stat, period, ts):
    """AWS/RDS: the writer busiest (CPU past 75 % in the afternoon), readers lagging a little,
    the reporting reader busy at night, sessions-dev pinned at its 4 ACU minimum, analytics-old
    idle."""
    if namespace != "AWS/RDS":
        return None
    seed = int(hashlib.sha256(f"{name}{sorted(dims.items())}{int(ts)}".encode()).hexdigest()[:8], 16) / 0xFFFFFFFF
    day = ts / 86400 * 2 * math.pi
    wave = 1 + 0.6 * math.sin(day)
    if "DBClusterIdentifier" in dims:
        cluster = dims["DBClusterIdentifier"]
        volume = {"orders-prod": 412e9, "sessions-dev": 6e9, "analytics-old": 120e9}.get(cluster)
        if volume is None:
            return CW_NO_DATA
        if name == "VolumeBytesUsed":
            return volume
        if name in ("VolumeReadIOPs", "VolumeWriteIOPs"):
            per5 = {"orders-prod": (1.6e6, 4.2e5), "sessions-dev": (2e3, 1.5e3), "analytics-old": (0, 0)}[cluster]
            v = per5[0 if name == "VolumeReadIOPs" else 1] * wave * (0.8 + 0.4 * seed)
            return v * period / 300 if stat == "Sum" else v
        return CW_NO_DATA
    info = rds_instance_info(dims.get("DBInstanceIdentifier", ""))
    if not info:
        return CW_NO_DATA
    _, _, (iid, cls, _, writer, _, pi) = info
    night = max(0.0, math.sin(day + math.pi))
    peak = 1.25 if stat == "Maximum" else 1.0
    if name == "AuroraReplicaLag":
        if writer:
            return CW_NO_DATA
        spike = 1400 if seed > 0.985 else 0
        return round(18 + 14 * seed + spike + (10 if iid == "orders-3" else 0), 1)
    if name in ("ServerlessDatabaseCapacity", "ACUUtilization"):
        if cls != "db.serverless":
            return CW_NO_DATA
        return 4.0 if name == "ServerlessDatabaseCapacity" else 25.0
    if name == "DBLoad":
        if not pi:
            return CW_NO_DATA
        return round((2.6 * wave + 0.8 * seed) * peak, 2) if iid == "orders-1" else round(0.6 * wave * seed + 0.3, 2)
    profile = {
        # CPU %, connections, freeable GiB, read IOPS, write IOPS
        "orders-1": (min(98.0, (44 + 34 * math.sin(day) + 8 * seed) * peak), 180 * wave + 30 * seed, 9.5, 1200 * wave, 800 * wave),
        "orders-2": ((14 + 6 * wave + 4 * seed) * peak, 55 * wave + 10 * seed, 21.0, 900 * wave, 15),
        "orders-3": ((12 + 50 * night + 4 * seed) * peak, 8 + 4 * seed, 7.5, 300 * (1 + night), 10),
        "sessions-dev-1": ((5 + 3 * seed) * peak, 3 + 3 * seed, 7.0, 40 * wave, 30 * wave),
        "analytics-old-1": ((1.5 + seed) * peak, 0, 13.2, 0.2, 0.4),
    }[iid]
    cpu, conns, free_gib, read_iops, write_iops = profile
    values = {
        "CPUUtilization": round(max(0.5, cpu), 1),
        "DatabaseConnections": round(max(0.0, conns) * peak),
        "FreeableMemory": (free_gib - 1.5 * seed) * 1024 ** 3,
        "ReadLatency": RdsExact(0.0009 + 0.0008 * seed),
        "WriteLatency": RdsExact(0.0018 + 0.0015 * seed if writer else 0.0),
        "ReadIOPS": round(read_iops + 50 * seed, 1),
        "WriteIOPS": round(write_iops + 20 * seed, 1),
        "BufferCacheHitRatio": round(99.6 - (4.5 * seed if iid == "orders-1" and seed > 0.9 else 0.4 * seed), 2),
        "CommitLatency": round(2.2 + 1.5 * seed, 2) if writer else 0.0,
        "Deadlocks": 0.02 if iid == "orders-1" and seed > 0.97 else 0.0,
    }
    return values.get(name, CW_NO_DATA)


CW_METRIC_HOOKS.append(rds_metric)


# ---- Performance Insights (awsJson1_1, X-Amz-Target PerformanceInsightsv20180227.*) -----------
# Per instance: (tokenized statement, load in sessions, {wait type: share}).
PI_SQL = {
    "orders-1": [
        ("SELECT o.id, o.status, o.total FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.email = ? "
         "AND o.created_at > ? ORDER BY o.created_at DESC LIMIT ?", 1.12, {"CPU": 0.65, "IO": 0.35}),
        ("UPDATE inventory SET reserved = reserved + ? WHERE sku = ? AND warehouse_id = ?", 0.61, {"Lock": 0.75, "CPU": 0.25}),
        ("INSERT INTO order_events (order_id, kind, payload, created_at) VALUES (?, ?, ?, ?)", 0.34, {"IO": 0.7, "CPU": 0.3}),
        ("SELECT * FROM audit_log WHERE note = ? /* \x1b]52;c;aGk=\x07 copied */", 0.18, {"CPU": 1.0}),
        ("SELECT p.sku, p.name, sum(l.quantity) AS sold\nFROM order_lines l\nJOIN products p ON p.sku = l.sku\n"
         "WHERE l.created_at BETWEEN ? AND ?\nGROUP BY p.sku, p.name\nORDER BY sold DESC\nLIMIT ?", 0.09, {"IO": 0.8, "CPU": 0.2}),
    ],
    "orders-2": [
        ("SELECT id, status FROM orders WHERE id = ?", 0.42, {"CPU": 0.9, "IO": 0.1}),
        ("SELECT count(*) FROM shipments WHERE order_id = ? AND state <> ?", 0.12, {"IO": 0.6, "CPU": 0.4}),
    ],
    "orders-3": [
        ("WITH daily AS (SELECT date_trunc(?, created_at) AS day, sum(total) AS revenue FROM orders WHERE created_at > ? "
         "GROUP BY ?) SELECT day, revenue, revenue - lag(revenue) OVER (ORDER BY day) AS change FROM daily ORDER BY day", 0.75,
         {"IO": 0.85, "CPU": 0.15}),
    ],
}
PI_WAITS = {
    "orders-1": [("CPU", "CPU", 1.21), ("IO:DataFileRead", "IO", 0.52), ("Lock:transactionid", "Lock", 0.46),
                 ("IO:XactSync", "IO", 0.21), ("Client:ClientRead", "Client", 0.08)],
    "orders-2": [("CPU", "CPU", 0.41), ("IO:DataFileRead", "IO", 0.13)],
    "orders-3": [("IO:DataFileRead", "IO", 0.64), ("CPU", "CPU", 0.12)],
}


def pi_instance(body):
    ident = body.get("Identifier", "")
    for c in RDS_CLUSTERS.values():
        for iid, *_, pi in c["instances"]:
            if rds_resource_id(iid) == ident:
                if not pi:
                    raise KeyError(f"InvalidArgumentException: Performance Insights is not enabled for {ident}")
                return iid
    raise KeyError(f"InvalidArgumentException: No resource {ident}")


def performance_insights(op, body):
    iid = pi_instance(body)
    start, end = body.get("StartTime", T0 - 3600), body.get("EndTime", T0)
    if op == "DescribeDimensionKeys":
        group, limit = body["GroupBy"]["Group"], body["GroupBy"].get("Limit", 10)
        if group == "db.sql_tokenized":
            types = ["CPU", "IO", "Lock"]
            keys = [{"Dimensions": {"db.sql_tokenized.statement": sql, "db.sql_tokenized.id": hashlib.md5(sql.encode()).hexdigest()[:16].upper(),
                                    "db.sql_tokenized.db_id": f"pi-{iid}"},
                     "Total": load, "Partitions": [round(load * split.get(t, 0), 3) for t in types]}
                    for sql, load, split in PI_SQL.get(iid, [])[:limit]]
            return {"AlignedStartTime": start, "AlignedEndTime": end, "Keys": keys,
                    "PartitionKeys": [{"Dimensions": {"db.wait_event_type.name": t}} for t in types]}
        if group == "db.wait_event":
            keys = [{"Dimensions": {"db.wait_event.name": n, "db.wait_event.type": t}, "Total": load}
                    for n, t, load in PI_WAITS.get(iid, [])[:limit]]
            return {"AlignedStartTime": start, "AlignedEndTime": end, "Keys": keys}
        raise KeyError(f"InvalidArgumentException: group {group} is not in the mock")
    if op == "GetResourceMetrics":
        period = body.get("PeriodInSeconds", 60)
        points = [{"Timestamp": t, "Value": rds_metric("AWS/RDS", "DBLoad", {"DBInstanceIdentifier": iid}, "Average", period, t)}
                  for t in range(int(start) // period * period, int(end), period)][-1440:]
        return {"AlignedStartTime": start, "AlignedEndTime": end, "Identifier": body["Identifier"],
                "MetricList": [{"Key": {"Metric": "db.load.avg"}, "DataPoints": points}]}
    raise KeyError(f"InvalidArgumentException: {op} is not in the mock yet")


def is_appconfig(path):
    return path.startswith("/applications") or path.startswith("/deploymentstrategies")


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code, obj):
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def check_sig(self):
        auth = self.headers.get("authorization", "")
        SIGNED["ok" if auth.startswith("AWS4-HMAC-SHA256 Credential=AKIDTEST/") and "Signature=" in auth else "unsigned"] += 1

    def reply_raw(self, code, headers, data):
        self.send_response(code)
        for k, v in headers.items():
            self.send_header(k, v)
        if "content-length" not in {k.lower() for k in headers}:
            self.send_header("content-length", str(len(data)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    def raw_body(self):
        return self.rfile.read(int(self.headers.get("content-length", 0) or 0))

    def route_rest(self, method, body=b""):
        """S3 and AppConfig requests (anything that is not Lambda or a JSON target)."""
        path, _, qs = self.path.partition("?")
        query = {k: v[0] for k, v in parse_qs(qs, keep_blank_values=True).items()}
        if is_appconfig(path):
            return self.reply_raw(*appconfig_rest(method, path, query, self.headers, body))
        if is_apigateway(path):
            return self.reply_raw(*apigateway_rest(method, path, query, self.headers, body))
        return self.reply_raw(*s3_rest(method, path, query, self.headers))

    def do_GET(self):
        self.check_sig()
        if self.path.startswith("/__stats"):
            return self.reply(200, SIGNED)
        if self.path.startswith("/2019-09-25/functions/"):
            return self.reply(*lambda_event_invoke_config(self.path))
        if self.path.startswith("/2019-09-30/functions/"):
            return self.reply(*lambda_provisioned_concurrency(self.path))
        if not self.path.startswith("/2015-03-31/"):
            return self.route_rest("GET")
        try:
            self.reply(200, lambda_rest(self.path))
        except Exception as e:
            self.reply(404, {"Type": "User", "message": f"Function not found: {e}"})

    def do_HEAD(self):
        self.check_sig()
        return self.route_rest("HEAD")

    def do_DELETE(self):
        self.check_sig()
        return self.route_rest("DELETE", self.raw_body())

    def do_PATCH(self):
        self.check_sig()
        return self.route_rest("PATCH", self.raw_body())

    def reply_xml(self, code, text):
        data = text.encode()
        self.send_response(code)
        self.send_header("content-type", "text/xml")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        self.check_sig()
        raw = self.raw_body()
        if self.path.startswith("/2015-03-31/functions/") and "/invocations" in self.path:
            return self.reply_raw(*lambda_invoke(self.path, self.headers, raw))
        if is_appconfig(self.path) or is_apigateway(self.path):
            return self.route_rest("POST", raw)
        if self.headers.get("content-type", "").startswith("application/x-www-form-urlencoded"):
            # awsQuery services, told apart by their API version.
            params = parse_qs(raw.decode())
            service = {"2010-08-01": cloudwatch, "2014-10-31": rds}.get(params.get("Version", [""])[0], cloudformation)
            return self.reply_xml(*service(params))
        body = json.loads(raw or b"{}")
        target = self.headers.get("x-amz-target", "")
        prefix, _, op = target.partition(".")
        handler = {"CodePipeline_20150709": codepipeline, "CodeBuild_20161006": codebuild,
                   "Logs_20140328": cwlogs, "AmazonSSM": ssm, "secretsmanager": secretsmanager,
                   "AWSStepFunctions": stepfunctions, "DynamoDB_20120810": dynamodb, "AWSEvents": eventbridge,
                   "AmazonSQS": sqs, "PerformanceInsightsv20180227": performance_insights}.get(prefix)
        try:
            self.reply(200, handler(op, body))
        except Exception as e:
            # KeyError("ResourceNotFoundException: …") → that error code, like AWS.
            first = str(e.args[0]).split(":")[0] if e.args else ""
            code = first if first[:1].isupper() and " " not in first else "ValidationException"
            self.reply(400, {"__type": f"com.amazon.coral#{code}", "message": f"{target}: {e!r}"})


if __name__ == "__main__":
    port = int(sys.argv[1])
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()

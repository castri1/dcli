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


def revision(name, k):
    """Source revision of the current run (k=0) or the k-th history entry. A pipeline's
    "lag" makes it run older commits, like a later environment of the same app."""
    k += PIPES.get(name, {}).get("lag", 0)
    sha, msg = COMMITS[min(k, len(COMMITS) - 1)]
    return [{"actionName": "Source", "revisionId": sha,
             "revisionUrl": f"https://github.com/acme/{name}/commit/{sha}",
             "revisionSummary": json.dumps({"ProviderType": "GitHub", "CommitMessage": msg})}]


new_run("app-dev", 120)
new_run("api-dev", 600, "approval")
new_run("worker-dev", 400, "fail")
PIPES["worker-dev"]["history"][0] = (eid(), "Failed", time.time() - 86400 - 3600, 300)
new_run("infra-dev", 900, "cfnfail")
new_run("app-prod", 0, "idle")
PIPES["app-prod"]["exec"] = None
PIPES["app-prod"]["lag"] = 2
new_run("app-qa", 0, "idle")
PIPES["app-qa"]["exec"] = None
PIPES["app-qa"]["lag"] = 1
# In the account but not in demo-config.yaml: found with `n` (add pipelines); they get a
# finished run when first looked at.
UNLISTED = ["legacy-batch", "billing-dev", "billing-prod", "search-indexer-dev", "notifications-v2-dev",
            "support-ai-buyer-dev", "support-ai-seller-dev", "data-export-nightly"]


def timeline(p):
    """Per stage: (status, start, end) for the current execution."""
    now = time.time()
    t = p["start"]
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
    sts = [v[0] for v in tl.values()]
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
    return {
        "pipelineExecutionId": p["exec"], "status": exec_status(p), "startTime": p["start"],
        "lastUpdateTime": max(ends) if exec_status(p) != "InProgress" and ends else time.time(),
        "trigger": {"triggerType": p["trigger"]},
        "sourceRevisions": revision(name, 0),
    }


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
             "trigger": {"triggerType": "StartPipelineExecution"}, "sourceRevisions": revision(name, k + 1)}
            for k, (h, st, s, d) in enumerate(p["history"])]
        return {"pipelineExecutionSummaries": items[: body.get("maxResults", 10)]}
    if op == "GetPipelineExecution":
        return {"pipelineExecution": summary(name, PIPES[name])}
    if op == "ListActionExecutions":
        return {"actionExecutionDetails": action_details(name, (body.get("filter") or {}).get("pipelineExecutionId"))}
    if op == "StartPipelineExecution":
        old = PIPES[name]
        new_run(name, 0, "approval" if name == "api-dev" else "run", "StartPipelineExecution")
        PIPES[name]["history"] = old["history"]
        return {"pipelineExecutionId": PIPES[name]["exec"]}
    if op == "StopPipelineExecution":
        PIPES[name]["stopped"] = True
        return {"pipelineExecutionId": PIPES[name]["exec"]}
    if op == "PutApprovalResult":
        PIPES[name]["approved"] = body["result"]["status"]
        PIPES[name]["mode"] = "run"
        return {"approvedAt": time.time()}
    raise KeyError(op)


def cloudformation(params):
    """Query protocol: form-encoded request, XML response."""
    action = params.get("Action", [""])[0]
    stack = params.get("StackName", [""])[0]
    name = stack.removesuffix("-stack")
    if action != "DescribeStackEvents" or name not in PIPES:
        return 400, ("<ErrorResponse><Error><Type>Sender</Type><Code>ValidationError</Code>"
                     f"<Message>Stack with id {stack} does not exist</Message></Error></ErrorResponse>")
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
    "REPORT RequestId: {rid}  Duration: 211.40 ms  Billed Duration: 212 ms  Memory Size: 512 MB  Max Memory Used: 96 MB",
]


def synthetic_events(group, start_ms, pattern):
    """An event every 7 s for the last 2 hours, the same on every call (by timestamp)."""
    now_ms = int(time.time() * 1000)
    first = max(start_ms, now_ms - 2 * 3600 * 1000)
    t = first - first % 7000 + 7000
    terms = [w.strip('"?') for w in pattern.split()] if pattern else []
    out = []
    while t <= now_ms:
        k = t // 7000
        line = LAMBDA_LINES[k % len(LAMBDA_LINES)].format(rid=f"{k:08x}-7f3c", n=1000 + k % 977)
        if not terms or any(term.lower() in line.lower() for term in terms):
            out.append({"logStreamName": f"2026/10/20/[$LATEST]{k % 3:04x}", "timestamp": t, "message": line + "\n",
                        "ingestionTime": t + 300, "eventId": f"{k}-{group}"})
        t += 7000
    return out


def cwlogs(op, body):
    if op == "DescribeLogGroups":
        prefix = body.get("logGroupNamePrefix", "")
        groups = [g for g in log_groups() if g.startswith(prefix)]
        return {"logGroups": [{"logGroupName": g, "storedBytes": 1_000_000 * (i + 1), "retentionInDays": 30,
                               "creationTime": int(T0 * 1000)} for i, g in enumerate(groups)]}
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


def secret_meta(sec):
    return {"ARN": f"arn:aws:secretsmanager:us-east-1:123456789012:secret:{sec['Name']}-AbCdEf", "Name": sec["Name"],
            **({"Description": sec["Description"]} if sec["Description"] else {}),
            "LastChangedDate": sec["Changed"], "LastAccessedDate": time.time() - 86400, "CreatedDate": sec["Created"],
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
    "invoices": {"keys": [("invoiceNo", "N", "HASH")], "items": DDB_INVOICES, "gsis": []},
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
        "ItemCount": len(t["items"]), "TableSizeBytes": len(json.dumps(t["items"], default=sorted)),
        "BillingModeSummary": {"BillingMode": "PAY_PER_REQUEST"} if name != "invoices" else None,
        "ProvisionedThroughput": {"ReadCapacityUnits": 5, "WriteCapacityUnits": 5},
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
def cw_alarms(action, params):
    """DescribeAlarms…: XML response (status, body)."""
    return 400, query_error("InvalidAction", f"{action} is not in the mock yet")


# ---- CloudWatch metrics (awsQuery, Version 2010-08-01) ---------------------------------------
def cw_iso(t):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t))


def cw_parse(s):
    return calendar.timegm(time.strptime(s[:19], "%Y-%m-%dT%H:%M:%S"))


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
        times = list(range(int(start) // period * period, int(end), period))[-1440:]
        stamps = "".join(f"<member>{cw_iso(t)}</member>" for t in times)
        values = "".join(f"<member>{cw_metric_value(name, dims, t)}</member>" for t in times)
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
def sqs(op, body):
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
        return self.reply_raw(*s3_rest(method, path, query, self.headers))

    def do_GET(self):
        self.check_sig()
        if self.path.startswith("/__stats"):
            return self.reply(200, SIGNED)
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
        if is_appconfig(self.path):
            return self.route_rest("POST", raw)
        if self.headers.get("content-type", "").startswith("application/x-www-form-urlencoded"):
            # awsQuery services, told apart by their API version.
            params = parse_qs(raw.decode())
            service = {"2010-08-01": cloudwatch}.get(params.get("Version", [""])[0], cloudformation)
            return self.reply_xml(*service(params))
        body = json.loads(raw or b"{}")
        target = self.headers.get("x-amz-target", "")
        prefix, _, op = target.partition(".")
        handler = {"CodePipeline_20150709": codepipeline, "CodeBuild_20161006": codebuild,
                   "Logs_20140328": cwlogs, "AmazonSSM": ssm, "secretsmanager": secretsmanager,
                   "AWSStepFunctions": stepfunctions, "DynamoDB_20120810": dynamodb, "AWSEvents": eventbridge,
                   "AmazonSQS": sqs}.get(prefix)
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

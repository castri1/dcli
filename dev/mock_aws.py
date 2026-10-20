"""Fake AWS endpoint to try or develop dcli without an AWS account (see demo.sh).
Simulates CodePipeline, CodeBuild, CloudWatch Logs, SSM and Lambda with time-based progress."""
import json
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
    {"name": "events-ingest", "type": "EXPRESS", "created": T0 - 86400 * 30,
     "logging": f"arn:aws:logs:us-east-1:123456789012:log-group:/aws/vendedlogs/states/events-ingest-Logs:*",
     "description": "High-volume event ingestion"},
]
# Each state: (name, lambda function or None). The optimized integration ("lambda:invoke")
# for order-fulfillment, the legacy Lambda resource for invoice-generation.
SFN_STATES = {
    "order-fulfillment": [("ValidateOrder", "orders-sync"), ("ChargePayment", "payments-webhook"),
                          ("ShipOrder", "report-builder"), ("NotifyCustomer", None)],
    "invoice-generation": [("CollectUsage", "report-builder"), ("RenderPdf", "image-resizer"), ("SendInvoice", "users-auth")],
}
SFN_INPUT = {"orderId": "ORD-1042", "customerId": "C-77", "amount": 129.9, "currency": "USD",
             "items": [{"sku": "SKU-1", "qty": 2}, {"sku": "SKU-9", "qty": 1}]}
SFN_CAUSE = json.dumps({"errorType": "TimeoutError", "errorMessage": "payment provider did not answer in 3000 ms",
                        "trace": ["TimeoutError: payment provider did not answer in 3000 ms",
                                  "    at charge (/var/task/payments.js:42:11)",
                                  "    at handler (/var/task/index.js:17:5)"]})
# scenario: success, fail (fails at the 2nd state after a retry), running (in the 2nd state),
# aborted (stopped in the 3rd state). Started executions run for 20 s, then succeed.
SFN_EXECUTIONS = []
for _k, (_scenario, _ago) in enumerate([("running", 40), ("fail", 3600), ("success", 7200), ("success", 3 * 3600),
                                         ("aborted", 5 * 3600), ("fail", 86400), ("success", 2 * 86400)]):
    SFN_EXECUTIONS.append({"machine": "order-fulfillment", "name": f"ord-{1042 - _k}-{uuid.uuid4().hex[:6]}",
                           "scenario": _scenario, "start": time.time() - _ago, "redrives": 0,
                           "input": json.dumps({**SFN_INPUT, "orderId": f"ORD-{1042 - _k}"})})
for _k, _scenario in enumerate(["success", "fail", "success"]):
    SFN_EXECUTIONS.append({"machine": "invoice-generation", "name": f"invoices-2026-{9 - _k:02d}",
                           "scenario": _scenario, "start": time.time() - 86400 * (1 + 30 * _k), "redrives": 0,
                           "input": json.dumps({"month": f"2026-{9 - _k:02d}", "customers": 412})})


def sfn_exec_arn(e):
    return f"{SFN_ARN}:execution:{e['machine']}:{e['name']}"


def sfn_status(e):
    if e["scenario"] == "started":
        return "SUCCEEDED" if time.time() - e["start"] > 20 else "RUNNING"
    return {"success": "SUCCEEDED", "fail": "FAILED", "running": "RUNNING", "aborted": "ABORTED",
            "redriven": "SUCCEEDED"}[e["scenario"]]


def sfn_history(e):
    """Events of an execution, oldest first."""
    states = SFN_STATES[e["machine"]]
    legacy = e["machine"] == "invoice-generation"
    events, t = [], e["start"]

    def add(kind, prev, **details):
        nonlocal t
        events.append({"id": len(events) + 1, "previousEventId": prev, "timestamp": t, "type": kind, **details})
        t += 0.05
        return len(events)

    add("ExecutionStarted", 0, executionStartedEventDetails={"input": e["input"], "roleArn": "arn:aws:iam::123456789012:role/sfn"})
    scenario = e["scenario"]
    if scenario == "started":
        scenario = "success" if sfn_status(e) == "SUCCEEDED" else "running"
    stop_at = {"fail": 1, "running": 1, "aborted": 2}.get(scenario, len(states))
    prev = 1
    for i, (name, fn) in enumerate(states):
        kind = "Pass" if fn is None else "Task"
        entered = add(f"{kind}StateEntered", prev, stateEnteredEventDetails={"name": name, "input": e["input"]})
        if fn is None:
            prev = add(f"{kind}StateExited", entered, stateExitedEventDetails={"name": name, "output": e["input"]})
            continue
        arn = f"arn:aws:lambda:us-east-1:123456789012:function:{fn}"
        attempts = 2 if (scenario == "fail" and i == stop_at) else 1
        last = entered
        for attempt in range(attempts):
            if legacy:
                last = add("LambdaFunctionScheduled", last, lambdaFunctionScheduledEventDetails={"resource": arn, "input": e["input"]})
                last = add("LambdaFunctionStarted", last)
            else:
                params = json.dumps({"FunctionName": f"{arn}:live", "Payload": json.loads(e["input"])})
                last = add("TaskScheduled", last, taskScheduledEventDetails={"resourceType": "lambda", "resource": "invoke",
                                                                              "region": "us-east-1", "parameters": params})
                last = add("TaskStarted", last, taskStartedEventDetails={"resourceType": "lambda", "resource": "invoke"})
            t += 1.5 + i
            if scenario == "running" and i == stop_at:
                return events
            if scenario == "aborted" and i == stop_at:
                add("ExecutionAborted", last, executionAbortedEventDetails={"error": "Manual.Stop", "cause": "Stopped from the console"})
                return events
            if scenario == "fail" and i == stop_at:
                failed = {"error": "TimeoutError", "cause": SFN_CAUSE}
                if legacy:
                    last = add("LambdaFunctionFailed", last, lambdaFunctionFailedEventDetails=failed)
                else:
                    last = add("TaskFailed", last, taskFailedEventDetails={"resourceType": "lambda", "resource": "invoke", **failed})
                if attempt == attempts - 1:
                    add("ExecutionFailed", last, executionFailedEventDetails=failed)
                    return events
                t += 2
                continue
            ok = json.dumps({"ok": True, "state": name})
            if legacy:
                last = add("LambdaFunctionSucceeded", last, lambdaFunctionSucceededEventDetails={"output": ok})
            else:
                last = add("TaskSucceeded", last, taskSucceededEventDetails={"resourceType": "lambda", "resource": "invoke", "output": ok})
        prev = add(f"{kind}StateExited", last, stateExitedEventDetails={"name": name, "output": json.dumps({"ok": True})})
        if scenario == "redriven" and i == 1:
            add("ExecutionRedriven", prev, executionRedrivenEventDetails={"redriveCount": e["redrives"]})
    add("ExecutionSucceeded", prev, executionSucceededEventDetails={"output": json.dumps({"status": "DONE"})})
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
               "description": m["description"], "definition": json.dumps({"StartAt": "Start", "States": {}})}
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
        if name not in SFN_STATES:
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


# ---- AppConfig (restJson1; hosted configurations are raw documents) --------------------------
def appconfig_rest(method, path, query, headers, body):
    """Returns (status, headers, body bytes)."""
    return 404, {"content-type": "application/json"}, json.dumps({"Message": "mock: not implemented"}).encode()


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
            return self.reply_xml(*cloudformation(parse_qs(raw.decode())))
        body = json.loads(raw or b"{}")
        target = self.headers.get("x-amz-target", "")
        prefix, _, op = target.partition(".")
        handler = {"CodePipeline_20150709": codepipeline, "CodeBuild_20161006": codebuild,
                   "Logs_20140328": cwlogs, "AmazonSSM": ssm, "secretsmanager": secretsmanager,
                   "AWSStepFunctions": stepfunctions, "DynamoDB_20120810": dynamodb, "AWSEvents": eventbridge}.get(prefix)
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

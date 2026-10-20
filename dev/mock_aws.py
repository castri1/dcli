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

    def do_GET(self):
        self.check_sig()
        if self.path.startswith("/__stats"):
            return self.reply(200, SIGNED)
        try:
            self.reply(200, lambda_rest(self.path))
        except Exception as e:
            self.reply(404, {"Type": "User", "message": f"Function not found: {e}"})

    def reply_xml(self, code, text):
        data = text.encode()
        self.send_response(code)
        self.send_header("content-type", "text/xml")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        self.check_sig()
        raw = self.rfile.read(int(self.headers.get("content-length", 0)))
        if self.headers.get("content-type", "").startswith("application/x-www-form-urlencoded"):
            return self.reply_xml(*cloudformation(parse_qs(raw.decode())))
        body = json.loads(raw or b"{}")
        target = self.headers.get("x-amz-target", "")
        prefix, _, op = target.partition(".")
        handler = {"CodePipeline_20150709": codepipeline, "CodeBuild_20161006": codebuild,
                   "Logs_20140328": cwlogs, "AmazonSSM": ssm, "secretsmanager": secretsmanager}.get(prefix)
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

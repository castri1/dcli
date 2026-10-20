"""Fake AWS endpoint to try or develop dcli without an AWS account (see demo.sh).
Simulates CodePipeline, CodeBuild, CloudWatch Logs, SSM and Lambda with time-based progress."""
import json
import sys
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

T0 = time.time()
SIGNED = {"ok": 0, "unsigned": 0}


def eid():
    return str(uuid.uuid4())


# ---- pipelines ---------------------------------------------------------------------------
STAGES = ["Source", "Build", "Deploy", "Approval", "Production"]
AVG = {"Source": 15, "Build": 240, "Deploy": 90, "Approval": 30, "Production": 120}
PIPES = {}


def new_run(name, offset, mode="run", trigger="Webhook"):
    """mode: run (progresses with time), approval (stops at Approval), fail (fails at Build)."""
    PIPES[name] = {
        "exec": eid(), "start": time.time() - offset, "mode": mode, "trigger": trigger,
        "stopped": False, "approved": None,
        "history": [(eid(), "Succeeded", time.time() - 86400 * k - 3600, 470 + 30 * k) for k in range(1, 6)],
    }


new_run("app-dev", 120)
new_run("api-dev", 600, "approval")
new_run("worker-dev", 400, "fail")
new_run("app-prod", 0, "idle")
PIPES["app-prod"]["exec"] = None
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
        "sourceRevisions": [{"actionName": "Source", "revisionId": "3f9e1a2b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f",
                             "revisionSummary": json.dumps({"ProviderType": "GitHub", "CommitMessage": "fix(payments): retry on timeout\n\nlong body"})}],
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
                le["errorDetails"] = {"code": "JobFailed", "message": "Error while executing command: npm test. Reason: exit status 1"}
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
                           "configuration": {"BranchName": "main", "FullRepositoryId": "acme/" + name} if s == "Source" else {}},
                 "output": {"outputVariables": {"BranchName": "main"} if s == "Source" else {},
                            "executionResult": {"externalExecutionId": f"{name}-build:{p['exec'][:8]}"} if s == "Build" else {}}}
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
             "trigger": {"triggerType": "StartPipelineExecution"},
             "sourceRevisions": [{"revisionId": "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678", "revisionSummary": "feat: earlier change"}]}
            for (h, st, s, d) in p["history"]]
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


def codebuild(op, body):
    bid = body["ids"][0]
    name = bid.split("-build:")[0]
    st = timeline(PIPES[name])["Build"][0]
    status = {"InProgress": "IN_PROGRESS", "Failed": "FAILED"}.get(st, "SUCCEEDED")
    return {"builds": [{"id": bid, "buildStatus": status, "currentPhase": "BUILD" if status == "IN_PROGRESS" else "COMPLETED",
                        "logs": {"groupName": "/aws/codebuild/" + name, "streamName": bid.split(":")[1]}}]}


LOG = ["[Container] 2026/10/19 20:25:01 Running command npm ci",
       "added 812 packages in 14s", "\x1b[32m✓\x1b[0m lint passed",
       "[Container] 2026/10/19 20:25:20 Running command npm test",
       "PASS src/payments/retry.test.ts", "PASS src/orders/api.test.ts",
       "WARN deprecated uuid@8.3.2", "[Container] 2026/10/19 20:26:02 Running command npm run build",
       "dist/index.js  412.3kb", "Error: something failed in a test fixture (expected)"]


def cwlogs(op, body):
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


def ssm(op, body):
    if op == "DescribeParameters":
        start = int(body.get("NextToken") or 0)
        page = PARAMS[start:start + 7]
        time.sleep(0.15)
        out = {"Parameters": [{"Name": p["Name"], "Type": p["Type"], "Version": 3, "Tier": "Standard",
                               "LastModifiedDate": time.time() - 86400 * 3, "DataType": "text",
                               "LastModifiedUser": "arn:aws:sts::123456789012:assumed-role/Admin/dev-user",
                               **({"KeyId": p["KeyId"]} if "KeyId" in p else {})} for p in page]}
        if start + 7 < len(PARAMS):
            out["NextToken"] = str(start + 7)
        return out
    if op == "GetParameter":
        p = next(x for x in PARAMS if x["Name"] == body["Name"])
        value = p["Value"] if (p["Type"] != "SecureString" or body.get("WithDecryption")) else "AQICAHh...ciphertext"
        return {"Parameter": {"Name": p["Name"], "Type": p["Type"], "Value": value, "Version": 3}}
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

    def do_POST(self):
        self.check_sig()
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        target = self.headers.get("x-amz-target", "")
        prefix, _, op = target.partition(".")
        handler = {"CodePipeline_20150709": codepipeline, "CodeBuild_20161006": codebuild,
                   "Logs_20140328": cwlogs, "AmazonSSM": ssm}.get(prefix)
        try:
            self.reply(200, handler(op, body))
        except Exception as e:
            self.reply(400, {"__type": "com.amazon.coral#ValidationException", "message": f"{target}: {e!r}"})


if __name__ == "__main__":
    port = int(sys.argv[1])
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()

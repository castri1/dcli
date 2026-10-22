# dcli

Terminal UI and CLI to deploy and explore AWS: **CodePipeline** (a matrix of what is deployed
where, promote and rollback, an inbox of approvals), **CloudWatch** alarms, Logs and Logs
Insights, **Parameter Store**, **Secrets Manager**, **Lambda** (metrics, invoke), **API
Gateway**, **CloudFormation**, **S3**, **Step Functions**, **SQS**, **DynamoDB**, **Aurora**
(topology, performance, top SQL, failover), **Redshift** (nodes, queries, table health),
**DMS** (tasks, tables, CDC latency), **EventBridge** and **AppConfig** feature flags, plus a
**Savings** scan of what could cost less. Failures (pipelines, executions, functions, stacks,
routes, migration tasks) and slow databases can be copied as Markdown reports for an AI agent.
A single ~5 MB binary (about 2.5 MB to download) for macOS, Linux and Windows. No Node.js, no AWS
SDK, no runtime.

It reads the same config as the Node.js `deploy-cli` it replaces (`~/.deployment-cli/config.yaml`),
so an existing setup works as is.

```
 ◆ dcli   Pipelines   : modules   Approvals 2   Alarms ● 3                    ● dev corp-dev · us-east-1
╭ Pipelines · 3 ──────────────────────╮╭ app-dev ─────────────────────────────── ⠦ In progress · 3m 16s ╮
│ ⠦ app-dev                    3m 16s ││  ▸ 96f81235 · push · started 3m ago                              │
│   Main application - Dev            ││    3f9e1a2 "fix(payments): retry on timeout" (main)              │
│ ◆ api-dev                   11m 16s ││                                                                  │
│   Public API - Dev                  ││  ✓ Source ─ ⠦ Build ─ ○ Deploy ─ ○ Approval ─ ○ Production       │
│ ✗ worker-dev                 6m ago ││                                                                  │
│   fix(payments): retry on timeout   ││    Source     ✓ Succeeded       15s  CodeStarSourceConnection    │
│                                     ││  ▶ Build      ⠦ In progress  3m 01s  ████████████░░░░ ~75% · ≈58s│
╰─────────────────────────────────────╯╰──────────────────────────────────────────────────────────────────╯
 ↑↓ pipeline  ←→ stage  d deploy  a approve  s stop  l logs  n add  x remove  │ : go to  e env  ? help
```

## Install

macOS and Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/castri1/dcli/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/castri1/dcli/main/install.ps1 | iex
```

Run the same command again to update. `DCLI_VERSION=v2.0.0` installs a specific version and
`DCLI_INSTALL_DIR` changes where it goes (default `~/.local/bin`, or `%LOCALAPPDATA%\dcli\bin`).

With Node.js on any system: `npm install -g @castri1/dcli` (the same command updates it).

You can also download the archive for your platform from
[Releases](https://github.com/castri1/dcli/releases) and put `dcli` on your `PATH`.

From source (Rust 1.88+): `cargo install --git https://github.com/castri1/dcli`

You also need:

- AWS profiles in `~/.aws/config` (SSO, static keys or `credential_process`). For SSO:
  `aws configure sso`.
- The [AWS CLI v2](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html),
  only to sign in with SSO (`dcli login`, or `L` in the UI).

Then run `dcli add` to pick an environment and the pipelines to follow, or copy a teammate's
`~/.deployment-cli/config.yaml`.

## Use

```bash
dcli                      # interactive UI in the current environment
dcli prod                 # UI in another environment (same as `dcli ui prod`)
dcli use prod             # set the current environment (account); `use` alone opens a picker
dcli add                  # search the account's pipelines and add them (or: add dev app-dev)
dcli remove dev app-dev   # remove from the config (the pipeline in AWS is untouched)
dcli login                # AWS SSO sign-in for the current environment
dcli status app-dev       # --json for scripts
dcli history dev app-dev -n 20
dcli matrix               # what is deployed where: commit per environment, drift (--json)
dcli promote app --to prod      # deploy to prod the commit the environment before it runs (--from qa)
dcli rollback prod app-prod     # back to the previous successful deploy (--stage, --to, --rerun)
dcli approvals            # approvals waiting in every environment (--json)
dcli deploy               # picks a pipeline, runs hooks, starts and follows it (exit code = result)
dcli approve dev app-dev  # --reject, -m "comment"
dcli stop dev app-dev     # --abandon
dcli logs dev app-dev -f
dcli report app-dev       # Markdown failure report for an AI agent or an issue (--full: whole log)
dcli param /app/db/url --decrypt
dcli param /app/db/url --set "postgres://…" --type SecureString   # create or store a new version
dcli secret prod/payments/stripe                                  # print a secret (--json: all fields)
dcli secret dev/api-key --set - --yes < key.txt                   # create or update from stdin
dcli tail /aws/lambda/orders --since 1h --filter ERROR -f         # CloudWatch Logs, every stream
dcli lambda dev my-function --json
dcli lambda payments-webhook --metrics --since 7d                 # invocations, errors, p95, concurrency, throttles
dcli lambda payments-webhook --report                             # Markdown report for an AI agent
dcli lambda payments-webhook --invoke '{"orderId": "ORD-1"}'      # asks first; exit 1 on a function error
dcli insights /aws/lambda/orders -q 'fields @timestamp, @message | filter @message like /ERROR/' --since 2h
dcli insights --saved "Errors per 5 minutes" /aws/lambda/orders  # a saved query (--json, --csv)
dcli alarms                                                       # firing first; exit code 1 when any is firing
dcli alarms --all-envs --state alarm --json
dcli cfn                                                          # stacks, nested ones under their parent
dcli cfn payments --report                                        # why the last update failed, for an AI agent
dcli cfn shared-network --drift                                   # detect drift and show what changed
dcli cfn orders-api --change-set add-dlq                          # what a change set would do (--report)
dcli sqs                                                          # queues, counts, dead-letter queues
dcli sqs peek orders-dlq -n 20                                    # messages of a dead-letter queue, not consumed
dcli sqs redrive orders-dlq                                       # back to their source queues (--to QUEUE, --yes)
dcli s3 ls s3://my-bucket/exports/                                # buckets without a URI
dcli s3 url s3://my-bucket/exports/report.csv --expires 12h       # presigned download link
dcli s3 get s3://my-bucket/exports/report.csv ~/Downloads/
dcli sfn order-fulfillment --status failed                        # executions (no machine: list them)
dcli sfn order-fulfillment --report                               # Markdown report of the last failure
dcli ddb                                                          # the tables
dcli ddb orders --pk c-1001 --sk begins_with:2026-                # Query by key (--index for a GSI/LSI)
dcli ddb orders --scan --limit 20                                 # read the whole table, page by page
dcli events orders-bus                                            # rules and their targets
dcli events send --bus orders-bus --source orders.service --detail-type OrderPaid --detail '{"amount": 101}'
dcli flags checkout                                               # AppConfig feature flags × environments
dcli apigw                                                        # API Gateway: REST, HTTP and WebSocket APIs
dcli apigw public-api --stages                                    # which deployment each stage runs
dcli apigw public-api --metrics -s prod --route 'POST /orders' --since 1h
dcli apigw public-api --test 'GET /orders/42' -s prod             # runs the route for real; asks first
dcli apigw public-api --export -s prod --yaml                     # OpenAPI
dcli apigw public-api --report -s prod --route 'POST /orders'     # Markdown report for an AI agent
dcli apigw public-api --promote qa --to prod                      # also --redeploy STAGE, --rollback STAGE
dcli aurora                                                       # Aurora clusters
dcli aurora prod orders-prod                                      # topology: endpoints → writer and readers
dcli aurora prod orders-prod --metrics --range 7d                 # also --top-sql [--instance X], --events
dcli aurora prod orders-prod --report > diagnosis.md              # performance diagnosis for an AI agent
dcli aurora prod orders-prod --failover --to orders-2             # also --reboot X, --stop, --start, --snapshot
dcli aurora prod orders-prod --class orders-2=db.r7g.xlarge       # in the maintenance window (--now: right away)
dcli aurora dev sessions-dev --acu 0.5:8                          # Serverless v2 range
dcli redshift prod analytics-prod                                 # nodes: CPU, disk, skew
dcli redshift prod analytics-prod --queries --db-user admin --save-identity   # running and queued (Data API)
dcli redshift prod analytics-prod --slow                          # also --tables, --metrics, --events, --report
dcli redshift prod analytics-prod --cancel 1073
dcli redshift dev analytics-dev --pause                           # also --resume, --resize, --snapshot
dcli dms                                                          # tasks; exit code 1 when one failed or has tables in error
dcli dms orders-cdc                                               # status, latency diagnosis, settings, table mappings
dcli dms orders-cdc --tables --state error                        # also --metrics, --report
dcli dms billing-cdc --resume                                     # also --stop
dcli dms orders-cdc --reload errors                               # or schema.t1,schema.t2; alone: every table
dcli dms --scale dms-prod-1 --class dms.r5.xlarge                 # also --instances, --endpoints, --test-endpoint
dcli optimize                                                     # what could cost less (--json, --report)
dcli optimize --service lambda,logs --days 7
dcli optimize --lambda-memory                                     # memory used vs assigned (Logs Insights; asks first)
dcli update                # update dcli (npm installs update through npm); --check only tells
```

Writes (`--set`, `events send`, API Gateway stage changes and tests, Aurora, Redshift and DMS
actions) ask for confirmation, or for the environment name in
production-looking environments; `--yes` skips it (needed when the value comes from stdin).

The environment argument is optional everywhere: without it, commands use the current
environment (`use`, else the first one in the config). A single argument that is not an
environment name is taken as the pipeline (or parameter, function). Without a pipeline, the
command uses the environment's only one, or shows a searchable picker.

`add` with a new environment name (or `+ new environment` in its picker) creates the
environment: you pick an AWS profile from `~/.aws/config` and confirm the region.

### Keys

| Where | Keys |
|---|---|
| Everywhere | `:` / `Ctrl+K` go to a module, an environment or (type its name) any resource of the environment · `Tab` / `1`-`9` switch module · `e` environment · `L` SSO login · `?` help · `q` quit |
| Pipelines | `↑↓` pipeline · `←→` stage · `d` deploy · `a` approve/reject · `s` stop · `R` roll back · `l` logs · `h` history · `o` open in console · `r` refresh · `n` search the account and add pipelines · `x` remove from config · `c` copy a failure report · `C` save it with the full log |
| Matrix | `↑↓←→` move · `Enter` open the pipeline in that environment (or sign in) · `p` promote what the environment to the left runs · `R` roll back · `g` compare with the environment to the left · `r` refresh |
| Approvals | `↑↓` move · `a` approve / reject (with a comment) · `Enter` open the pipeline (or sign in) · `o` open the approval's link · `r` refresh |
| Alarms | `Enter` open the alarm's resource (Lambda, SQS, DynamoDB, Step Functions; switches environment) · `/` filter · `a` every environment / this one · `y` copy a Markdown summary · `o` console · `r` refresh · `PgUp/PgDn` scroll the detail |
| Savings | `Enter`/`g` open the resource · `a` apply the fix (log retention, SQS long polling) · `m` Lambda memory analysis (tells the GB to scan first) · `s` filter by service · `i` filter by impact · `/` search · `c` copy the plan for AI · `y` copy the finding · `r` scan again · `PgUp/PgDn` scroll |
| Pipelines (logs) | `l` build log · `z` full screen · `g` compare the commits since the last success |
| Full-screen logs | `/` search · `n`/`N` next/previous match · `e`/`E` next/previous error · `y` copy every line · `Esc` back. The mouse is left to the terminal: drag to select and copy |
| Parameters | `Enter`/`←` open folder / go up · `/` search all names · `v` reveal secret · `c` copy value · `y` copy name · `n` new parameter · `u` new value |
| Secrets | `/` search · `v` reveal · `c` copy value · `y` copy name · `n` new secret · `u` new value · `o` open in console |
| Lambda | `/` search · `m` metrics 1h/24h/7d · `M` memory used, from its logs (Logs Insights) · `i` invoke with a payload · `c` copy a report for an AI agent · `y` copy ARN · `v` show env values · `l` its logs · `o` console · `r` refresh · `PgUp/PgDn` scroll |
| API Gateway | `/` search · `←→` routes / stages / metrics / domains · `Enter` a route opens its integration (Lambda, SQS, Step Functions, EventBridge), a stage shows its settings, a domain mapping opens its API · `s` next stage · `m` 1h/24h/7d · `t` test the route · `y` copy a `curl` (`wscat` for WebSocket) or the URL · `x` export OpenAPI · `c` copy a report for AI · `l` logs · `d` redeploy · `b` roll back · `p` promote a stage · `v` show stage variables · `o` console · `r` reload |
| Logs | `Enter` open a log group · `/` search groups · `t` time range · `f` filter pattern · `p` pause/resume live · `z` full screen · `y` copy · `o` console · `Space` mark groups · `i` Logs Insights on them |
| Logs Insights | `Enter` run (`Alt+Enter` new line) · `i` edit · `r` run again · `t` range · `g` log groups · `s` saved and recent queries · `Enter` on a row: the whole record · `←→` columns · `y`/`Y` copy the table as Markdown/CSV · `c` copy for AI · `Esc` stop / back |
| CloudFormation | `↑↓` stack · `←→` overview / events / resources / outputs / drift / change sets · `Enter` its rows (a resource opens in its module, a nested stack is selected, a change set shows its changes) · `c` copy for AI (why it failed; on Drift the drift; on Change sets the change set) · `d` detect drift · `T` template · `y` copy · `/` search · `o` console · `r` reload |
| S3 | `Enter`/`←` open / go up · `/` filter · `p` preview (first 64 KB) · `u` download link (15 min, 1 h, 12 h) · `d` download to `~/Downloads` · `y` copy the `s3://` URI · `o` console · `r` reload |
| Step Functions | `Enter` executions · `f` filter by status · `→` steps · `g` graph · `t` timeline · `i`/`O` input/output · `l` logs · `c` copy a failure report · `R` redrive from the failed step · `n` start with the same input · `x` stop · `o` console |
| SQS | `→`/`Enter` its DLQ or source queue · `m` messages (dead-letter queues only) · `R` redrive · `x` cancel the redrive · `/` search · `y` copy URL · `o` console · `r` reload |
| Step Functions (graph, timeline) | `↑↓` state · `←→` pan · `g`/`t` switch view · `f` the failed state · `i`/`O` its input/output · `l` its logs · `c` copy a failure report · click a state to select it |
| DynamoDB | `Enter` open a table (scans the first 50 items) · `f` find by key (table or index) · `s` scan · `m` next page · `/` filter the items read · `y` copy the item · `z` full screen · `o` console |
| Aurora | `Enter`/`Esc` open a cluster / back · `←→` topology / performance / top SQL / events / configuration · `↑↓` endpoint or node (a statement in Top SQL) · `Enter` a node's metrics; in Top SQL select a statement, again to read it whole · `f` fail over (to the selected reader) · `b` reboot the node · `M` change the node's class (Serverless v2: the cluster's ACU range) · `x` stop / start the cluster · `S` manual snapshot · `m` 1h/24h/7d · `y` copy the endpoint (or statement) · `Y` copy a `psql`/`mysql` command · `c` copy a performance diagnosis for AI · `/` search · `o` console · `r` reload |
| Redshift | `Enter`/`Esc` open a cluster / back · `←→` nodes / performance / queries / tables / events / configuration · `↑↓` node, or a query after `Enter` in Queries · `Enter` a node's metrics; in Queries select, then the whole SQL · `x` cancel the selected query · `t` sort tables · `i` database identity · `p` pause / resume · `R` resize · `s` manual snapshot · `m` 1h/24h/7d · `y` copy endpoint (or query) · `Y` copy a `psql` command · `c` copy a performance diagnosis for AI · `/` search · `o` console · `r` reload |
| DMS | `←→` tasks / instances / endpoints (in a task: overview / tables / settings) · `Enter` open a task (on an instance: its tasks; on an endpoint: test it) · `s` resume or start · `x` stop · `R` reload (marked tables, those in error, or every table) · `S` change the instance class · `t` test an endpoint's connection · `Space` / `E` mark a table / those in error · `f` filter tables · `/` search · `c` copy a report for AI · `l` its logs · `m` 1h/24h/7d · `y` copy ARN · `o` console · `r` refresh |
| EventBridge | `Enter` a bus's rules · `→` targets, `Enter` on one opens it (Lambda, Step Functions, Logs) · `t` send a test event · `y` copy the pattern · `o` console |
| AppConfig | `Enter` application → profile → flags · `↑↓←→` flag × environment · `Enter` edit and deploy · `x` stop the deployment (rolls back) · `y` copy the document |

Lists with a search box (`:`, `e`, `n`) filter as you type; `Space` marks several items
where that makes sense and `Ctrl+A` marks every match.

The header shows every module while they fit; with more modules (or a narrow terminal) it
shows only the current one, and `:` reaches the rest by name. Modules with something to say
show it there: approvals waiting, alarms firing, DMS tasks failed.

Typing in `:` also searches the environment's resources by name: pipelines, functions, log
groups, parameters, secrets, stacks, buckets, state machines, queues, tables, APIs, Aurora and
Redshift clusters, DMS tasks, event buses, AppConfig applications and alarms (listed in the background the first time, then kept for
five minutes). Enter opens the resource in its module.

The mouse works too: wheel to scroll, click to select and switch modules. Forms accept pasted
text (multi-line JSON included); `Ctrl+R` shows a hidden value while typing it.

### When a pipeline fails

`c` copies a Markdown report of the pipeline's most recent failure, ready to paste into
Claude Code or any other AI agent (or an issue): the failed stage and action, the error, the
commit, what changed since the last successful run (with a link comparing the commits), the
failed CodeBuild phase and the useful part of the build log (error lines with the lines around
them, and the end of the log), or the failed resources of a CloudFormation deploy. `C` saves the report with the whole log to a file
and copies its path, for logs too long to paste. `dcli report [env] [pipeline]` prints the same
report, so an agent can also run it by itself: "run `dcli report dev app-dev` and find out why
it failed".

Build logs can contain secrets that the build printed; read the report before sharing it
outside your team.

The detail panel and `dcli status` also show what changed since the last successful run;
`g` opens the comparison of those commits (GitHub, GitLab, Bitbucket).

Production-looking environments (`prod`, `prd`, `live`) ask you to type the environment name
before deploying and before creating or changing parameters and secrets.

### Other services

- **S3**: links are presigned URLs anyone can open until they expire; they are signed with
  your session, so they stop working when the session ends (dcli tells you when that comes
  first). The CLI allows up to 7 days (`--expires 7d`) with long-lived credentials.
- **Step Functions**: a failed execution shows the step that failed, its error and cause.
  `g` draws the execution like the console's graph: the state machine's flow (Choice branches
  side by side, Parallel and Map states as frames with their flows inside, loops and catch
  handlers), each state colored by what happened to it and the Choice branch taken
  highlighted; beside it, the selected state's error, where it goes next and its input and
  output. `t` shows a timeline instead: a bar for each state over the execution's time, so
  slow steps, retries, polling loops and Map items running side by side stand out. On the
  list of state machines, `g` draws a machine's definition.
  `c` copies the same kind of report as a failed pipeline. Redrive (`R`) restarts from the
  failed step without running the ones that succeeded; `n` starts a new execution with the
  selected one's input, editable first. Express workflows keep no execution list: their
  history is in their logs (`l`).
- **DynamoDB**: find by key runs a Query (a GetItem with the whole key), which reads only the
  matching items. A scan reads the whole table and is billed for everything it reads, so it
  goes 50 items at a time. Items show as plain JSON, without DynamoDB's type wrappers.
- **EventBridge**: `t` prefills an event that the selected rule's pattern accepts. The first
  Enter checks it against the pattern (nothing is sent); the next one sends it to the bus,
  where every matching rule receives it, so its targets run.
- **Lambda**: charts of the last hour, day or week: invocations with their errors, p95
  duration against the timeout, concurrency and throttles. `i` invokes the function for real
  (type the environment name in production) and shows its response and the end of its log;
  it is never sent twice. `c` copies a report for an AI agent: the configuration (environment
  variable names, never their values), the last 24 hours of metrics, and the errors,
  timeouts and crashes in its logs grouped by kind with their stack traces.
- **Logs Insights**: `i` in Logs runs a query over the selected group, or several marked with
  `Space`, showing results as they arrive with the records matched and data scanned
  (Insights is billed by the data it scans: narrow the range or the groups on big ones).
  `stats … by …` results get bars. Saved queries from the account and this session's queries
  are under `s`; `c` copies the query and results as Markdown for an AI agent.
- **CloudFormation**: a failed or rolled-back stack shows its root cause, the first resource
  that failed in the latest operation (not the resources cancelled because of it), followed
  into nested stacks. `c` copies a report with that cause, the timeline, the failing
  resource's piece of the template and the parameters (NoEcho masked). `d` runs a drift
  detection (a read). The Change sets tab shows what an update would do, replacements
  highlighted, before anyone executes it.
- **SQS**: dead-letter queues holding messages come first. Messages are read only in
  dead-letter queues, and not consumed: in a normal queue, reading counts as a receive and
  could push messages to their DLQ. Redrive sends a DLQ's messages back to the queues they
  came from (or another queue), shows its progress and can be cancelled.
- **API Gateway**: REST, HTTP and WebSocket APIs. Routes show what they call and how they
  are protected (routes anyone can call are marked ⚠), with requests, 5xx and p99 per route
  when the stage has detailed metrics. The Stages tab shows which deployment each stage runs
  and how far it trails the stage before it; `d` redeploys a stage, `b` rolls it back to an
  earlier deployment, `p` promotes one stage's deployment to the next (stages with auto-deploy
  are left alone). `t` tests a route: REST APIs through API Gateway's test invoke (it runs the
  integration for real and skips the authorizer), HTTP APIs with a real request to the stage
  URL; neither is ever sent twice. Stage variables stay masked until `v` and never go into
  reports. `c` copies a report of a failing route: its settings, metrics and the error lines of
  its access and execution logs.
- **Aurora**: the Topology tab draws the cluster: its writer, reader and custom endpoints with
  arrows to the nodes they serve, the nodes by Availability Zone (writer, readers with their
  failover tier) with their CPU, connections and replica lag, and the cluster volume. Charts
  per node and range are under Performance; Top SQL shows the database load, wait events and
  heaviest statements from Performance Insights (when it is on; dcli never turns it on).
  Events puts failovers, reboots and maintenance on a timeline. Actions: fail over (to the
  selected reader), reboot a node, stop and start the cluster (AWS starts a stopped cluster
  again after 7 days), change a node's class or a Serverless v2 range (now or in the
  maintenance window), take a manual snapshot; each form says what happens first. `Y` copies
  a connection command; with IAM authentication its password comes from `aws rds
  generate-db-auth-token` when you run it, so dcli never handles a token or password.
- **Redshift**: the Nodes tab draws the leader and compute nodes with their CPU and disk; a
  node far above the others is marked as skewed (usually the distribution key). Queries and
  Tables read the system tables with SQL through the Redshift Data API, as a database identity
  you choose once per cluster: a database user with temporary credentials, or a Secrets
  Manager secret (saved in `~/.deployment-cli/redshift.json`, without passwords). Queries shows
  what runs and waits now, the slowest queries and longest queue waits of the day and who
  blocks whom (users who are not superusers see only their own queries); `x` cancels one.
  Tables flags those that need VACUUM or ANALYZE, are skewed or uncompressed. `p` pauses or
  resumes the cluster, `R` resizes it among the shapes elastic resize allows, with their
  estimated disk use, and `s` takes a snapshot.
- **DMS**: tasks in trouble come first. A task's overview says where its CDC latency comes
  from (reading the source or applying to the target), whether changes wait on disk, and
  whether its replication instance is short of memory or CPU, with the charts behind it and
  the distinct error lines of its log. Tables shows each table's state, rows, changes and
  validation. Resume warns when a task has been stopped long (the source may no longer keep
  the changes it needs); reloading every table always asks for the task's name and says what
  happens to the target tables, and tables in error can be reloaded alone. `S` changes the
  instance class (it restarts; the form says which tasks stop meanwhile), `t` tests an
  endpoint's connection. The header counts failed tasks and a notification tells when one
  fails.
- **AppConfig**: each cell is what that environment serves now. Editing starts from that
  version, so other environments' changes do not slip in; you see the change, pick a
  deployment strategy, and dcli creates the new version, deploys it and follows it. Freeform
  profiles (JSON, YAML, text) are shown read-only.

Production-looking environments ask for their name before these writes too (redrive, start,
stop, test events, flag deploys, invoking a function, SQS redrives, promote and rollback, API
Gateway stage changes and route tests, Aurora, Redshift and DMS actions, savings fixes; for
AppConfig, also when its environment is named like production, and for API Gateway when the
stage is).

Reports for AI agents quote logs, failure reasons and templates as data: in code blocks they
cannot close, marked as material to analyse rather than instructions. Logs can still contain
secrets your code printed; read a report before sharing it outside your team.

### What is deployed where

The Matrix module (and `dcli matrix`) shows one row per application and one column per
environment, in the order of your config, which is read as the promotion order (dev → qa →
prod, or uat → prod…). Each cell is the commit of that pipeline's last successful run, with
what is deploying now and how many deploys it trails the environment to its left. Pipelines
are grouped into applications when they share a name across environments, or when their names
only differ by the environment (`orders-uat`, `orders-prod`); otherwise set the same `app:` on
them:

```yaml
environments:
  uat:
    pipelines:
      - name: billing-v2-uat
        app: billing
  prod:
    pipelines:
      - name: legacy-billing-live
        app: billing
```

Every environment is queried, so each needs a valid AWS session: an expired one shows
"sign in" in its cells, and Enter there signs in.

### Promote, roll back, approve

In the Matrix, `p` on a cell that is behind deploys there exactly the commit the environment
to its left runs, through the usual deploy (hooks, confirmation, typed name in production).
`R` (also in Pipelines) goes back to the previous successful deploy with another commit: on
V2 pipelines with CodePipeline's stage rollback (fast, nothing rebuilt, you pick the stage),
on V1 pipelines by running the whole pipeline again at that commit. Pinning a commit works for
CodeCommit and CodeStar connection sources (GitHub, GitLab, Bitbucket), S3 object versions and
ECR digests.

The Approvals module lists every manual approval waiting in every environment, with what is
being approved and the approval's link; the header counts them and dcli notifies new ones.

### Alarms

The Alarms module lists CloudWatch alarms (metric and composite) of every environment, firing
ones first, with what makes each fire, its recent state changes and a graph of its metric
against the threshold, the firing periods marked. The header counts the alarms firing in the
current environment (refreshed every minute, also while you use other modules), and a desktop
notification tells when one starts firing (`defaults.notifications`). `dcli alarms` exits
with 1 when an alarm is firing, for scripts and CI checks.

### Savings

The Savings module (and `dcli optimize`) looks at the environment's resources and their usage
over the last 14 days and lists what could cost less: Lambda functions that could run on arm64,
idle provisioned concurrency, timeouts far above what functions take; log groups kept forever
and the biggest ingesters; SQS queues without long polling; DynamoDB capacity far above use,
unused tables, steady on-demand tables, tables that are mostly storage; S3 buckets without
lifecycle rules, abandoned multipart uploads, old versions kept forever; busy Standard state
machines of short executions; secrets nobody reads; alarms without data; REST APIs that could
be HTTP APIs and stage caches that rarely hit; idle or oversized Aurora instances, Serverless
minimums above use, heavy I/O on Standard storage, old manual snapshots; Redshift clusters idle
for hours (to pause on a schedule), nodes to spare, old snapshots, heavy concurrency scaling;
DMS replication instances with nothing running, oversized ones, Multi-AZ outside production.

There are no prices in dcli: each finding says how much usage the change removes, in the
unit AWS bills for it (GB-seconds, GB stored, requests, capacity-unit hours, instance hours),
with the measurements behind it and what the estimate assumes; its impact (●●● high, ●●○
medium, ●○○ low) comes from that volume. Log retention and SQS long polling can be applied
from the module (`a`). `m` (or `M` on a function in Lambda) compares the memory functions use
with what they have, read from their logs with Logs Insights; Insights is billed by the data
it scans, so dcli tells you how much first. `c` copies the findings as a plan for an AI agent.

### Updates

`dcli update` downloads the latest release and replaces the binary (checksum verified); when
dcli was installed with npm it runs `npm install -g @castri1/dcli` instead. The UI checks for a
new release once a day and tells you; `DCLI_NO_UPDATE_CHECK=1` turns that off.

## Config

```yaml
environments:
  dev:
    profile: corp-dev        # AWS profile (SSO, static keys or credential_process)
    region: us-east-1        # optional: defaults to the profile's region
    pipelines:
      - name: app-dev
        description: Main app
        hooks:
          pre-deploy:
            - command: npm test
              description: Unit tests
          post-deploy:
            - curl -sf https://dev.example.com/health
defaults:
  pollIntervalSeconds: 10
  notifications: true        # desktop notifications on finish / approval needed
```

Search order: `--config FILE`, `~/.deployment-cli/config.yaml`, `./deployment-cli.yaml`,
`./.deployment-cli.yaml`. Without any config file, every AWS profile shows up as an environment
(Parameters and Lambda work; press `n` in Pipelines to add the account's pipelines, which
creates the config file).

Adding or removing pipelines rewrites the config file and keeps the previous version as
`config.yaml.bak`. Unknown keys are kept; comments are not (the Node version behaves the same).
The current environment lives in `~/.deployment-cli/current-env`.

`AWS_ENDPOINT_URL` sends every call to another endpoint (LocalStack, a proxy, a test server).

## Architecture

```
src/
  aws/            minimal AWS client: SigV4 signing, profiles, SSO, one shared HTTP agent
    mod.rs        Aws::call (awsJson1_1/1_0), Aws::rest (restJson1), Aws::query (XML), retries,
                  Aws::raw (any request), Aws::presign (presigned URLs), Aws::download
    codepipeline.rs codebuild.rs cloudformation.rs logs.rs ssm.rs secrets.rs lambda.rs
    s3.rs sfn.rs sqs.rs dynamodb.rs events.rs appconfig.rs apigateway.rs
    rds.rs pi.rs  Aurora (awsQuery) and Performance Insights
    redshift.rs redshift_data.rs dms.rs
    xml.rs        reading awsQuery XML responses
    cloudwatch.rs alarms.rs metrics.rs   CloudWatch (awsQuery): alarms and GetMetricData
  tui/            app shell: header, module palette, environment, toasts, help, SSO login, event loop
    module.rs     the Module trait every module implements
    picker.rs     searchable list, in the UI and inline in the shell
    logview.rs    log viewer with full screen, search and error jumps (Pipelines, Logs)
    form.rs       modal forms (new parameter, new secret…)
    chart.rs      terminal charts (metrics of Lambda, Alarms, API Gateway, Aurora)
    canvas.rs     box-drawing canvas for diagrams (Step Functions graph, Aurora topology)
  modules/        one file (or folder) per module: pipelines, matrix, approvals, alarms,
                  savings, parameters, secrets, lambda, apigateway, cwlogs, cfn, s3, sfn,
                  sqs, dynamodb, aurora, redshift, dms, events, appconfig
  clicmd/         the commands of the newer modules (dcli s3, sfn, ddb, events, flags, lambda,
                  alarms, approvals, promote, rollback, sqs, insights, cfn, apigw, aurora,
                  redshift, dms, optimize)
  savings/        one check per service for the Savings module and `dcli optimize`
  catalog.rs      every resource of an environment by name, for the `:` search
  asl.rs          Amazon States Language: a state machine's definition arranged for its graph
  pipeline.rs     CodePipeline domain logic shared by the UI and the CLI
  matrix.rs       applications × environments: grouping, deployed commit, drift
  cli.rs          non-interactive commands
  report.rs       failure reports
  update.rs       self-update and the daily new-version check
```

Background work runs on plain threads and reports back through a channel, so the UI never
blocks. There is no async runtime and no AWS SDK: each service is a handful of typed JSON calls.

### Adding a module (e.g. ECS)

1. **AWS calls**: create `src/aws/<service>.rs` with a `Service` constant and functions that
   call `aws.call(&SERVICE, "Operation", &json!({...}))` (or `aws.rest(...)` for REST APIs),
   deserializing into `#[derive(Deserialize)]` structs. Register it in `src/aws/mod.rs`.
2. **UI**: create `src/modules/<name>.rs` implementing `tui::module::Module`:
   `title` and `description` (shown in the `:` palette), `reset` (environment changed),
   `activate` (load lazily), `key`, `task` (results from `ctx.spawn`), `render`, `hints`.
   `parameters.rs` is a compact example.
3. **Register** it in `src/modules/mod.rs`. It appears in the header and the palette. To make
   its resources searchable from `:`, add a lister to `src/catalog.rs` and implement `open`.
4. **CLI** (optional): a `src/clicmd/<name>.rs` with a clap `Args` struct and `run`, added to
   the `Command` enum in `src/cli.rs`.

## Development

```bash
cargo test                 # unit tests, including the official AWS SigV4 test vectors
cargo clippy --all-targets
cargo build --release      # target/release/dcli
dev/demo.sh                # the UI against a local fake AWS (no account needed)
```

CI runs clippy and the tests on Linux, macOS and Windows.

To release: set `version` in `Cargo.toml`, commit, then tag and push (`git tag v2.1.0 &&
git push origin v2.1.0`). The release workflow builds archives for macOS (arm64, x86_64),
Linux (x86_64, arm64, static musl) and Windows (x86_64) and attaches them, with checksums,
to a GitHub release; the install scripts pick up the newest one. It then publishes them to npm
(`npm/build.mjs`): `@castri1/dcli` is a small launcher, and npm installs only the
`@castri1/dcli-<os>-<cpu>` package with the binary for the machine.

## The Node.js version

`dcli` 2.x replaces the Node.js `deploy-cli` 1.x (npm `@castri1/deployment-cli`, now deprecated
in favour of `@castri1/dcli`). That code is
frozen at the git tag [`node-v1.0.1`](https://github.com/castri1/dcli/tree/node-v1.0.1); new
work happens here, on `main`. Both read the same config, so they can be installed side by side
while you switch over.

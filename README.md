# dcli

Terminal UI and CLI to deploy and explore AWS: **CodePipeline**, **Parameter Store** and **Lambda**.
A single ~3 MB binary for macOS, Linux and Windows. No Node.js, no AWS SDK, no runtime.

It reads the same config as the Node.js `deploy-cli` it replaces (`~/.deployment-cli/config.yaml`),
so an existing setup works as is.

```
 ◆ dcli   1 Pipelines   2 Parameters   3 Lambda                               ● dev corp-dev · us-east-1
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
dcli deploy               # picks a pipeline, runs hooks, starts and follows it (exit code = result)
dcli approve dev app-dev  # --reject, -m "comment"
dcli stop dev app-dev     # --abandon
dcli logs dev app-dev -f
dcli param /app/db/url --decrypt
dcli lambda dev my-function --json
```

The environment argument is optional everywhere: without it, commands use the current
environment (`use`, else the first one in the config). A single argument that is not an
environment name is taken as the pipeline (or parameter, function). Without a pipeline, the
command uses the environment's only one, or shows a searchable picker.

`add` with a new environment name (or `+ new environment` in its picker) creates the
environment: you pick an AWS profile from `~/.aws/config` and confirm the region.

### Keys

| Where | Keys |
|---|---|
| Everywhere | `:` / `Ctrl+K` go to a module or environment · `Tab` / `1`-`9` switch module · `e` environment · `L` SSO login · `?` help · `q` quit |
| Pipelines | `↑↓` pipeline · `←→` stage · `d` deploy · `a` approve/reject · `s` stop · `l` logs · `h` history · `o` open in console · `r` refresh · `n` search the account and add pipelines · `x` remove from config |
| Parameters | `Enter`/`←` open folder / go up · `/` search all names · `v` reveal secret · `c` copy value · `y` copy name |
| Lambda | `/` search · `v` show env values · `c` copy ARN · `o` open in console · `PgUp/PgDn` scroll |

Lists with a search box (`:`, `e`, `n`) filter as you type; `Space` marks several items
where that makes sense and `Ctrl+A` marks every match.

The header shows every module while they fit; with more modules (or a narrow terminal) it
shows only the current one, and `:` reaches the rest by name.

The mouse works too: wheel to scroll, click to select and switch modules.

Production-looking environments (`prod`, `prd`, `live`) ask you to type the environment name
before deploying.

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
    mod.rs        Aws::call (awsJson1_1) and Aws::rest (restJson1), retries, endpoint
    codepipeline.rs codebuild.rs logs.rs ssm.rs lambda.rs   typed calls per service
  tui/            app shell: header, module palette, environment, toasts, help, SSO login, event loop
    module.rs     the Module trait every module implements
    picker.rs     searchable list, in the UI and inline in the shell
  modules/        one file per module: pipelines.rs, parameters.rs, lambda.rs
  pipeline.rs     CodePipeline domain logic shared by the UI and the CLI
  cli.rs          non-interactive commands
```

Background work runs on plain threads and reports back through a channel, so the UI never
blocks. There is no async runtime and no AWS SDK: each service is a handful of typed JSON calls.

### Adding a module (e.g. ECS, Secrets Manager, CloudWatch)

1. **AWS calls**: create `src/aws/<service>.rs` with a `Service` constant and functions that
   call `aws.call(&SERVICE, "Operation", &json!({...}))` (or `aws.rest(...)` for REST APIs),
   deserializing into `#[derive(Deserialize)]` structs. Register it in `src/aws/mod.rs`.
2. **UI**: create `src/modules/<name>.rs` implementing `tui::module::Module`:
   `title` and `description` (shown in the `:` palette), `reset` (environment changed),
   `activate` (load lazily), `key`, `task` (results from `ctx.spawn`), `render`, `hints`.
   `parameters.rs` is a compact example.
3. **Register** it in `src/modules/mod.rs`. It appears in the header and the palette.

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
to a GitHub release; the install scripts pick up the newest one.

## The Node.js version

`dcli` 2.x replaces the Node.js `deploy-cli` 1.x (npm `@castri1/deployment-cli`). That code is
frozen at the git tag [`node-v1.0.1`](https://github.com/castri1/dcli/tree/node-v1.0.1); new
work happens here, on `main`. Both read the same config, so they can be installed side by side
while you switch over.

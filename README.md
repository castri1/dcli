<p align="center">
  <img src="https://img.shields.io/npm/v/@castri1/deployment-cli?style=flat-square&color=0ea5e9" alt="npm version" />
  <img src="https://img.shields.io/badge/node-%3E%3D18-brightgreen?style=flat-square" alt="node version" />
  <img src="https://img.shields.io/npm/l/@castri1/deployment-cli?style=flat-square" alt="license" />
</p>

<h1 align="center">deploy-cli</h1>

<p align="center">
  A powerful CLI tool to launch, monitor, and manage AWS CodePipeline deployments across multiple environments — from a single terminal.
</p>

<br/>

```
  ____  _____ ____  _     _____   __
 |  _ \| ____|  _ \| |   / _ \ \ / /
 | | | |  _| | |_) | |  | | | \ V /
 | |_| | |___|  __/| |__| |_| || |
 |____/|_____|_|   |_____\___/ |_|

 v1.0.0
```

---

## Why

Deploying through the AWS Console is slow and tedious. Jumping between tabs to check stages, approve gates, or tail logs breaks your flow. **deploy-cli** brings all of that into your terminal with real-time tracking, desktop notifications, and interactive hotkeys — so you never leave your editor.

---

## Install

```bash
npm install -g @castri1/deployment-cli
```

## Quick Start

```bash
# Interactive first-time setup
deploy-cli init

# Deploy a pipeline
deploy-cli deploy dev my-app

# Watch all tracked pipelines in real-time
deploy-cli watch
```

---

## Commands

| Command | Description |
|---------|-------------|
| `init` | Set up deploy-cli with your first environment and pipeline |
| `login [env]` | Run AWS SSO login for an environment |
| `deploy [env] [pipeline]` | Start a pipeline execution and monitor progress |
| `watch [env] [pipeline]` | Live status view of all tracked executions |
| `status <env> [pipeline]` | Detailed status of a specific execution |
| `list` | List all configured environments and pipelines |
| `add` | Add a new pipeline or environment interactively |
| `remove` | Remove a pipeline or environment |
| `sync [env]` | Import pipelines from AWS into your config |
| `approve <env> [pipeline]` | Approve or reject a pending manual approval |
| `history <env> [pipeline]` | Show recent deployment history |
| `logs <env> [pipeline]` | Stream CodeBuild logs in real-time |
| `stop [env] [pipeline]` | Stop a running pipeline execution |
| `config` | View or edit the active configuration |

Running `deploy-cli` with no command opens an **interactive menu**.

---

## Live View

The `watch` command gives you a real-time dashboard of all running pipelines with interactive hotkeys:

| Key | Action |
|-----|--------|
| `d` | Deploy a new pipeline |
| `a` | Approve a pending gate |
| `s` | Stop a running pipeline |
| `l` | Stream CodeBuild logs |
| `h` | Show execution history |
| `q` | Quit |

Pipelines show per-stage progress with status indicators:

```
  ✓ Source        ✓ Build        ⟳ Deploy (2m 34s)        ○ Approval        ○ Production
```

Desktop notifications fire on completions, failures, and pending approvals.

---

## Configuration

deploy-cli uses a YAML config file. Run `deploy-cli init` to create one, or set it up manually.

**Config file locations** (checked in order):

1. `~/.deployment-cli/config.yaml`
2. `./deployment-cli.yaml`
3. `./.deployment-cli.yaml`

### Example

```yaml
environments:
  dev:
    profile: my-company-dev       # AWS SSO profile
    region: us-east-1
    pipelines:
      - name: my-app-dev
        description: "Main app — Dev"
        hooks:
          pre-deploy:
            - command: npm test
              description: Run tests before deploying
          post-deploy:
            - command: npm run smoke:test
              description: Smoke test the deployment

  staging:
    profile: my-company-staging
    region: us-east-1
    pipelines:
      - name: my-app-staging
        description: "Main app — Staging"

  prod:
    profile: my-company-prod
    region: us-east-1
    pipelines:
      - name: my-app-prod
        description: "Main app — Production"

defaults:
  pollIntervalSeconds: 10
  notifications: true
```

---

## Hooks

Run commands before or after deployments. Pre-deploy hooks abort the deployment on failure. Post-deploy hooks run only after a successful pipeline execution.

```yaml
hooks:
  pre-deploy:
    - command: npm test
      description: Run unit tests
    - command: npm run lint
  post-deploy:
    - command: curl -sf https://my-app.dev/health
      description: Health check
```

Skip hooks with `--skip-hooks`:

```bash
deploy-cli deploy dev my-app --skip-hooks
```

---

## Pipeline Sync

Already have pipelines in AWS? Import them automatically:

```bash
# Interactive selection
deploy-cli sync dev

# Import all missing pipelines
deploy-cli sync dev --all
```

---

## JSON Output

Machine-readable output is available on display commands:

```bash
deploy-cli status dev my-app --json
deploy-cli list --json
deploy-cli history dev my-app --json
```

---

## Authentication

deploy-cli uses AWS SSO profiles defined in `~/.aws/config`. Each environment maps to a profile:

```bash
# Log in to an environment
deploy-cli login dev

# Credentials are validated automatically before operations
```

If credentials expire during a long watch session, deploy-cli detects it and prompts you to re-login.

---

## Requirements

- **Node.js** >= 18
- **AWS CLI v2** with SSO profiles configured
- Active AWS SSO session for target environments

---

## License

MIT

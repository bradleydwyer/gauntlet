# Gauntlet

> **Note:** This is an experimental project and is unlikely to be actively supported. Use at your own risk. It depends on [Tasked](https://github.com/bradleydwyer/tasked) which is not yet public — compilation is not currently possible for external users.

CI pipeline runner powered by Tasked. Runs on your own hardware, replaces GitHub Actions.

## How it works

1. Push to any repo with a `.gauntlet/pipeline.json`
2. Gauntlet receives the webhook, clones the repo, compiles the pipeline
3. Steps execute in parallel via the Tasked DAG engine
4. Results reported back to GitHub as check runs

## Quick start

```bash
cargo install --path .
gauntlet serve
```

Config at `~/.gauntlet/config.json`:
```json
{
  "github_app_id": 12345,
  "github_private_key": "~/.gauntlet/private.pem",
  "webhook_secret": "your-secret"
}
```

Requires a [GitHub App](#github-app-setup) and a public URL ([Cloudflare Tunnel](#cloudflare-tunnel)) for webhooks.

---

## Pipeline Format

Pipeline file: `.gauntlet/pipeline.json`

### Minimal

```json
{
  "steps": [{ "command": "cargo test" }]
}
```

### Top-level fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `steps` | Step[] | **required** | Pipeline steps |
| `defs` | map | `{}` | Reusable step definitions (see [Definitions](#definitions)) |
| `runner` | string or object | none | Default runner for all steps (see [Runner](#runner)) |
| `env` | map | `{}` | Environment variables for all steps |
| `checkout` | bool or object | `true` | Git checkout config |
| `secrets` | map | `{}` | Secret references (`{"KEY": {"env": "VAR"}}`) |
| `retry` | number | 3 | Default auto-retry count |
| `timeout` | number | 300 | Default timeout in seconds |

### Steps

A step must have exactly one **step type** field:

| Field | Type | Description |
|-------|------|-------------|
| `command` | string | Run a shell command |
| `commands` | string[] | Multiple commands (joined with `&&`) |
| `container` | object | Docker container (`{"image": "node:20"}`) + optional `command` |
| `block` | string | Approval gate |
| `trigger` | object | Sub-pipeline (`{"pipeline": "deploy"}`) |
| `executor` | string | Raw Tasked executor (escape hatch) |

Common fields on any step:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `key` | string | auto | Unique ID for `depends_on` |
| `use` | string | none | Inherit from a definition |
| `depends_on` | string or string[] | `[]` | Steps that must complete first |
| `if` | string | none | Condition expression |
| `env` | map | `{}` | Step environment variables |
| `runner` | string or object | pipeline default | Runner override |
| `timeout` | number | pipeline default | Timeout in seconds |
| `retry` | number | pipeline default | Auto-retry count |
| `soft_fail` | bool | `false` | Failure doesn't fail the pipeline |
| `matrix` | string[] or object | none | Matrix expansion |
| `artifacts` | string[] or object | none | Artifact glob patterns |
| `cache` | object | none | Cache config |
| `config` | any | none | Executor config (with `executor`) |

### Definitions

Reusable step templates. Steps inherit fields via `use`:

```json
{
  "defs": {
    "rust": {
      "runner": { "image": "rust:latest", "setup": "rustup component add clippy rustfmt" },
      "timeout": 600
    }
  },
  "steps": [
    { "key": "check",  "use": "rust", "command": "cargo check" },
    { "key": "clippy", "use": "rust", "command": "cargo clippy -- -D warnings" },
    { "key": "test",   "use": "rust", "command": "cargo test" },
    { "key": "fmt",    "use": "rust", "command": "cargo fmt --check" }
  ]
}
```

Merge rules: step overrides def. `env` merges (def base + step overlay). `if` conditions AND together.

### Runner

```json
"runner": "rust:latest"
```

```json
"runner": {
  "image": "rust:latest",
  "setup": "rustup component add clippy rustfmt"
}
```

```json
"runner": "host"
```

| Type | Description |
|------|-------------|
| `"image:tag"` | Docker container |
| `"host"` | Run directly on host |
| `{"type": "docker", "image": "...", "setup": "..."}` | Docker with setup commands |
| `{"type": "tart", "vm": "sonoma-base"}` | Tart VM (macOS) |

Docker containers get: workspace at `/workspace`, cargo/npm/pip cache mounts, git credentials for private deps, `setup` commands before each step.

### Parallelism

Steps run in parallel by default. `depends_on` creates ordering:

```json
{
  "steps": [
    { "key": "a", "command": "echo a" },
    { "key": "b", "command": "echo b" },
    { "key": "c", "command": "echo c", "depends_on": ["a", "b"] }
  ]
}
```

`a` and `b` run in parallel. `c` waits for both.

### Conditions

```
"if": "branch == 'main'"
"if": "event == 'pull_request'"
```

### Matrix

```json
{ "key": "test", "command": "cargo test --features ${matrix}", "matrix": ["serde", "tokio"] }
```

Multi-dimension:
```json
{
  "matrix": {
    "dimensions": { "toolchain": ["stable", "nightly"], "os": ["linux", "macos"] },
    "exclude": [{ "toolchain": "nightly", "os": "macos" }]
  }
}
```

### Executor escape hatch

Direct access to any Tasked executor:

```json
{
  "key": "notify",
  "executor": "slack",
  "config": { "operation": "post_message", "channel": "#builds", "text": "Done" },
  "depends_on": ["build"]
}
```

---

## Examples

### Rust project

```json
{
  "defs": {
    "rust": {
      "runner": { "image": "rust:latest", "setup": "rustup component add clippy rustfmt" }
    }
  },
  "steps": [
    { "key": "check",  "use": "rust", "command": "cargo check" },
    { "key": "clippy", "use": "rust", "command": "cargo clippy -- -D warnings" },
    { "key": "test",   "use": "rust", "command": "cargo test" },
    { "key": "fmt",    "use": "rust", "command": "cargo fmt --check" }
  ]
}
```

### Build and deploy

```json
{
  "runner": "rust:latest",
  "steps": [
    { "key": "test",   "command": "cargo test" },
    { "key": "build",  "command": "cargo build --release", "depends_on": ["test"], "artifacts": ["target/release/myapp"] },
    { "key": "deploy", "command": "./deploy.sh", "depends_on": ["build"], "if": "branch == 'main'", "runner": "host" }
  ]
}
```

### Node with matrix

```json
{
  "steps": [
    { "key": "lint", "command": "npm run lint", "runner": "node:20" },
    {
      "key": "test",
      "command": "npm ci && npm test",
      "runner": "node:${matrix}",
      "matrix": ["18", "20", "22"],
      "depends_on": ["lint"]
    }
  ]
}
```

---

## Setup

### GitHub App

1. **Settings > Developer Settings > GitHub Apps > New GitHub App**
2. Permissions: Checks (rw), Commit statuses (rw), Contents (read), Pull requests (read)
3. Subscribe to events: Push, Pull request
4. Webhook URL: `https://your-domain/webhook/github`
5. Set a webhook secret
6. Install on **All repositories**
7. Download the private key to `~/.gauntlet/private.pem`

### Config

`~/.gauntlet/config.json`:
```json
{
  "github_app_id": 12345,
  "github_private_key": "~/.gauntlet/private.pem",
  "webhook_secret": "your-secret"
}
```

All fields can also be set via CLI flags or environment variables.

### Cloudflare Tunnel

```bash
brew install cloudflared
cloudflared tunnel login
cloudflared tunnel create gauntlet
cloudflared tunnel route dns gauntlet ci.yourdomain.com
cloudflared tunnel run gauntlet
```

`~/.cloudflared/config.yml`:
```yaml
tunnel: <tunnel-id>
credentials-file: ~/.cloudflared/<tunnel-id>.json
ingress:
  - hostname: ci.yourdomain.com
    service: http://localhost:7711
  - service: http_status:404
```

### Run

```bash
gauntlet serve
```

---

## CLI

### `gauntlet serve`

Run the CI daemon with TUI dashboard.

```
gauntlet serve [OPTIONS]

Options:
  --data-dir <DIR>                 Data directory [env: GAUNTLET_DATA_DIR] [default: ~/.gauntlet]
  --github-app-id <ID>             GitHub App ID [env: GITHUB_APP_ID]
  --github-private-key <PATH>      PEM file path [env: GITHUB_PRIVATE_KEY]
  --port <PORT>                    Listen port [default: 7711]
  --webhook-secret <SECRET>        Enable webhook mode [env: GITHUB_WEBHOOK_SECRET]
  --poll-interval <SECS>           Poll interval (poll mode only) [default: 30]
  --concurrency <N>                Max concurrent tasks [default: 8]
```

All options can be set in `~/.gauntlet/config.json`.

### `gauntlet run [FILE]`

Execute a pipeline locally.

```
gauntlet run [OPTIONS] [FILE]

Arguments:
  [FILE]    Pipeline file [default: .gauntlet/pipeline.json]

Options:
  --git-ref <REF>         Git ref to checkout
  --no-checkout           Skip checkout step
  --no-cache              Disable caching
  --concurrency <N>       Max parallel tasks [default: 16]
  --filter <IDS>          Run specific tasks only (comma-separated)
  --matrix <KEY=VALUE>    Pin a matrix dimension
  --env <KEY=VALUE>       Override environment variables
  --secret <KEY=VALUE>    Provide secrets
  --dry-run               Print compiled FlowDef without executing
  --auto-approve          Auto-approve approval gates
  --github-status         Report commit status to GitHub
  -v, --verbose           Show synthetic tasks
  -q, --quiet             Show final result only
```

### `gauntlet logs [FLOW_ID]`

View build logs from the running daemon.

```
gauntlet logs                      List recent builds
gauntlet logs <FLOW_ID>            Show all step logs for a build
gauntlet logs <FLOW_ID> --step X   Show a specific step's log

Options:
  --step <NAME>           Specific step to view
  --server <URL>          Daemon URL [default: http://localhost:7711]
```

### `gauntlet validate [FILE]`

Validate a pipeline definition without executing.

### `gauntlet schema`

Print the pipeline JSON schema.

## Architecture

```
GitHub --> Cloudflare Tunnel --> gauntlet serve (port 7711)
                                    |
                                    |-- Webhook handler
                                    |-- Workspace manager (clone/checkout)
                                    |-- Compiler (JSON --> Tasked FlowDef)
                                    |-- Tasked engine (parallel DAG execution)
                                    +-- GitHub Checks API (report results)
```



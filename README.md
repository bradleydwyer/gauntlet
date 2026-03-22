# Gauntlet

CI pipeline runner powered by [Tasked](https://github.com/bradleydwyer/tasked). Runs on your own hardware, replaces GitHub Actions.

## How it works

1. Push to any repo with a `.gauntlet/pipeline.json`
2. Gauntlet receives the webhook, clones the repo, compiles the pipeline
3. Steps execute in parallel via the Tasked DAG engine
4. Results reported back to GitHub as check runs

## Quick start

```bash
# Build
cargo build --release

# Run the daemon
gauntlet serve \
  --github-app-id YOUR_APP_ID \
  --github-private-key ~/.gauntlet/private.pem \
  --webhook-secret YOUR_WEBHOOK_SECRET
```

Requires a [GitHub App](#github-app-setup) and a public URL (e.g., [Cloudflare Tunnel](#cloudflare-tunnel)) for webhooks.

## Pipeline format

`.gauntlet/pipeline.json`:

```json
{
  "steps": [
    { "key": "check",  "command": "cargo check" },
    { "key": "clippy", "command": "cargo clippy -- -D warnings" },
    { "key": "test",   "command": "cargo test" },
    { "key": "fmt",    "command": "cargo fmt --check" }
  ]
}
```

Steps run in parallel by default. Use `depends_on` to create a DAG:

```json
{
  "steps": [
    { "key": "test",   "command": "cargo test" },
    { "key": "build",  "command": "cargo build --release", "depends_on": ["test"] },
    { "key": "deploy", "command": "./deploy.sh", "depends_on": ["build"], "if": "branch == 'main'" }
  ]
}
```

### Runner

Steps run on the host by default. Specify a Docker image to run in a container:

```json
{
  "runner": "rust:latest",
  "steps": [
    { "key": "test", "command": "cargo test" }
  ]
}
```

Per-step override:

```json
{
  "steps": [
    { "key": "test-rust", "command": "cargo test", "runner": "rust:latest" },
    { "key": "test-node", "command": "npm test", "runner": "node:20" },
    { "key": "local",     "command": "echo hi",   "runner": "host" }
  ]
}
```

### Step types

| Field | Type | Description |
|-------|------|-------------|
| `command` | string | Shell command |
| `commands` | string[] | Multiple commands (joined with `&&`) |
| `container` | object | Run in a Docker container (`{"image": "node:20"}`) |
| `block` | string | Approval gate (pauses pipeline) |
| `trigger` | object | Start a sub-pipeline |
| `executor` | string | Raw Tasked executor (escape hatch for http, slack, etc.) |

### Common fields

| Field | Type | Description |
|-------|------|-------------|
| `key` | string | Unique step identifier |
| `depends_on` | string or string[] | Steps that must complete first |
| `if` | string | Condition expression (e.g., `branch == 'main'`) |
| `env` | object | Environment variables |
| `timeout` | number | Timeout in seconds |
| `retry` | number | Auto-retry count |
| `soft_fail` | bool | Failure doesn't fail the pipeline |
| `matrix` | string[] or object | Matrix expansion |
| `artifacts` | string[] | Glob patterns to upload |

### Matrix builds

```json
{
  "key": "test",
  "command": "cargo test --features ${matrix}",
  "matrix": ["serde", "tokio", "full"]
}
```

Multi-dimension:

```json
{
  "key": "test",
  "command": "cargo +${matrix.toolchain} test",
  "matrix": {
    "dimensions": {
      "toolchain": ["stable", "nightly"],
      "target": ["x86_64", "aarch64"]
    },
    "exclude": [{ "toolchain": "nightly", "target": "aarch64" }]
  }
}
```

## GitHub App setup

1. Go to **Settings > Developer Settings > GitHub Apps > New GitHub App**
2. Set permissions: Checks (rw), Commit statuses (rw), Contents (read), Pull requests (read)
3. Subscribe to events: Push, Pull request
4. Set Webhook URL to your public URL + `/webhook/github`
5. Set a Webhook secret
6. Install on **All repositories**
7. Download the private key

## Cloudflare Tunnel

For webhook delivery to a machine behind NAT:

```bash
brew install cloudflared
cloudflared tunnel login
cloudflared tunnel create gauntlet
cloudflared tunnel route dns gauntlet your-subdomain.example.com
```

`~/.cloudflared/config.yml`:
```yaml
tunnel: <tunnel-id>
credentials-file: ~/.cloudflared/<tunnel-id>.json

ingress:
  - hostname: your-subdomain.example.com
    service: http://localhost:7711
  - service: http_status:404
```

```bash
cloudflared tunnel run gauntlet
```

## Architecture

```
GitHub webhook --> Cloudflare Tunnel --> gauntlet serve (port 7711)
                                            |
                                            |-- Webhook handler (verify + parse)
                                            |-- Workspace manager (clone/checkout)
                                            |-- Compiler (pipeline.json --> Tasked FlowDef)
                                            |-- Tasked engine (DAG execution)
                                            +-- GitHub Checks API (report results)
```

## CLI

```
gauntlet run [FILE]       # Execute a pipeline locally
gauntlet serve            # Run the CI daemon
gauntlet validate [FILE]  # Validate a pipeline
gauntlet schema           # Print pipeline JSON schema
```

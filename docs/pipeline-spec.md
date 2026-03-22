# Gauntlet Pipeline Specification

Version: 2

## Overview

A gauntlet pipeline is a JSON file (`.gauntlet/pipeline.json`) that defines a set of steps to execute. Steps run in parallel by default — use `depends_on` to create ordering. Gauntlet compiles the pipeline into a Tasked DAG for durable, parallel execution.

## Top-Level Fields

```json
{
  "steps": [],
  "defs": {},
  "runner": "",
  "env": {},
  "checkout": true,
  "on": [],
  "secrets": {},
  "retry": 3,
  "timeout": 300
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `steps` | Step[] | **required** | Pipeline steps |
| `defs` | map<string, StepDef> | `{}` | Reusable step definitions (referenced via `use`) |
| `runner` | string or RunnerConfig | none | Default runner for all steps |
| `env` | map<string, string> | `{}` | Environment variables for all steps |
| `checkout` | bool or CheckoutConfig | `true` | Git checkout config |
| `on` | Trigger[] | `[]` | Trigger event declarations (informational) |
| `secrets` | map<string, SecretSource> | `{}` | Secret references |
| `retry` | number | queue default (3) | Default auto-retry count |
| `timeout` | number | queue default (300) | Default timeout in seconds |

### Aliases (v1 compatibility)

| v1 field | v2 equivalent |
|----------|---------------|
| `tasks` | `steps` |
| `retries` | `retry` |
| `timeout_secs` | `timeout` |

## Steps

Each step is an object in the `steps` array. A step must have exactly one **step type** field that determines what it does.

### Step Types

| Field | Type | Description |
|-------|------|-------------|
| `command` | string | Run a shell command |
| `commands` | string[] | Run multiple commands (joined with `&&`) |
| `container` | ContainerConfig | Run in a Docker container (combine with `command`) |
| `block` | string | Approval gate — pauses pipeline with a message |
| `trigger` | TriggerConfig | Start a sub-pipeline |
| `executor` | string | Raw Tasked executor name (escape hatch) |

`container` + `command`/`commands` is allowed — the command runs inside the container.

### Common Fields

Available on every step:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `key` | string | auto (`step-N`) | Unique identifier for `depends_on` references |
| `label` | string | auto | Display name |
| `use` | string | none | Name of a def to inherit from |
| `depends_on` | string or string[] | `[]` | Steps that must complete first |
| `if` | string | none | Condition — step skipped if false |
| `env` | map<string, string> | `{}` | Step environment variables (merged on global + def) |
| `runner` | string or RunnerConfig | pipeline default | Runner override for this step |
| `timeout` | number | pipeline default | Timeout in seconds |
| `retry` | number | pipeline default | Auto-retry count |
| `soft_fail` | bool | `false` | Failure doesn't fail the pipeline |
| `matrix` | string[] or MatrixConfig | none | Matrix expansion |
| `artifacts` | string[] or ArtifactConfig | none | Artifact glob patterns |
| `cache` | CacheConfig | none | Cache configuration |
| `spawn` | bool | `false` | Enable dynamic step generation |
| `spawn_output` | string[] | `[]` | Signal IDs for spawn deferred deps |
| `config` | any | none | Executor config (used with `executor`) |

### Aliases (v1 compatibility)

| v1 field | v2 equivalent |
|----------|---------------|
| `id` | `key` |
| `retries` | `retry` |
| `timeout_secs` | `timeout` |

## Definitions (`defs`)

Named step templates. Steps reference them with `use` to inherit fields.

```json
{
  "defs": {
    "rust": {
      "runner": { "image": "rust:latest", "setup": "rustup component add clippy rustfmt" },
      "timeout": 600
    }
  },
  "steps": [
    { "key": "test", "use": "rust", "command": "cargo test" }
  ]
}
```

### StepDef Fields

A def can set any step field except `key` and `depends_on`.

| Field | Type | Description |
|-------|------|-------------|
| `runner` | string or RunnerConfig | Runner config |
| `env` | map<string, string> | Base environment |
| `timeout` | number | Default timeout |
| `retry` | number | Default retry count |
| `soft_fail` | bool | Default soft_fail |
| `if` | string | Base condition (AND'd with step's condition) |
| `command` | string | Default command |
| `commands` | string[] | Default commands |

### Merge Rules

When a step uses a def:
- **Most fields**: step value overrides def value
- **`env`**: def is base, step merges on top (step wins on conflict)
- **`if`**: both conditions must be true (AND). Def narrows, step narrows further.

## Runner

Determines where a step executes.

### Short form (string)

```json
"runner": "rust:latest"       // Docker container
"runner": "host"              // Run directly on the host
```

### Full form (object)

```json
"runner": {
  "type": "docker",
  "image": "rust:latest",
  "setup": "rustup component add clippy rustfmt"
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | `"docker"` | Runner type: `docker`, `tart`, `host` |
| `image` | string | none | Docker image (for docker type) |
| `vm` | string | none | Tart VM name (for tart type) |
| `setup` | string | none | Commands to run before every step |

### Docker Runner Behavior

When a step runs in Docker:
- Workspace mounted at `/workspace` (working directory)
- Cargo cache mounted at `/usr/local/cargo/registry` and `/usr/local/cargo/git`
- npm cache mounted at `/root/.npm`
- pip cache mounted at `/root/.cache/pip`
- `setup` commands run before the step command
- GitHub App token injected for private git dependency access

### Host Runner Behavior

When runner is `"host"` or not set:
- Command runs in a shell on the host machine
- Working directory is the workspace (cloned repo)
- Host toolchain used directly

## Parallelism

Steps run in parallel by default. Use `depends_on` to create ordering:

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

`depends_on` can be a single string: `"depends_on": "a"`.

## Conditions

The `if` field uses a simple expression language:

```
branch == 'main'
branch =~ /release-.*/
event == 'push'
event == 'pull_request'
```

Available variables: `branch`, `event`, `tag`.

When a condition is false, the step is skipped (marked as succeeded with no output).

## Matrix

Expand a step into multiple parallel variants.

### Simple (single dimension)

```json
{
  "key": "test",
  "command": "cargo test --features ${matrix}",
  "matrix": ["serde", "tokio", "full"]
}
```

Expands to `test-serde`, `test-tokio`, `test-full`. `${matrix}` interpolates the value.

### Multi-dimension

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

Cartesian product minus exclusions. Values via `${matrix.field}`.

Steps that `depends_on` a matrix step automatically fan-in (wait for all variants).

Matrix values are also available as `MATRIX_<KEY>` environment variables.

## Artifacts

```json
{ "key": "build", "command": "cargo build --release", "artifacts": ["target/release/myapp"] }
```

Glob patterns. Uploaded after step succeeds. Available to downstream steps via Tasked's artifact system.

Full form:
```json
{ "artifacts": { "upload": ["dist/*"], "download_from": ["build"] } }
```

## Cache

```json
{
  "key": "build",
  "command": "cargo build",
  "cache": {
    "key": "cargo-${matrix.toolchain}",
    "paths": ["target/"],
    "restore_keys": ["cargo-"]
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `key` | string | Cache key (supports `${matrix.*}` substitution) |
| `paths` | string[] | Paths to cache |
| `restore_keys` | string[] | Fallback keys if exact key misses |

## Checkout

```json
"checkout": true                          // shallow clone (default)
"checkout": false                         // skip checkout
"checkout": { "depth": 10, "lfs": true }  // custom
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `depth` | number | 1 | Clone depth (0 = full) |
| `submodules` | bool | `false` | Init submodules |
| `lfs` | bool | `false` | Pull LFS objects |

In serve mode, checkout is handled by the workspace manager. The pipeline's checkout setting is ignored.

## Triggers

```json
"on": [
  { "push": { "branches": ["main"] } },
  { "pull_request": { "branches": ["main"] } },
  { "schedule": { "cron": "0 0 * * *" } },
  "manual"
]
```

Currently informational — the webhook receiver and poller determine when builds run.

## Secrets

```json
"secrets": {
  "API_KEY": { "env": "MY_API_KEY" },
  "TOKEN": { "file": "~/.secrets/token" }
}
```

Available in steps via `${secrets.API_KEY}` (Tasked interpolation).

## Container Config

```json
"container": {
  "image": "node:20",
  "env": { "NODE_ENV": "test" },
  "working_dir": "/app"
}
```

## Trigger Config (sub-pipeline)

```json
"trigger": {
  "pipeline": "deploy",
  "env": { "TARGET": "staging" }
}
```

## Executor Escape Hatch

Direct access to any registered Tasked executor:

```json
{
  "key": "notify",
  "executor": "slack",
  "config": {
    "operation": "post_message",
    "channel": "#builds",
    "text": "Build complete"
  }
}
```

Full Tasked variable interpolation available in `config`: `${tasks.*}`, `${secrets.*}`.

## Examples

### Minimal

```json
{
  "steps": [{ "command": "cargo test" }]
}
```

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

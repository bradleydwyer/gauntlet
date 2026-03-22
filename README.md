# Gauntlet

CI pipeline runner powered by [Tasked](../README.md). Single binary, JSON pipelines, DAG-native execution.

## Quick Start

```bash
# Build
cargo build -p gauntlet

# Create a pipeline
mkdir -p .gauntlet
cat > .gauntlet/pipeline.json << 'EOF'
{
  "checkout": false,
  "tasks": [
    {"id": "lint", "command": "cargo clippy -- -D warnings"},
    {"id": "test", "command": "cargo test", "depends_on": ["lint"]},
    {"id": "build", "command": "cargo build --release", "depends_on": ["test"]}
  ]
}
EOF

# Run it
gauntlet run
```

## Why JSON?

Gauntlet uses JSON instead of YAML because:

- LLMs generate valid JSON reliably (structured output, function calling)
- No indentation sensitivity — no silent breakage from whitespace
- A published JSON Schema means any agent can generate valid pipelines
- MCP agents submit it directly — it's already the wire format

If you're writing pipelines by hand and prefer not to write JSON directly, use any language to generate it:

```bash
python3 generate_pipeline.py | gauntlet run /dev/stdin
```

## Pipeline Format

Pipelines are JSON files (default: `.gauntlet/pipeline.json`) that compile to Tasked FlowDefs.

### Minimal Example

```json
{
  "tasks": [
    {"id": "test", "command": "cargo test"}
  ]
}
```

### Full Example

```json
{
  "on": [{"push": {"branches": ["main"]}}, "pull_request"],
  "checkout": true,
  "env": {"RUST_BACKTRACE": "1", "CARGO_TERM_COLOR": "always"},
  "secrets": {"DEPLOY_TOKEN": {"env": "DEPLOY_TOKEN"}},
  "timeout_secs": 600,
  "retries": 1,
  "tasks": [
    {
      "id": "lint",
      "command": "cargo clippy -- -D warnings"
    },
    {
      "id": "test",
      "command": "cargo test",
      "depends_on": ["lint"],
      "matrix": {
        "dimensions": {"toolchain": ["stable", "nightly"]}
      },
      "cache": {
        "key": "cargo-${matrix.toolchain}",
        "paths": ["target/", "~/.cargo/registry/"]
      },
      "timeout_secs": 900
    },
    {
      "id": "build",
      "command": "cargo build --release",
      "depends_on": ["test"],
      "artifacts": {"upload": ["target/release/myapp"]}
    },
    {
      "id": "deploy",
      "command": "./deploy.sh",
      "depends_on": ["build"],
      "if": "branch == 'main'"
    }
  ]
}
```

### Pipeline Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `on` | `Trigger[]` | `[]` | Trigger events (informational in local mode) |
| `checkout` | `bool` | `true` | Inject a git checkout step as the DAG root |
| `checkout_config` | `object` | `{depth: 1}` | Checkout options: `depth`, `submodules`, `lfs` |
| `env` | `map` | `{}` | Global env vars merged into every task |
| `secrets` | `map` | `{}` | Secret references: `{"name": {"env": "VAR"}}` or `{"name": {"file": "/path"}}` |
| `retries` | `int` | none | Default retry count for all tasks |
| `timeout_secs` | `int` | none | Default timeout for all tasks |
| `tasks` | `Task[]` | required | Pipeline tasks |

### Task Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | `string` | required | Unique task identifier |
| `command` | `string` | - | Shell command (sugar for shell executor) |
| `executor` | `string` | - | Explicit executor: `shell`, `container`, `http`, `delay`, `approval`, `noop` |
| `config` | `object` | - | Executor config (when using `executor` directly) |
| `container` | `object` | - | Container shorthand: `{image, command?, env?, working_dir?}` |
| `env` | `map` | `{}` | Task-level env vars (merged on top of global) |
| `depends_on` | `string[]` | `[]` | Task IDs this task depends on |
| `if` | `string` | - | Conditional execution expression |
| `matrix` | `object` | - | Matrix expansion: `{dimensions: {key: [values]}, exclude?: []}` |
| `retries` | `int` | - | Override retry count |
| `timeout_secs` | `int` | - | Override timeout |
| `cache` | `object` | - | Cache config: `{key, paths, restore_keys?}` |
| `artifacts` | `object` | - | Artifacts: `{upload?: [paths], download_from?: [task_ids]}` |
| `spawn` | `bool` | `false` | Enable dynamic pipeline generation (stdout parsed as task definitions) |
| `spawn_output` | `string[]` | `[]` | Signal IDs exported by spawn tasks |

Use exactly one of `command`, `container`, or `executor` per task.

### Matrix Builds

Matrix expands a task into multiple parallel variants:

```json
{
  "id": "test",
  "command": "cargo +$MATRIX_TOOLCHAIN test",
  "matrix": {
    "dimensions": {
      "toolchain": ["stable", "nightly"],
      "features": ["default", "all"]
    },
    "exclude": [
      {"toolchain": "nightly", "features": "all"}
    ]
  }
}
```

This produces 3 tasks: `test-stable-all`, `test-stable-default`, `test-nightly-default`. Matrix values are injected as `MATRIX_<KEY>` environment variables. Downstream tasks that `depends_on: ["test"]` automatically fan-in on all variants.

### Dynamic Pipelines (Spawn)

A task with `spawn: true` has its stdout parsed as a JSON array of new task definitions, injected into the running DAG:

```json
{
  "tasks": [
    {
      "id": "discover",
      "command": "./find-changed-services.sh",
      "spawn": true,
      "spawn_output": ["complete"]
    },
    {
      "id": "deploy-all",
      "command": "echo 'all services deployed'",
      "depends_on": ["discover/complete"]
    }
  ]
}
```

The `discover` task outputs JSON task definitions. Generated tasks are namespaced under `discover/`. The `deploy-all` task waits for all generated tasks to complete.

### Caching

Local filesystem cache at `~/.gauntlet/cache/`:

```json
{
  "id": "build",
  "command": "npm install && npm run build",
  "cache": {
    "key": "node-modules-${file.hash:package-lock.json}",
    "paths": ["node_modules/"],
    "restore_keys": ["node-modules-"]
  }
}
```

Cache restore runs before the task, cache save runs after.

### Artifacts

Local filesystem artifacts at `~/.gauntlet/artifacts/`:

```json
{
  "id": "build",
  "command": "cargo build --release",
  "artifacts": {"upload": ["target/release/myapp"]}
},
{
  "id": "deploy",
  "command": "./deploy.sh",
  "depends_on": ["build"],
  "artifacts": {"download_from": ["build"]}
}
```

## CLI

### `gauntlet run [FILE]`

Execute a pipeline locally. Default file: `.gauntlet/pipeline.json`.

```
FLAGS:
  --ref <REF>           Git ref to checkout
  --no-checkout         Skip checkout step
  --no-cache            Disable caching
  --concurrency <N>     Max parallel tasks (default: CPU count)
  --filter <IDS>        Run specific tasks + dependencies only
  --matrix KEY=VAL      Pin a matrix dimension
  --env KEY=VAL         Override environment variables
  --secret KEY=VAL      Provide secrets
  --dry-run             Print compiled FlowDef JSON without executing
  --auto-approve        Auto-approve approval tasks
  --github-status       Report commit status to GitHub
  --github-token <T>    GitHub API token (or GITHUB_TOKEN env)
  --github-repo <R>     GitHub repo as owner/repo (or GITHUB_REPOSITORY env)
  --github-sha <S>      Commit SHA (or GITHUB_SHA env)
  -v, --verbose         Show synthetic tasks and full output
  -q, --quiet           Only show final result
```

Exit codes: `0` success, `1` failure, `2` validation error.

### `gauntlet validate [FILE]`

Validate a pipeline without executing.

```bash
gauntlet validate                              # text output
gauntlet validate --format json                # machine-readable
gauntlet validate .gauntlet/pipeline.json      # explicit path
```

### `gauntlet schema`

Print the pipeline JSON schema (for IDE validation and LLM system prompts).

## Architecture

Gauntlet is a CI-specific layer on top of Tasked's generic DAG execution engine:

```
  Pipeline JSON
       │
       ▼
  ┌─────────┐     Gauntlet compiles CI-specific JSON
  │ Compiler │     into Tasked FlowDefs through 9 passes:
  │ (9 pass) │     validate → matrix → checkout → cache →
  └────┬─────┘     artifacts → shorthand → env → condition → assemble
       │
       ▼
  ┌──────────┐
  │ FlowDef  │     Standard Tasked flow definition
  └────┬─────┘
       │
       ▼
  ┌──────────┐     Tasked handles:
  │  Engine   │     DAG scheduling, parallel execution,
  │ (Tasked)  │     retries, timeouts, variable interpolation,
  └──────────┘     spawn/dynamic task injection
```

Gauntlet depends on `tasked` as a library crate. Zero changes to Tasked — all CI opinions live in the Gauntlet crate.

### What Gauntlet Adds vs What Tasked Provides

**Tasked provides:** DAG execution, shell/container/HTTP executors, retries with backoff, timeouts, variable interpolation, spawn (dynamic task injection), approval gates, cron schedules, webhooks, SQLite storage.

**Gauntlet adds:** Pipeline JSON format, CI-specific compiler, matrix expansion, git checkout injection, cache/artifact step injection, environment merging, GitHub commit status, CLI with TUI progress display.

## GitHub Integration

Report pipeline status to GitHub commits:

```bash
gauntlet run --github-status \
  --github-token "$GITHUB_TOKEN" \
  --github-repo "owner/repo" \
  --github-sha "$(git rev-parse HEAD)"
```

Or via environment variables (auto-detected in GitHub Actions):

```bash
export GITHUB_TOKEN="..."
export GITHUB_REPOSITORY="owner/repo"
export GITHUB_SHA="$(git rev-parse HEAD)"
gauntlet run --github-status
```

## Using Inside GitHub Actions

Use Gauntlet's DAG engine inside a GitHub Actions workflow:

```yaml
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo install --path gauntlet
      - run: gauntlet run --no-checkout --github-status
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

This gives you DAG-optimized parallel execution, matrix builds, and caching — all within a single GHA job.

# test


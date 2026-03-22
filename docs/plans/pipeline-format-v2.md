# Pipeline Format v2

## Context

Gauntlet is a personal CI system — one process on a Mac Studio replacing GitHub Actions for all repos. Not a product, just a tool. The pipeline format needs to be:

- Simple enough to write by hand for most repos
- JSON (structured, LLM-friendly, no YAML indentation bugs)
- DAG-native — use tasked properly, don't fight it
- Compiled down to tasked FlowDefs for durable execution

## Design Principles

1. **DAG-first, not phase-based.** Steps declare what they depend on. Everything else runs in parallel. No `wait` barriers — they're a Buildkite legacy that produces worse schedules than explicit DAGs.

2. **Shorthands for the common case, raw access for everything else.** `command` is sugar for the shell executor. `container` is sugar for the container executor. But you can always drop down to `executor` + `config` for any tasked executor (http, delay, approval, spawn, trigger, remote, integrations).

3. **Tasked is the engine, not a hidden detail.** Features that tasked already has (retries with backoff, per-queue concurrency, rate limiting, sub-flow composition, dynamic task injection, variable interpolation) are exposed directly, not reimplemented.

4. **Simple until you need complex.** A one-step pipeline is one line. Matrix builds, conditions, artifacts are opt-in per step.

## Format

Pipeline file: `.gauntlet/pipeline.json` (or passed via CLI).

### Minimal

```json
{
  "steps": [
    { "command": "cargo test" }
  ]
}
```

One step, shell, checkout automatic. Done.

### Typical Rust project

```json
{
  "env": { "RUST_BACKTRACE": "1" },
  "steps": [
    { "key": "check",  "command": "cargo check" },
    { "key": "clippy", "command": "cargo clippy -- -D warnings" },
    { "key": "test",   "command": "cargo test" },
    { "key": "fmt",    "command": "cargo fmt --check" },
    {
      "key": "build",
      "command": "cargo build --release",
      "depends_on": ["check", "clippy", "test", "fmt"],
      "artifacts": ["target/release/myapp"]
    }
  ]
}
```

check, clippy, test, fmt all run in parallel (no `depends_on` between them). build runs after all four succeed. This is a proper DAG — tasked schedules it optimally.

### Full-featured

```json
{
  "env": { "RUST_BACKTRACE": "1" },
  "checkout": { "depth": 1, "submodules": true },
  "steps": [
    { "key": "lint", "command": "cargo clippy -- -D warnings" },
    {
      "key": "test",
      "command": "cargo test --features ${matrix}",
      "matrix": ["default", "serde", "full"],
      "depends_on": ["lint"],
      "retry": 2,
      "timeout": 600
    },
    {
      "key": "build",
      "command": "cargo build --release",
      "depends_on": ["test"],
      "artifacts": ["target/release/myapp"]
    },
    {
      "key": "docker",
      "commands": ["docker build -t myapp .", "docker push ghcr.io/myorg/myapp"],
      "depends_on": ["build"],
      "if": "branch == 'main'"
    },
    {
      "key": "approve",
      "block": "Deploy to production?",
      "depends_on": ["docker"],
      "if": "branch == 'main'"
    },
    {
      "key": "deploy",
      "command": "./deploy.sh",
      "depends_on": ["approve"],
      "timeout": 300
    }
  ]
}
```

### Top-level fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `steps` | array | required | Pipeline steps |
| `env` | object | `{}` | Environment variables for all steps |
| `checkout` | bool or object | `true` | Git checkout config. `true` = shallow clone. `false` = skip. Object for depth/submodules/lfs. |

No `secrets` field — secrets are environment variables on the host. No `agents` field yet — single machine for now.

### Steps

Every step is an object in the `steps` array. Common fields available on all step types:

| Field | Type | Description |
|-------|------|-------------|
| `key` | string | Unique identifier for `depends_on`. Auto-generated from index if omitted. |
| `label` | string | Display name (defaults to command or step type) |
| `depends_on` | string or string[] | Step keys that must succeed before this runs |
| `if` | string | Condition expression — step skipped (marked succeeded) if false |
| `env` | object | Step environment variables (merged on global) |
| `timeout` | number | Timeout in seconds |
| `retry` | number | Auto-retry count (uses exponential backoff via tasked) |
| `soft_fail` | bool | Failure doesn't fail the pipeline |

Step type is determined by which field is present:

#### `command` / `commands` — Shell step

```json
{ "key": "test", "command": "cargo test" }
{ "key": "build", "commands": ["make clean", "make build", "make package"] }
```

`commands` joins with `&&` into a single shell invocation.

#### `container` — Docker step

```json
{
  "key": "test-node",
  "container": { "image": "node:20" },
  "command": "npm test"
}
```

Runs the command inside the container. `container` sets the image; `command`/`commands` sets what runs in it. Maps to tasked's container executor.

#### `block` — Approval gate

```json
{ "key": "approve", "block": "Deploy?", "depends_on": ["build"] }
```

Maps to tasked's approval executor. Approved via CLI or API.

#### `trigger` — Sub-pipeline

```json
{
  "key": "deploy-staging",
  "trigger": {
    "pipeline": "deploy",
    "env": { "TARGET": "staging" }
  },
  "depends_on": ["build"]
}
```

Maps to tasked's trigger executor. Submits a child flow.

#### `executor` — Raw tasked executor

```json
{
  "key": "notify",
  "executor": "slack",
  "config": {
    "operation": "post_message",
    "credential": "${secrets.SLACK_TOKEN}",
    "channel": "#builds",
    "text": "Build ${tasks.build.output.exit_code == 0 ? 'passed' : 'failed'}"
  },
  "depends_on": ["build"]
}
```

Direct access to any registered tasked executor — shell, http, container, delay, approval, spawn, trigger, remote, or any integration executor. This is the escape hatch. Full tasked variable interpolation (`${tasks.*}`, `${secrets.*}`) is available in `config`.

#### `spawn` — Dynamic step generation

```json
{
  "key": "discover",
  "command": "./find-services.sh",
  "spawn": true,
  "spawn_output": ["done"]
}
```

Maps to tasked's spawn executor. The command's stdout is parsed as task definitions and injected into the running flow. Downstream steps can depend on `"discover/done"`.

### Matrix builds

**Single dimension:**
```json
{
  "key": "test",
  "command": "cargo test --features ${matrix}",
  "matrix": ["serde", "tokio", "full"]
}
```

Expands to 3 parallel steps (`test-serde`, `test-tokio`, `test-full`). `${matrix}` interpolates the value.

**Multi-dimension:**
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

Cartesian product with exclusions. Values via `${matrix.field}`.

Steps that `depends_on` a matrix step automatically fan-in — they wait for all matrix variants to complete.

### Artifacts

```json
{
  "key": "build",
  "command": "cargo build --release",
  "artifacts": ["target/release/myapp", "target/release/*.so"]
}
```

Glob patterns. Uploaded after step succeeds. Available to downstream steps via tasked's artifact system. Downloaded automatically when a step depends on a step that has artifacts.

### Conditions

```
branch == 'main'
branch =~ /release-.*/
event == 'push'
event == 'pull_request'
```

Available: `branch`, `event`, `tag`. Evaluated at compile time (before submission to tasked). Step is skipped (marked succeeded with no output) if condition is false.

### Parallelism model

**Everything without `depends_on` runs in parallel.** This is tasked's natural model — root tasks (no dependencies) start immediately.

```json
{
  "steps": [
    { "key": "a", "command": "..." },
    { "key": "b", "command": "..." },
    { "key": "c", "command": "...", "depends_on": ["a", "b"] },
    { "key": "d", "command": "...", "depends_on": ["a"] }
  ]
}
```

a and b start immediately in parallel. d starts as soon as a finishes (doesn't wait for b). c starts when both a and b finish. This is optimal scheduling — tasked does this naturally.

No `wait` step. If you want a barrier, just add `depends_on` to the steps that need it. This is more explicit and produces better schedules.

## Compilation to tasked

The compiler is simpler than v1 because we're not fighting the model:

1. **Auto-key** — assign `step-N` keys to steps without one
2. **Matrix expansion** — cartesian product, fan-in wiring
3. **Checkout injection** — prepend checkout task as DAG root (all other root steps depend on it)
4. **Shorthand expansion** — `command` → shell, `container` → container, `block` → approval, `trigger` → trigger
5. **Env merging** — global → step → matrix
6. **Condition resolution** — evaluate `if` expressions, skip false steps
7. **Artifact injection** — add upload/download tasks around steps with `artifacts`
8. **Assemble** — emit FlowDef with all TaskDefs

No wait expansion pass needed (no waits). No concurrency group pass needed (tasked handles it via queues).

## Execution

Single `gauntlet` process:
- Embedded tasked engine with SQLite storage (durable across restarts)
- GitHub webhook listener (smee.io or ngrok for local dev, or direct if port-forwarded)
- On push/PR: clone repo, load pipeline, compile, submit to engine
- Steps run via shell or Docker on the Mac Studio
- Status reported back to GitHub via checks API

## What tasked gives us for free

- Parallel DAG execution with optimal scheduling
- Per-task retries with exponential backoff + jitter
- Timeouts with automatic detection and retry
- Variable interpolation (`${tasks.<id>.output.*}`)
- Artifact storage between tasks
- Sub-flow composition via trigger executor
- Dynamic task generation via spawn executor
- Approval gates
- All registered executors (shell, container, http, delay, integrations, remote, etc.)
- Durable state — if gauntlet crashes mid-pipeline, tasked resumes on restart

## Changes from v1

| v1 | v2 |
|---|---|
| `tasks` | `steps` |
| `id` | `key` |
| Explicit `executor` + `config` always | Shorthands + raw `executor` escape hatch |
| No implicit parallelism | Everything parallel unless `depends_on` |
| `timeout_secs` | `timeout` (seconds, shorter field name) |
| `retries` | `retry` |
| `cache` object per step | Deferred (host-level cache, not per-step) |
| `spawn` bool + `spawn_output` | Same (unchanged) |
| Rhai conditions | Simple expression language |
| MemoryStorage | SQLite (durable) |

# Plan: Service Containers & Concurrency Groups

## Feature 1: Service Containers

### Goal
Allow pipelines to declare long-running sidecar containers (Postgres, Redis, etc.) that start before steps run and are torn down after.

### Pipeline Schema Changes

Add a top-level `services` field and a per-step `services` field:

```json
{
  "services": {
    "postgres": {
      "image": "postgres:15",
      "env": { "POSTGRES_PASSWORD": "test" },
      "ports": ["5432:5432"],
      "health_check": {
        "command": "pg_isready",
        "interval": 2,
        "retries": 10
      }
    },
    "redis": {
      "image": "redis:7",
      "ports": ["6379:6379"]
    }
  },
  "steps": [
    {
      "key": "integration-tests",
      "command": "cargo test --features integration",
      "services": ["postgres", "redis"]
    }
  ]
}
```

**Design decisions:**
- Top-level `services` defines available services (like `defs`)
- Steps opt-in via `services: ["postgres"]` — only requested services start
- If a step doesn't reference any services, none are started (no waste)
- Services share a Docker network with the step container, accessible via service name as hostname

### Schema Changes (`schema.rs`)

1. Add `ServiceConfig` struct:
   ```rust
   pub struct ServiceConfig {
       pub image: String,
       pub env: HashMap<String, String>,
       pub ports: Vec<String>,
       pub health_check: Option<HealthCheckConfig>,
       pub command: Option<Vec<String>>,
       pub volumes: Vec<String>,
   }

   pub struct HealthCheckConfig {
       pub command: String,
       pub interval: Option<u64>,  // seconds, default 2
       pub retries: Option<u32>,   // default 10
       pub start_period: Option<u64>, // seconds, default 0
   }
   ```

2. Add to `Pipeline`: `pub services: HashMap<String, ServiceConfig>`
3. Add to `Step`: `pub services: Vec<String>`

### Compiler Changes (`compiler.rs`)

When a step references services:
- Pass the service configs into the container executor config as a `"services"` key
- The container executor handles lifecycle (start services → run step → stop services)

```rust
// In expand_executor, when step.services is non-empty:
let services: Vec<_> = step.services.iter()
    .map(|name| pipeline.services.get(name))
    .collect();
config["services"] = serde_json::to_value(&services)?;
```

### Tasked Changes

The `ContainerExecutor` (in tasked crate) needs to:
1. Create a Docker network for the step
2. Start service containers on that network (named by service key)
3. Health-check each service (retry loop with interval)
4. Run the main step container on the same network
5. Tear down all service containers + network on completion (success or failure)

This is self-contained within the executor — no engine changes needed.

### Validation (`compiler.rs`)

- Error if a step references a service not defined in top-level `services`
- Error if services are used with `runner: "host"` (services require Docker networking)

---

## Feature 2: Concurrency Groups

### Goal
Allow grouping builds so that a new build in a group cancels any in-progress build in the same group. Primary use case: push to the same branch should cancel the previous build.

### Pipeline Schema Changes

Add a top-level `concurrency` field:

```json
{
  "concurrency": {
    "group": "ci-${branch}",
    "cancel_in_progress": true
  }
}
```

- `group`: Template string. `${branch}`, `${repo}`, `${sha}` are substituted from BuildContext
- `cancel_in_progress`: If true, cancel running builds when a new one enters the group. Default: false (queue behavior — new build waits)

### Schema Changes (`schema.rs`)

```rust
pub struct ConcurrencyConfig {
    pub group: String,
    pub cancel_in_progress: bool,  // default false
}
```

Add to `Pipeline`: `pub concurrency: Option<ConcurrencyConfig>`

### Tasked Changes — Flow Cancellation API

Tasked currently has `FlowState::Cancelled` but no way to trigger it. Need to add:

```rust
impl Engine {
    /// Cancel a running flow. All pending/running tasks are moved to Cancelled state.
    /// Running tasks' executors are sent a cancellation signal.
    pub async fn cancel_flow(&self, flow_id: &FlowId) -> Result<(), Error>;
}
```

Implementation:
1. Set flow state to `Cancelled`
2. For each task in `Pending`/`Ready` state → move to `Cancelled`
3. For each task in `Running` state → send cancel signal via executor's cancel method (new trait method on `Executor`)
4. For container executor: `docker kill` the running container

### Serve Changes (`serve.rs`)

1. After compiling the pipeline, resolve the concurrency group key:
   ```rust
   let group_key = concurrency.group
       .replace("${branch}", &branch)
       .replace("${repo}", &repo)
       .replace("${sha}", &sha);
   ```

2. Track active concurrency groups in `AppState`:
   ```rust
   struct AppState {
       // existing fields...
       concurrency_groups: Mutex<HashMap<String, String>>,  // group_key → flow_id
   }
   ```

3. Before submitting a new flow, check for an existing flow in the same group:
   ```rust
   if cancel_in_progress {
       if let Some(old_flow_id) = groups.get(&group_key) {
           engine.cancel_flow(&FlowId(old_flow_id.clone())).await?;
       }
   }
   groups.insert(group_key, new_flow_id);
   ```

4. Clean up group entry when a flow completes (in `build_monitor`).

### Compiler Changes

Pass `ConcurrencyConfig` through `CompileResult` so `serve.rs` can access it:
```rust
pub struct CompileResult {
    pub flow_def: FlowDef,
    pub queue_config: QueueConfig,
    pub metadata: CompileMetadata,
    pub concurrency: Option<ConcurrencyConfig>,  // NEW
}
```

---

## Implementation Order

### Phase 1: Concurrency Groups (smaller scope, high value)
1. **Tasked**: Add `engine.cancel_flow()` API + executor cancel trait method
2. **Gauntlet schema**: Add `ConcurrencyConfig` to `Pipeline`
3. **Gauntlet compiler**: Pass concurrency config through `CompileResult`
4. **Gauntlet serve**: Implement group tracking and cancel-on-new-build logic
5. **Pipeline spec**: Document the `concurrency` field

### Phase 2: Service Containers (larger scope)
1. **Gauntlet schema**: Add `ServiceConfig`, `HealthCheckConfig` to schema
2. **Gauntlet compiler**: Pass service configs into executor config JSON
3. **Gauntlet validation**: Validate service references and runner compatibility
4. **Tasked ContainerExecutor**: Implement Docker network + sidecar lifecycle
5. **Pipeline spec**: Document the `services` field

### Files to Modify

| File | Changes |
|------|---------|
| `src/schema.rs` | Add `ServiceConfig`, `HealthCheckConfig`, `ConcurrencyConfig`, add fields to `Pipeline` and `Step` |
| `src/compiler.rs` | Pass services into executor config, pass concurrency through CompileResult, validate service refs |
| `src/serve.rs` | Concurrency group tracking, cancel-on-new-build, group cleanup |
| `docs/pipeline-spec.md` | Document both new features |
| **tasked** (external) | `engine.cancel_flow()`, executor cancel trait, container executor sidecar lifecycle |

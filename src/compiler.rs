use std::collections::{HashMap, HashSet};

use tasked::types::{FlowDef, QueueConfig, SecretRef, TaskDef, TaskId};
use thiserror::Error;

use crate::artifacts;
use crate::cache;
use crate::checkout::{self, CHECKOUT_TASK_ID};
use crate::matrix;
use crate::schema::{Pipeline, PipelineTask};

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("task '{0}' has unknown dependency '{1}'")]
    UnknownDependency(String, String),

    #[error("task '{0}' specifies both 'command' and 'executor' — use one or the other")]
    AmbiguousExecutor(String),

    #[error("task '{0}' has no executor — specify 'command', 'container', or 'executor'")]
    MissingExecutor(String),

    #[error("task '{0}' has empty matrix dimension '{1}'")]
    EmptyMatrixDimension(String, String),

    #[error("duplicate task id '{0}'")]
    DuplicateTaskId(String),
}

/// Build context provided by the CLI / webhook receiver.
#[derive(Debug, Clone, Default)]
pub struct BuildContext {
    pub repo_dir: Option<String>,
    pub git_ref: Option<String>,
    pub branch: Option<String>,
    pub event: Option<String>,
    pub env_overrides: HashMap<String, String>,
}

/// Metadata about the compilation for the TUI.
#[derive(Debug, Clone)]
pub struct CompileMetadata {
    /// Maps expanded task IDs back to original pipeline task IDs.
    pub task_origins: HashMap<String, String>,
    /// Matrix values for each expanded task.
    pub matrix_values: HashMap<String, HashMap<String, String>>,
    /// Task IDs that are synthetic (checkout, cache, artifact steps).
    pub synthetic_tasks: HashSet<String>,
}

/// Result of compiling a Pipeline into a Tasked FlowDef.
pub struct CompileResult {
    pub flow_def: FlowDef,
    pub queue_config: QueueConfig,
    pub metadata: CompileMetadata,
}

/// Compile a Pipeline into a Tasked FlowDef.
///
/// The compiler is pure and deterministic: given the same Pipeline and BuildContext,
/// it always produces the same output.
pub fn compile(pipeline: &Pipeline, ctx: &BuildContext) -> Result<CompileResult, CompileError> {
    let mut metadata = CompileMetadata {
        task_origins: HashMap::new(),
        matrix_values: HashMap::new(),
        synthetic_tasks: HashSet::new(),
    };

    // Pass 1: Validate
    validate(pipeline)?;

    // Pass 2: Matrix expansion
    let expanded_tasks = expand_matrices(pipeline, &mut metadata)?;

    // Pass 3-7: Build TaskDefs
    let task_defs = build_task_defs(pipeline, &expanded_tasks, ctx, &mut metadata);

    // Pass 8: Assemble
    let flow_def = FlowDef {
        tasks: task_defs,
        webhooks: None,
    };

    let queue_config = build_queue_config(pipeline);

    Ok(CompileResult {
        flow_def,
        queue_config,
        metadata,
    })
}

/// Pass 1: Validate the pipeline structure.
fn validate(pipeline: &Pipeline) -> Result<(), CompileError> {
    let mut seen_ids = HashSet::new();

    for task in &pipeline.tasks {
        if !seen_ids.insert(&task.id) {
            return Err(CompileError::DuplicateTaskId(task.id.clone()));
        }

        // Check executor specification.
        let has_command = task.command.is_some();
        let has_executor = task.executor.is_some();
        let has_container = task.container.is_some();

        let exec_count = [has_command, has_executor, has_container]
            .iter()
            .filter(|&&x| x)
            .count();

        if exec_count > 1 {
            return Err(CompileError::AmbiguousExecutor(task.id.clone()));
        }
        if exec_count == 0 {
            return Err(CompileError::MissingExecutor(task.id.clone()));
        }

        // Check matrix dimensions non-empty.
        if let Some(ref matrix) = task.matrix {
            for (key, values) in &matrix.dimensions {
                if values.is_empty() {
                    return Err(CompileError::EmptyMatrixDimension(
                        task.id.clone(),
                        key.clone(),
                    ));
                }
            }
        }
    }

    // Check depends_on references (allow deferred spawn refs containing '/').
    for task in &pipeline.tasks {
        for dep in &task.depends_on {
            if !dep.contains('/') && !seen_ids.contains(&dep.as_str().to_string()) {
                // Check if the dep matches any task's id (need to handle String vs &str)
                let found = pipeline.tasks.iter().any(|t| t.id == *dep);
                if !found {
                    return Err(CompileError::UnknownDependency(
                        task.id.clone(),
                        dep.clone(),
                    ));
                }
            }
        }
    }

    Ok(())
}

/// A task after matrix expansion (may be original or a matrix variant).
struct ExpandedTask {
    /// The new task ID (original or with matrix suffix).
    id: String,
    /// The original pipeline task.
    original: PipelineTask,
    /// Matrix values if this is a matrix variant.
    matrix_combo: Option<HashMap<String, String>>,
}

/// Pass 2: Expand matrix builds.
fn expand_matrices(
    pipeline: &Pipeline,
    metadata: &mut CompileMetadata,
) -> Result<Vec<ExpandedTask>, CompileError> {
    // Track which original IDs expand to which new IDs.
    let mut expansion_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut expanded = Vec::new();

    for task in &pipeline.tasks {
        if let Some(ref matrix_config) = task.matrix {
            let combos = matrix::expand(matrix_config);
            if combos.is_empty() {
                // No combinations — treat as non-matrix task.
                expansion_map
                    .entry(task.id.clone())
                    .or_default()
                    .push(task.id.clone());
                expanded.push(ExpandedTask {
                    id: task.id.clone(),
                    original: task.clone(),
                    matrix_combo: None,
                });
                continue;
            }

            for combo in &combos {
                let suffix = matrix::suffix(combo);
                let new_id = format!("{}-{suffix}", task.id);
                expansion_map
                    .entry(task.id.clone())
                    .or_default()
                    .push(new_id.clone());
                metadata
                    .task_origins
                    .insert(new_id.clone(), task.id.clone());
                metadata
                    .matrix_values
                    .insert(new_id.clone(), combo.clone());

                expanded.push(ExpandedTask {
                    id: new_id,
                    original: task.clone(),
                    matrix_combo: Some(combo.clone()),
                });
            }
        } else {
            expansion_map
                .entry(task.id.clone())
                .or_default()
                .push(task.id.clone());
            expanded.push(ExpandedTask {
                id: task.id.clone(),
                original: task.clone(),
                matrix_combo: None,
            });
        }
    }

    // Rewrite depends_on: if a dep refers to a matrix-expanded task,
    // replace with all expanded variants (fan-in).
    for task in &mut expanded {
        let mut new_deps = Vec::new();
        for dep in &task.original.depends_on {
            if let Some(expanded_ids) = expansion_map.get(dep) {
                new_deps.extend(expanded_ids.iter().cloned());
            } else {
                // Deferred spawn ref or already-expanded ref — keep as-is.
                new_deps.push(dep.clone());
            }
        }
        task.original.depends_on = new_deps;
    }

    Ok(expanded)
}

/// Pass 3-7: Build concrete TaskDefs from expanded tasks.
fn build_task_defs(
    pipeline: &Pipeline,
    expanded: &[ExpandedTask],
    ctx: &BuildContext,
    metadata: &mut CompileMetadata,
) -> Vec<TaskDef> {
    let mut task_defs: Vec<TaskDef> = Vec::new();

    // Collect IDs that have no dependencies (will become checkout dependents).
    // Pass 3: Checkout injection.
    if pipeline.checkout {
        let checkout_config = pipeline
            .checkout_config
            .clone()
            .unwrap_or_default();
        let checkout = checkout::checkout_task(&checkout_config, ctx);
        metadata.synthetic_tasks.insert(CHECKOUT_TASK_ID.to_string());
        task_defs.push(checkout);
    }

    for ex in expanded {
        let task = &ex.original;
        let task_id = &ex.id;

        // Collect this task's dependencies (may be modified by cache/artifact injection).
        let mut deps: Vec<String> = task.depends_on.clone();

        // If checkout is enabled and this task has no deps, depend on checkout.
        if pipeline.checkout && deps.is_empty() {
            deps.push(CHECKOUT_TASK_ID.to_string());
        }

        // Pass 4: Cache injection.
        if let Some(ref cache_config) = task.cache {
            let cache_key = resolve_cache_key(&cache_config.key, ex.matrix_combo.as_ref());
            let restore = cache::restore_task(task_id, &cache_key, &cache_config.paths);
            let save = cache::save_task(task_id, &cache_key, &cache_config.paths);

            let restore_id = restore.id.0.clone();
            let save_id = save.id.0.clone();

            // Restore depends on whatever the task originally depended on.
            let mut restore_def = restore;
            restore_def.depends_on = deps.iter().map(|d| TaskId(d.clone())).collect();

            // The main task now depends on cache restore.
            deps = vec![restore_id.clone()];

            // Save depends on the main task.
            let mut save_def = save;
            save_def.depends_on = vec![TaskId(task_id.clone())];

            metadata.synthetic_tasks.insert(restore_id);
            metadata.synthetic_tasks.insert(save_id);
            task_defs.push(restore_def);
            task_defs.push(save_def);
        }

        // Pass 5: Artifact download injection.
        if let Some(ref artifact_config) = task.artifacts
            && !artifact_config.download_from.is_empty()
        {
            let download =
                artifacts::download_task(task_id, &artifact_config.download_from);
            let download_id = download.id.0.clone();

            let mut download_def = download;
            download_def.depends_on = deps.iter().map(|d| TaskId(d.clone())).collect();
            deps = vec![download_id.clone()];

            metadata.synthetic_tasks.insert(download_id);
            task_defs.push(download_def);
        }

        // Pass 6: Shorthand expansion + env merge.
        let (executor, config) = expand_executor(task, &ex.matrix_combo, &pipeline.env, ctx);

        // Build the main TaskDef.
        let task_def = TaskDef {
            id: TaskId(task_id.clone()),
            executor,
            config,
            input: None,
            depends_on: deps.iter().map(|d| TaskId(d.clone())).collect(),
            timeout_secs: task.timeout_secs.or(pipeline.timeout_secs),
            retries: task.retries.or(pipeline.retries),
            backoff: None,
            condition: resolve_condition(&task.condition, ctx),
            spawn_output: task.spawn_output.clone(),
        };

        task_defs.push(task_def);

        // Pass 5 continued: Artifact upload injection.
        if let Some(ref artifact_config) = task.artifacts
            && !artifact_config.upload.is_empty()
        {
            let upload = artifacts::upload_task(task_id, &artifact_config.upload);
            let upload_id = upload.id.0.clone();

            let mut upload_def = upload;
            upload_def.depends_on = vec![TaskId(task_id.clone())];

            metadata.synthetic_tasks.insert(upload_id);
            task_defs.push(upload_def);
        }
    }

    task_defs
}

/// Expand executor shorthand and merge environment variables into the command.
fn expand_executor(
    task: &PipelineTask,
    matrix_combo: &Option<HashMap<String, String>>,
    global_env: &HashMap<String, String>,
    ctx: &BuildContext,
) -> (String, serde_json::Value) {
    // Merge env: global → task → matrix → ctx overrides.
    let mut env = global_env.clone();
    env.extend(task.env.clone());
    if let Some(combo) = matrix_combo {
        for (k, v) in combo {
            env.insert(format!("MATRIX_{}", k.to_uppercase()), v.clone());
        }
    }
    env.extend(ctx.env_overrides.clone());

    // Build env prefix for shell commands.
    let env_prefix = if env.is_empty() {
        String::new()
    } else {
        let mut exports: Vec<String> = env
            .iter()
            .map(|(k, v)| format!("export {}={}", k, shell_escape(v)))
            .collect();
        exports.sort(); // Deterministic ordering.
        format!("{}\n", exports.join("\n"))
    };

    if let Some(ref command) = task.command {
        let full_command = format!("{env_prefix}set -euo pipefail\n{command}");
        let executor_name = if task.spawn { "spawn" } else { "shell" };
        let config = serde_json::json!({ "command": full_command });
        (executor_name.to_string(), config)
    } else if let Some(ref container) = task.container {
        let config = serde_json::json!({
            "image": container.image,
            "command": container.command,
            "env": env,
            "working_dir": container.working_dir,
        });
        ("container".to_string(), config)
    } else if let Some(ref executor) = task.executor {
        let config = task.config.clone().unwrap_or(serde_json::Value::Null);
        (executor.clone(), config)
    } else {
        // Shouldn't reach here after validation.
        ("noop".to_string(), serde_json::json!({}))
    }
}

/// Resolve cache key by substituting matrix variables.
fn resolve_cache_key(key: &str, matrix_combo: Option<&HashMap<String, String>>) -> String {
    let mut resolved = key.to_string();
    if let Some(combo) = matrix_combo {
        for (k, v) in combo {
            resolved = resolved.replace(&format!("${{matrix.{k}}}"), v);
        }
    }
    resolved
}

/// Resolve condition by substituting CI variables.
fn resolve_condition(condition: &Option<String>, ctx: &BuildContext) -> Option<String> {
    condition.as_ref().map(|cond| {
        let mut resolved = cond.clone();
        if let Some(ref branch) = ctx.branch {
            resolved = resolved.replace("branch", &format!("'{branch}'"));
        }
        if let Some(ref event) = ctx.event {
            resolved = resolved.replace("event", &format!("'{event}'"));
        }
        resolved
    })
}

/// Escape a string for safe use in shell export statements.
fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        format!("\"{}\"", s)
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Build queue config from pipeline-level settings.
fn build_queue_config(pipeline: &Pipeline) -> QueueConfig {
    let mut config = QueueConfig::default();

    if let Some(retries) = pipeline.retries {
        config.max_retries = retries;
    }
    if let Some(timeout) = pipeline.timeout_secs {
        config.timeout_secs = timeout;
    }

    // Map pipeline secrets to Tasked SecretRef.
    if !pipeline.secrets.is_empty() {
        let mut secrets = HashMap::new();
        for (name, source) in &pipeline.secrets {
            secrets.insert(
                name.clone(),
                SecretRef {
                    env: source.env.clone(),
                    file: source.file.clone(),
                },
            );
        }
        config.secrets = Some(secrets);
    }

    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Pipeline;

    fn minimal_pipeline() -> Pipeline {
        serde_json::from_str(
            r#"{
                "checkout": false,
                "tasks": [
                    {"id": "test", "command": "cargo test"}
                ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn compile_minimal() {
        let pipeline = minimal_pipeline();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks.len(), 1);
        assert_eq!(result.flow_def.tasks[0].id.0, "test");
        assert_eq!(result.flow_def.tasks[0].executor, "shell");
    }

    #[test]
    fn compile_with_checkout() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "tasks": [
                    {"id": "test", "command": "cargo test"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        // Should have checkout + test.
        assert_eq!(result.flow_def.tasks.len(), 2);
        assert_eq!(result.flow_def.tasks[0].id.0, "__checkout");
        assert_eq!(result.flow_def.tasks[1].depends_on[0].0, "__checkout");
    }

    #[test]
    fn compile_with_deps() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "tasks": [
                    {"id": "lint", "command": "cargo clippy"},
                    {"id": "test", "command": "cargo test", "depends_on": ["lint"]},
                    {"id": "build", "command": "cargo build", "depends_on": ["test"]}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks.len(), 3);
        assert_eq!(result.flow_def.tasks[1].depends_on[0].0, "lint");
        assert_eq!(result.flow_def.tasks[2].depends_on[0].0, "test");
    }

    #[test]
    fn compile_matrix_expansion() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "tasks": [
                    {
                        "id": "test",
                        "command": "cargo test",
                        "matrix": {
                            "dimensions": {"toolchain": ["stable", "nightly"]}
                        }
                    },
                    {"id": "build", "command": "cargo build", "depends_on": ["test"]}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();

        // test-nightly, test-stable, build (with fan-in deps on both).
        let ids: Vec<&str> = result.flow_def.tasks.iter().map(|t| t.id.0.as_str()).collect();
        assert!(ids.contains(&"test-nightly"));
        assert!(ids.contains(&"test-stable"));
        assert!(ids.contains(&"build"));

        let build = result.flow_def.tasks.iter().find(|t| t.id.0 == "build").unwrap();
        assert_eq!(build.depends_on.len(), 2);
    }

    #[test]
    fn compile_env_merge() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "env": {"GLOBAL": "1"},
                "tasks": [
                    {"id": "test", "command": "echo hi", "env": {"LOCAL": "2"}}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        let config = &result.flow_def.tasks[0].config;
        let cmd = config["command"].as_str().unwrap();
        assert!(cmd.contains("GLOBAL"));
        assert!(cmd.contains("LOCAL"));
    }

    #[test]
    fn validate_unknown_dep() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "tasks": [
                    {"id": "test", "command": "echo", "depends_on": ["nonexistent"]}
                ]
            }"#,
        )
        .unwrap();
        let err = validate(&pipeline).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn validate_duplicate_id() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "tasks": [
                    {"id": "test", "command": "echo a"},
                    {"id": "test", "command": "echo b"}
                ]
            }"#,
        )
        .unwrap();
        let err = validate(&pipeline).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn validate_ambiguous_executor() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "tasks": [
                    {"id": "test", "command": "echo", "executor": "shell"}
                ]
            }"#,
        )
        .unwrap();
        let err = validate(&pipeline).unwrap_err();
        assert!(err.to_string().contains("both"));
    }

    #[test]
    fn spawn_ref_allowed() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "tasks": [
                    {"id": "discover", "command": "./find.sh", "spawn": true, "spawn_output": ["complete"]},
                    {"id": "deploy", "command": "echo done", "depends_on": ["discover/complete"]}
                ]
            }"#,
        )
        .unwrap();
        assert!(validate(&pipeline).is_ok());
    }
}

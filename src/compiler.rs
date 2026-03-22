use std::collections::{HashMap, HashSet};

use tasked::types::{FlowDef, QueueConfig, SecretRef, TaskDef, TaskId};
use thiserror::Error;

use crate::artifacts;
use crate::cache;
use crate::checkout::{self, CHECKOUT_TASK_ID};
use crate::matrix;
use crate::schema::{ArtifactSetting, MatrixConfig, MatrixSetting, Pipeline, Step};

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("step '{0}' has unknown dependency '{1}'")]
    UnknownDependency(String, String),

    #[error(
        "step '{0}' specifies multiple step types — use one of: command, commands, container, block, trigger, or executor"
    )]
    AmbiguousExecutor(String),

    #[error(
        "step '{0}' has no step type — specify command, commands, container, block, trigger, or executor"
    )]
    MissingExecutor(String),

    #[error("step '{0}' has empty matrix dimension '{1}'")]
    EmptyMatrixDimension(String, String),

    #[error("duplicate step key '{0}'")]
    DuplicateKey(String),
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
    /// Maps expanded step keys back to original pipeline step keys.
    pub task_origins: HashMap<String, String>,
    /// Matrix values for each expanded step.
    pub matrix_values: HashMap<String, HashMap<String, String>>,
    /// Step keys that are synthetic (checkout, cache, artifact steps).
    pub synthetic_tasks: HashSet<String>,
}

/// Result of compiling a Pipeline into a Tasked FlowDef.
pub struct CompileResult {
    pub flow_def: FlowDef,
    pub queue_config: QueueConfig,
    pub metadata: CompileMetadata,
}

/// Compile a Pipeline into a Tasked FlowDef.
pub fn compile(pipeline: &Pipeline, ctx: &BuildContext) -> Result<CompileResult, CompileError> {
    let mut metadata = CompileMetadata {
        task_origins: HashMap::new(),
        matrix_values: HashMap::new(),
        synthetic_tasks: HashSet::new(),
    };

    // Assign auto-keys to steps without one.
    let steps = auto_key(&pipeline.steps);

    // Pass 1: Validate
    validate(&steps, pipeline)?;

    // Pass 2: Matrix expansion
    let expanded = expand_matrices(&steps, &mut metadata)?;

    // Pass 3+: Build TaskDefs
    let task_defs = build_task_defs(pipeline, &expanded, ctx, &mut metadata);

    // Assemble
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

/// Assign auto-generated keys to steps that don't have one.
fn auto_key(steps: &[Step]) -> Vec<Step> {
    steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let mut s = step.clone();
            if s.key.is_none() {
                s.key = Some(format!("step-{i}"));
            }
            s
        })
        .collect()
}

/// Get the key of a step (must have been auto-keyed first).
fn step_key(step: &Step) -> &str {
    step.key.as_deref().unwrap()
}

/// Pass 1: Validate pipeline structure.
fn validate(steps: &[Step], pipeline: &Pipeline) -> Result<(), CompileError> {
    let mut seen_keys = HashSet::new();

    for step in steps {
        let key = step_key(step);

        if !seen_keys.insert(key.to_string()) {
            return Err(CompileError::DuplicateKey(key.to_string()));
        }

        // Check step type specification.
        let type_count = [
            step.command.is_some(),
            step.commands.is_some(),
            step.container.is_some(),
            step.block.is_some(),
            step.trigger.is_some(),
            step.executor.is_some(),
        ]
        .iter()
        .filter(|&&x| x)
        .count();

        if type_count > 1 {
            // Allow container + command (command runs inside the container)
            let is_container_command =
                step.container.is_some() && (step.command.is_some() || step.commands.is_some());
            if !is_container_command {
                return Err(CompileError::AmbiguousExecutor(key.to_string()));
            }
        }
        if type_count == 0 {
            return Err(CompileError::MissingExecutor(key.to_string()));
        }

        // Check matrix dimensions non-empty.
        if let Some(ref matrix) = step.matrix {
            let config = match matrix {
                MatrixSetting::Simple(v) => {
                    if v.is_empty() {
                        return Err(CompileError::EmptyMatrixDimension(
                            key.to_string(),
                            "matrix".to_string(),
                        ));
                    }
                    continue;
                }
                MatrixSetting::Multi(c) => c,
            };
            for (dim_key, values) in &config.dimensions {
                if values.is_empty() {
                    return Err(CompileError::EmptyMatrixDimension(
                        key.to_string(),
                        dim_key.clone(),
                    ));
                }
            }
        }
    }

    // Check depends_on references (allow deferred spawn refs containing '/').
    let _ = pipeline; // pipeline available for future use
    for step in steps {
        let key = step_key(step);
        for dep in step.depends_on.as_vec() {
            if !dep.contains('/') && !seen_keys.contains(&dep) {
                return Err(CompileError::UnknownDependency(key.to_string(), dep));
            }
        }
    }

    Ok(())
}

/// A step after matrix expansion.
struct ExpandedStep {
    key: String,
    original: Step,
    matrix_combo: Option<HashMap<String, String>>,
}

/// Pass 2: Expand matrix builds.
fn expand_matrices(
    steps: &[Step],
    metadata: &mut CompileMetadata,
) -> Result<Vec<ExpandedStep>, CompileError> {
    let mut expansion_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut expanded = Vec::new();

    for step in steps {
        let key = step_key(step).to_string();

        let matrix_config = match &step.matrix {
            Some(MatrixSetting::Simple(values)) => {
                // Convert simple matrix to multi-dimension with "matrix" as the key.
                Some(MatrixConfig {
                    dimensions: HashMap::from([("matrix".to_string(), values.clone())]),
                    exclude: vec![],
                })
            }
            Some(MatrixSetting::Multi(config)) => Some(config.clone()),
            None => None,
        };

        if let Some(ref config) = matrix_config {
            let combos = matrix::expand(config);
            if combos.is_empty() {
                expansion_map
                    .entry(key.clone())
                    .or_default()
                    .push(key.clone());
                expanded.push(ExpandedStep {
                    key: key.clone(),
                    original: step.clone(),
                    matrix_combo: None,
                });
                continue;
            }

            for combo in &combos {
                let suffix = matrix::suffix(combo);
                let new_key = format!("{key}-{suffix}");
                expansion_map
                    .entry(key.clone())
                    .or_default()
                    .push(new_key.clone());
                metadata.task_origins.insert(new_key.clone(), key.clone());
                metadata
                    .matrix_values
                    .insert(new_key.clone(), combo.clone());

                expanded.push(ExpandedStep {
                    key: new_key,
                    original: step.clone(),
                    matrix_combo: Some(combo.clone()),
                });
            }
        } else {
            expansion_map
                .entry(key.clone())
                .or_default()
                .push(key.clone());
            expanded.push(ExpandedStep {
                key: key.clone(),
                original: step.clone(),
                matrix_combo: None,
            });
        }
    }

    // Rewrite depends_on: if a dep refers to a matrix-expanded step,
    // replace with all expanded variants (fan-in).
    for ex in &mut expanded {
        let mut new_deps = Vec::new();
        for dep in ex.original.depends_on.as_vec() {
            if let Some(expanded_keys) = expansion_map.get(&dep) {
                new_deps.extend(expanded_keys.iter().cloned());
            } else {
                new_deps.push(dep);
            }
        }
        ex.original.depends_on = crate::schema::DependsOn::Multiple(new_deps);
    }

    Ok(expanded)
}

/// Build concrete TaskDefs from expanded steps.
fn build_task_defs(
    pipeline: &Pipeline,
    expanded: &[ExpandedStep],
    ctx: &BuildContext,
    metadata: &mut CompileMetadata,
) -> Vec<TaskDef> {
    let mut task_defs: Vec<TaskDef> = Vec::new();

    // Checkout injection.
    if pipeline.checkout.is_enabled() {
        let checkout_config = pipeline.checkout.config();
        let checkout = checkout::checkout_task(&checkout_config, ctx);
        metadata
            .synthetic_tasks
            .insert(CHECKOUT_TASK_ID.to_string());
        task_defs.push(checkout);
    }

    for ex in expanded {
        let step = &ex.original;
        let step_key = &ex.key;

        let mut deps: Vec<String> = step.depends_on.as_vec();

        // If checkout is enabled and this step has no deps, depend on checkout.
        if pipeline.checkout.is_enabled() && deps.is_empty() {
            deps.push(CHECKOUT_TASK_ID.to_string());
        }

        // Cache injection.
        if let Some(ref cache_config) = step.cache {
            let cache_key = resolve_cache_key(&cache_config.key, ex.matrix_combo.as_ref());
            let restore = cache::restore_task(step_key, &cache_key, &cache_config.paths);
            let save = cache::save_task(step_key, &cache_key, &cache_config.paths);

            let restore_id = restore.id.0.clone();
            let save_id = save.id.0.clone();

            let mut restore_def = restore;
            restore_def.depends_on = deps.iter().map(|d| TaskId(d.clone())).collect();
            deps = vec![restore_id.clone()];

            let mut save_def = save;
            save_def.depends_on = vec![TaskId(step_key.clone())];

            metadata.synthetic_tasks.insert(restore_id);
            metadata.synthetic_tasks.insert(save_id);
            task_defs.push(restore_def);
            task_defs.push(save_def);
        }

        // Artifact download injection.
        let download_from = match &step.artifacts {
            Some(ArtifactSetting::Full(cfg)) if !cfg.download_from.is_empty() => {
                Some(cfg.download_from.clone())
            }
            _ => None,
        };
        if let Some(ref sources) = download_from {
            let download = artifacts::download_task(step_key, sources);
            let download_id = download.id.0.clone();

            let mut download_def = download;
            download_def.depends_on = deps.iter().map(|d| TaskId(d.clone())).collect();
            deps = vec![download_id.clone()];

            metadata.synthetic_tasks.insert(download_id);
            task_defs.push(download_def);
        }

        // Expand step type to executor + config.
        let (executor, config) = expand_executor(step, &ex.matrix_combo, &pipeline.env, ctx);

        // Build the main TaskDef.
        let task_def = TaskDef {
            id: TaskId(step_key.clone()),
            executor,
            config,
            input: None,
            depends_on: deps.iter().map(|d| TaskId(d.clone())).collect(),
            timeout_secs: step.timeout.or(pipeline.timeout),
            retries: step.retry.or(pipeline.retry),
            backoff: None,
            condition: resolve_condition(&step.condition, ctx),
            spawn_output: step.spawn_output.clone(),
        };

        task_defs.push(task_def);

        // Artifact upload injection.
        let upload_globs = match &step.artifacts {
            Some(ArtifactSetting::Globs(globs)) => Some(globs.clone()),
            Some(ArtifactSetting::Full(cfg)) if !cfg.upload.is_empty() => Some(cfg.upload.clone()),
            _ => None,
        };
        if let Some(globs) = upload_globs {
            let upload = artifacts::upload_task(step_key, &globs);
            let upload_id = upload.id.0.clone();

            let mut upload_def = upload;
            upload_def.depends_on = vec![TaskId(step_key.clone())];

            metadata.synthetic_tasks.insert(upload_id);
            task_defs.push(upload_def);
        }
    }

    task_defs
}

/// Expand step type to executor name + config JSON.
fn expand_executor(
    step: &Step,
    matrix_combo: &Option<HashMap<String, String>>,
    global_env: &HashMap<String, String>,
    ctx: &BuildContext,
) -> (String, serde_json::Value) {
    // Merge env: global → step → matrix → ctx overrides.
    let mut env = global_env.clone();
    env.extend(step.env.clone());
    if let Some(combo) = matrix_combo {
        for (k, v) in combo {
            env.insert(format!("MATRIX_{}", k.to_uppercase()), v.clone());
        }
    }
    env.extend(ctx.env_overrides.clone());

    let env_prefix = if env.is_empty() {
        String::new()
    } else {
        let mut exports: Vec<String> = env
            .iter()
            .map(|(k, v)| format!("export {}={}", k, shell_escape(v)))
            .collect();
        exports.sort();
        format!("{}\n", exports.join("\n"))
    };

    // Block step → approval executor.
    if let Some(ref message) = step.block {
        return (
            "approval".to_string(),
            serde_json::json!({ "message": message }),
        );
    }

    // Trigger step → trigger executor.
    if let Some(ref trigger) = step.trigger {
        return (
            "trigger".to_string(),
            serde_json::json!({
                "pipeline": trigger.pipeline,
                "env": trigger.env,
            }),
        );
    }

    // Raw executor escape hatch.
    if let Some(ref executor) = step.executor {
        let config = step.config.clone().unwrap_or(serde_json::json!({}));
        return (executor.clone(), config);
    }

    // Container step (with optional command inside).
    if let Some(ref container) = step.container {
        let command = step
            .command
            .clone()
            .or_else(|| step.commands.as_ref().map(|cmds| cmds.join(" && ")));

        let config = serde_json::json!({
            "image": container.image,
            "command": command.map(|c| vec!["sh".to_string(), "-c".to_string(), format!("{env_prefix}set -euo pipefail\n{c}")]),
            "env": env,
            "working_dir": container.working_dir,
        });
        return ("container".to_string(), config);
    }

    // Command(s) step → shell executor.
    let command = if let Some(ref cmd) = step.command {
        cmd.clone()
    } else if let Some(ref cmds) = step.commands {
        cmds.join(" && ")
    } else {
        // Shouldn't reach here after validation.
        return ("noop".to_string(), serde_json::json!({}));
    };

    // Substitute matrix variables in the command.
    let mut full_command = command;
    if let Some(combo) = matrix_combo {
        for (k, v) in combo {
            full_command = full_command.replace(&format!("${{{k}}}"), v);
            full_command = full_command.replace(&format!("${{matrix.{k}}}"), v);
            full_command = full_command.replace("${matrix}", v);
        }
    }

    let full_command = format!("{env_prefix}set -euo pipefail\n{full_command}");
    let executor_name = if step.spawn { "spawn" } else { "shell" };
    let config = serde_json::json!({ "command": full_command });
    (executor_name.to_string(), config)
}

/// Resolve cache key by substituting matrix variables.
fn resolve_cache_key(key: &str, matrix_combo: Option<&HashMap<String, String>>) -> String {
    let mut resolved = key.to_string();
    if let Some(combo) = matrix_combo {
        for (k, v) in combo {
            resolved = resolved.replace(&format!("${{matrix.{k}}}"), v);
            resolved = resolved.replace("${matrix}", v);
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

fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        format!("\"{}\"", s)
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn build_queue_config(pipeline: &Pipeline) -> QueueConfig {
    let mut config = QueueConfig::default();

    if let Some(retry) = pipeline.retry {
        config.max_retries = retry;
    }
    if let Some(timeout) = pipeline.timeout {
        config.timeout_secs = timeout;
    }

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

    fn minimal_pipeline() -> Pipeline {
        serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "test", "command": "cargo test"}
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
                "steps": [
                    {"key": "test", "command": "cargo test"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks.len(), 2);
        assert_eq!(result.flow_def.tasks[0].id.0, "__checkout");
        assert_eq!(result.flow_def.tasks[1].depends_on[0].0, "__checkout");
    }

    #[test]
    fn compile_with_deps() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "lint", "command": "cargo clippy"},
                    {"key": "test", "command": "cargo test", "depends_on": ["lint"]},
                    {"key": "build", "command": "cargo build", "depends_on": ["test"]}
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
    fn compile_single_depends_on() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "a", "command": "echo a"},
                    {"key": "b", "command": "echo b", "depends_on": "a"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[1].depends_on[0].0, "a");
    }

    #[test]
    fn compile_matrix_simple() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {
                        "key": "test",
                        "command": "cargo test --features ${matrix}",
                        "matrix": ["serde", "tokio"]
                    }
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks.len(), 2);
        let ids: Vec<&str> = result
            .flow_def
            .tasks
            .iter()
            .map(|t| t.id.0.as_str())
            .collect();
        assert!(ids.contains(&"test-serde"));
        assert!(ids.contains(&"test-tokio"));
    }

    #[test]
    fn compile_matrix_multi() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {
                        "key": "test",
                        "command": "cargo test",
                        "matrix": {
                            "dimensions": {"toolchain": ["stable", "nightly"]}
                        }
                    },
                    {"key": "build", "command": "cargo build", "depends_on": ["test"]}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();

        let ids: Vec<&str> = result
            .flow_def
            .tasks
            .iter()
            .map(|t| t.id.0.as_str())
            .collect();
        assert!(ids.contains(&"test-nightly"));
        assert!(ids.contains(&"test-stable"));
        assert!(ids.contains(&"build"));

        let build = result
            .flow_def
            .tasks
            .iter()
            .find(|t| t.id.0 == "build")
            .unwrap();
        assert_eq!(build.depends_on.len(), 2);
    }

    #[test]
    fn compile_block_step() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "approve", "block": "Deploy?"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].executor, "approval");
    }

    #[test]
    fn compile_executor_escape_hatch() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "notify", "executor": "http", "config": {"url": "https://example.com"}}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].executor, "http");
    }

    #[test]
    fn compile_commands_joined() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "build", "commands": ["make clean", "make build"]}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        let cmd = result.flow_def.tasks[0].config["command"].as_str().unwrap();
        assert!(cmd.contains("make clean && make build"));
    }

    #[test]
    fn compile_env_merge() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "env": {"GLOBAL": "1"},
                "steps": [
                    {"key": "test", "command": "echo hi", "env": {"LOCAL": "2"}}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        let cmd = result.flow_def.tasks[0].config["command"].as_str().unwrap();
        assert!(cmd.contains("GLOBAL"));
        assert!(cmd.contains("LOCAL"));
    }

    #[test]
    fn compile_auto_key() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"command": "echo a"},
                    {"command": "echo b"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].id.0, "step-0");
        assert_eq!(result.flow_def.tasks[1].id.0, "step-1");
    }

    #[test]
    fn validate_unknown_dep() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "test", "command": "echo", "depends_on": ["nonexistent"]}
                ]
            }"#,
        )
        .unwrap();
        let steps = auto_key(&pipeline.steps);
        let err = validate(&steps, &pipeline).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn validate_duplicate_key() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "test", "command": "echo a"},
                    {"key": "test", "command": "echo b"}
                ]
            }"#,
        )
        .unwrap();
        let steps = auto_key(&pipeline.steps);
        let err = validate(&steps, &pipeline).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn validate_ambiguous_executor() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "test", "command": "echo", "executor": "shell"}
                ]
            }"#,
        )
        .unwrap();
        let steps = auto_key(&pipeline.steps);
        let err = validate(&steps, &pipeline).unwrap_err();
        assert!(err.to_string().contains("multiple"));
    }

    #[test]
    fn spawn_ref_allowed() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "discover", "command": "./find.sh", "spawn": true, "spawn_output": ["complete"]},
                    {"key": "deploy", "command": "echo done", "depends_on": ["discover/complete"]}
                ]
            }"#,
        )
        .unwrap();
        let steps = auto_key(&pipeline.steps);
        assert!(validate(&steps, &pipeline).is_ok());
    }

    #[test]
    fn v1_compat() {
        // v1 format with "tasks" and "id" should still parse and compile
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "tasks": [
                    {"id": "test", "command": "cargo test", "retries": 2, "timeout_secs": 300}
                ],
                "retries": 1,
                "timeout_secs": 600
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].id.0, "test");
        assert_eq!(result.flow_def.tasks[0].retries, Some(2));
        assert_eq!(result.flow_def.tasks[0].timeout_secs, Some(300));
        assert_eq!(result.queue_config.max_retries, 1);
        assert_eq!(result.queue_config.timeout_secs, 600);
    }
}

use std::collections::{HashMap, HashSet};

use tasked::types::{FlowDef, QueueConfig, SecretRef, TaskDef, TaskId};
use thiserror::Error;

use crate::artifacts;
use crate::cache;
use crate::checkout::{self, CHECKOUT_TASK_ID};
use crate::matrix;
use crate::schema::{ArtifactConfig, ArtifactSetting, MatrixSetting, Pipeline, RunnerConfig, Step};

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("step '{0}' has unknown dependency '{1}'")]
    UnknownDependency(String, String),

    #[error(
        "step '{0}' specifies multiple executor types — use only one of command/commands/container/block/trigger/executor"
    )]
    AmbiguousExecutor(String),

    #[error(
        "step '{0}' has no executor — specify 'command', 'commands', 'container', 'block', 'trigger', or 'executor'"
    )]
    MissingExecutor(String),

    #[error("step '{0}' has empty matrix dimension '{1}'")]
    EmptyMatrixDimension(String, String),

    #[error("duplicate step key '{0}'")]
    DuplicateTaskId(String),

    #[error("step '{0}' references unknown definition '{1}'")]
    UnknownDef(String, String),
}

/// Build context provided by the CLI / webhook receiver.
#[derive(Debug, Clone, Default)]
pub struct BuildContext {
    pub repo_dir: Option<String>,
    pub git_ref: Option<String>,
    pub branch: Option<String>,
    pub event: Option<String>,
    pub env_overrides: HashMap<String, String>,
    /// Extra Docker volume mounts: (host_path, container_path).
    pub extra_volumes: Vec<(String, String)>,
    /// Per-step workspace directories. Key = step key, value = absolute path.
    /// If empty, all steps share `repo_dir`.
    pub step_workspaces: HashMap<String, String>,
    /// Shared artifacts directory for upload/download between steps.
    pub artifacts_dir: Option<String>,
    /// GitHub token for private repo access inside containers.
    pub github_token: Option<String>,
}

/// Metadata about the compilation for the TUI.
#[derive(Debug, Clone)]
pub struct CompileMetadata {
    /// Maps expanded task IDs back to original pipeline step keys.
    pub task_origins: HashMap<String, String>,
    /// Matrix values for each expanded task.
    pub matrix_values: HashMap<String, HashMap<String, String>>,
    /// Task IDs that are synthetic (checkout, cache, artifact steps).
    pub synthetic_tasks: HashSet<String>,
}

/// Result of compiling a Pipeline into a Tasked FlowDef.
#[derive(Debug)]
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

    // Pass 0: Resolve `use` references — merge def fields into steps.
    let resolved_pipeline = resolve_defs(pipeline)?;
    let pipeline = &resolved_pipeline;

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

/// Pass 0: Resolve `use` references by merging definition fields into steps.
fn resolve_defs(pipeline: &Pipeline) -> Result<Pipeline, CompileError> {
    // Check if any step uses a def — skip if none do.
    let has_use = pipeline.steps.iter().any(|s| s.use_def.is_some());
    if !has_use {
        return Ok(pipeline.clone());
    }

    let mut resolved = pipeline.clone();
    for (i, step) in resolved.steps.iter_mut().enumerate() {
        let def_name = match &step.use_def {
            Some(name) => name.clone(),
            None => continue,
        };

        let def = pipeline.defs.get(&def_name).ok_or_else(|| {
            let key = step.key.clone().unwrap_or_else(|| format!("step-{i}"));
            CompileError::UnknownDef(key, def_name.clone())
        })?;

        // Merge: step fields override def fields.
        if step.runner.is_none() {
            step.runner = def.runner.clone();
        }
        if step.timeout.is_none() {
            step.timeout = def.timeout;
        }
        if step.retry.is_none() {
            step.retry = def.retry;
        }
        if !step.soft_fail && def.soft_fail {
            step.soft_fail = true;
        }
        if step.command.is_none() {
            step.command = def.command.clone();
        }
        if step.commands.is_none() {
            step.commands = def.commands.clone();
        }

        // Env: def is base, step merges on top.
        if !def.env.is_empty() {
            let mut merged = def.env.clone();
            merged.extend(step.env.clone());
            step.env = merged;
        }

        // Conditions: AND together.
        match (&def.condition, &step.condition) {
            (Some(def_cond), Some(step_cond)) => {
                step.condition = Some(format!("({def_cond}) && ({step_cond})"));
            }
            (Some(def_cond), None) => {
                step.condition = Some(def_cond.clone());
            }
            _ => {} // Step condition only, or neither.
        }
    }

    Ok(resolved)
}

/// Get the effective ID for a step (key or auto-generated).
fn step_id(step: &Step, index: usize) -> String {
    step.key.clone().unwrap_or_else(|| format!("step-{index}"))
}

/// Pass 1: Validate the pipeline structure.
fn validate(pipeline: &Pipeline) -> Result<(), CompileError> {
    let mut seen_ids = HashSet::new();

    for (i, step) in pipeline.steps.iter().enumerate() {
        let id = step_id(step, i);

        if !seen_ids.insert(id.clone()) {
            return Err(CompileError::DuplicateTaskId(id));
        }

        // Check executor specification — exactly one step type must be set.
        let has_command = step.command.is_some();
        let has_commands = step.commands.is_some();
        let has_executor = step.executor.is_some();
        let has_container = step.container.is_some();
        let has_block = step.block.is_some();
        let has_trigger = step.trigger.is_some();

        let exec_count = [
            has_command,
            has_commands,
            has_executor,
            has_container,
            has_block,
            has_trigger,
        ]
        .iter()
        .filter(|&&x| x)
        .count();

        // container + command/commands is allowed (run command inside container)
        let effective_count = if has_container && (has_command || has_commands) {
            1
        } else {
            exec_count
        };

        if effective_count > 1 {
            return Err(CompileError::AmbiguousExecutor(id));
        }
        if effective_count == 0 {
            return Err(CompileError::MissingExecutor(id));
        }

        // Check matrix dimensions non-empty.
        if let Some(ref matrix) = step.matrix {
            let config = matrix_to_config(matrix);
            for (key, values) in &config.dimensions {
                if values.is_empty() {
                    return Err(CompileError::EmptyMatrixDimension(id.clone(), key.clone()));
                }
            }
        }
    }

    // Check depends_on references (allow deferred spawn refs containing '/').
    for (i, step) in pipeline.steps.iter().enumerate() {
        let id = step_id(step, i);
        for dep in step.depends_on.as_vec() {
            if !dep.contains('/') && !seen_ids.contains(&dep) {
                return Err(CompileError::UnknownDependency(id.clone(), dep));
            }
        }
    }

    Ok(())
}

/// Convert a MatrixSetting to a MatrixConfig for expansion.
fn matrix_to_config(setting: &MatrixSetting) -> crate::schema::MatrixConfig {
    match setting {
        MatrixSetting::Simple(values) => crate::schema::MatrixConfig {
            dimensions: HashMap::from([("matrix".to_string(), values.clone())]),
            exclude: vec![],
        },
        MatrixSetting::Multi(config) => config.clone(),
    }
}

/// Normalize an ArtifactSetting into an ArtifactConfig for uniform handling.
fn artifact_to_config(setting: &ArtifactSetting) -> ArtifactConfig {
    match setting {
        ArtifactSetting::Globs(globs) => ArtifactConfig {
            upload: globs.clone(),
            download_from: vec![],
        },
        ArtifactSetting::Full(config) => config.clone(),
    }
}

/// A task after matrix expansion (may be original or a matrix variant).
struct ExpandedTask {
    /// The new task ID (original or with matrix suffix).
    id: String,
    /// The original pipeline step.
    original: Step,
    /// Resolved depends_on as a flat Vec (after matrix fan-in rewrite).
    depends_on: Vec<String>,
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

    for (i, step) in pipeline.steps.iter().enumerate() {
        let id = step_id(step, i);
        let deps = step.depends_on.as_vec();

        if let Some(ref matrix_setting) = step.matrix {
            let matrix_config = matrix_to_config(matrix_setting);
            let combos = matrix::expand(&matrix_config);
            if combos.is_empty() {
                // No combinations — treat as non-matrix step.
                expansion_map
                    .entry(id.clone())
                    .or_default()
                    .push(id.clone());
                expanded.push(ExpandedTask {
                    id: id.clone(),
                    original: step.clone(),
                    depends_on: deps,
                    matrix_combo: None,
                });
                continue;
            }

            for combo in &combos {
                let suffix = matrix::suffix(combo);
                let new_id = format!("{id}-{suffix}");
                expansion_map
                    .entry(id.clone())
                    .or_default()
                    .push(new_id.clone());
                metadata.task_origins.insert(new_id.clone(), id.clone());
                metadata.matrix_values.insert(new_id.clone(), combo.clone());

                expanded.push(ExpandedTask {
                    id: new_id,
                    original: step.clone(),
                    depends_on: deps.clone(),
                    matrix_combo: Some(combo.clone()),
                });
            }
        } else {
            expansion_map
                .entry(id.clone())
                .or_default()
                .push(id.clone());
            expanded.push(ExpandedTask {
                id: id.clone(),
                original: step.clone(),
                depends_on: deps,
                matrix_combo: None,
            });
        }
    }

    // Rewrite depends_on: if a dep refers to a matrix-expanded step,
    // replace with all expanded variants (fan-in).
    for task in &mut expanded {
        let mut new_deps = Vec::new();
        for dep in &task.depends_on {
            if let Some(expanded_ids) = expansion_map.get(dep) {
                new_deps.extend(expanded_ids.iter().cloned());
            } else {
                // Deferred spawn ref or already-expanded ref — keep as-is.
                new_deps.push(dep.clone());
            }
        }
        task.depends_on = new_deps;
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

    // Pass 3: Checkout injection.
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
        let task_id = &ex.id;

        // Collect this task's dependencies (may be modified by cache/artifact injection).
        let mut deps: Vec<String> = ex.depends_on.clone();

        // If checkout is enabled and this task has no deps, depend on checkout.
        if pipeline.checkout.is_enabled() && deps.is_empty() {
            deps.push(CHECKOUT_TASK_ID.to_string());
        }

        // Pass 4: Cache injection.
        if let Some(ref cache_config) = step.cache {
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
        // Auto-download: if this step depends on steps that have artifacts,
        // automatically download those artifacts. No explicit download_from needed.
        let mut auto_download_sources: Vec<String> = Vec::new();
        for dep_key in &deps {
            // Find the original step for this dependency.
            if let Some(dep_step) = pipeline.steps.iter().find(|s| {
                s.key.as_deref() == Some(dep_key) || s.key.is_none() // auto-keyed steps handled by index
            }) && let Some(ref art) = dep_step.artifacts
            {
                let ac = artifact_to_config(art);
                if !ac.upload.is_empty() {
                    auto_download_sources.push(dep_key.clone());
                }
            }
        }

        // Combine auto-download sources with explicit download_from.
        let artifact_config = step.artifacts.as_ref().map(artifact_to_config);
        let explicit_sources = artifact_config
            .as_ref()
            .map(|ac| ac.download_from.clone())
            .unwrap_or_default();

        let mut all_download_sources = auto_download_sources;
        all_download_sources.extend(explicit_sources);
        all_download_sources.dedup();

        if !all_download_sources.is_empty() {
            let artifacts_dir = ctx
                .artifacts_dir
                .as_deref()
                .unwrap_or("/tmp/gauntlet-artifacts");
            let download = artifacts::download_task(task_id, &all_download_sources, artifacts_dir);
            let download_id = download.id.0.clone();

            let mut download_def = download;
            download_def.depends_on = deps.iter().map(|d| TaskId(d.clone())).collect();
            deps = vec![download_id.clone()];

            metadata.synthetic_tasks.insert(download_id);
            task_defs.push(download_def);
        }

        // Resolve effective runner: step-level overrides pipeline-level.
        let effective_runner = step.runner.as_ref().or(pipeline.runner.as_ref());

        // Pass 6: Shorthand expansion + env merge.
        let (executor, config) = expand_executor(
            step,
            task_id,
            &ex.matrix_combo,
            &pipeline.env,
            ctx,
            effective_runner,
        );

        // Build the main TaskDef.
        let task_def = TaskDef {
            id: TaskId(task_id.clone()),
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

        // Pass 5 continued: Artifact upload injection.
        if let Some(ref ac) = artifact_config
            && !ac.upload.is_empty()
        {
            let artifacts_dir = ctx
                .artifacts_dir
                .as_deref()
                .unwrap_or("/tmp/gauntlet-artifacts");
            let upload = artifacts::upload_task(task_id, &ac.upload, artifacts_dir);
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
    step: &Step,
    step_key: &str,
    matrix_combo: &Option<HashMap<String, String>>,
    global_env: &HashMap<String, String>,
    ctx: &BuildContext,
    runner: Option<&RunnerConfig>,
) -> (String, serde_json::Value) {
    // Resolve per-step workspace (falls back to repo_dir).
    let workspace_dir = ctx
        .step_workspaces
        .get(step_key)
        .or(ctx.repo_dir.as_ref())
        .cloned();
    // Merge env: global -> step -> matrix -> ctx overrides.
    let mut env = global_env.clone();
    env.extend(step.env.clone());
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

    // Resolve the shell command body (command or commands shorthand).
    let shell_body = if let Some(ref command) = step.command {
        Some(command.clone())
    } else if let Some(ref commands) = step.commands {
        Some(commands.join(" && "))
    } else {
        None
    };

    // Substitute matrix variables in command.
    let shell_body = shell_body.map(|mut body| {
        if let Some(combo) = matrix_combo {
            for (k, v) in combo {
                body = body.replace(&format!("${{{k}}}"), v);
                body = body.replace(&format!("${{matrix.{k}}}"), v);
                body = body.replace("${matrix}", v);
            }
        }
        body
    });

    // Explicit container step (overrides runner).
    if let Some(ref container) = step.container {
        let cmd = shell_body.unwrap_or_default();
        let full_cmd = format!("{env_prefix}set -euo pipefail\n{cmd}");
        let config = serde_json::json!({
            "image": container.image,
            "command": ["sh", "-c", full_cmd],
            "env": env,
            "working_dir": container.working_dir,
        });
        return ("container".to_string(), config);
    }

    // Shell command — may be wrapped in a Docker container via runner.
    if let Some(body) = shell_body {
        let full_command = format!("{env_prefix}set -euo pipefail\n{body}");

        // Check if the runner specifies a Docker image.
        let docker_image = runner.and_then(|r| r.docker_image());

        if let Some(image) = docker_image {
            // Wrap in container executor with workspace + cache volume mounts.
            let mut volumes = vec![];

            // Mount workspace directory if available.
            if let Some(ref dir) = workspace_dir {
                volumes.push(format!("{dir}:/workspace"));
            }

            // Mount common cache directories for faster builds.
            let cache_base = dirs::home_dir().unwrap_or_default().join(".gauntlet/cache");
            let cache_mounts = [
                ("cargo/registry", "/usr/local/cargo/registry"),
                ("cargo/git", "/usr/local/cargo/git"),
                ("npm", "/root/.npm"),
                ("pip", "/root/.cache/pip"),
            ];
            for (host_sub, container_path) in &cache_mounts {
                let host_path = cache_base.join(host_sub);
                // Create the host directory if it doesn't exist.
                let _ = std::fs::create_dir_all(&host_path);
                volumes.push(format!("{}:{container_path}", host_path.display()));
            }

            // Add extra volume mounts (e.g., sibling repo worktrees for path deps).
            for (host_path, container_path) in &ctx.extra_volumes {
                volumes.push(format!("{host_path}:{container_path}"));
            }

            // Enable git CLI for cargo so it can use credentials for private deps.
            env.insert(
                "CARGO_NET_GIT_FETCH_WITH_CLI".to_string(),
                "true".to_string(),
            );

            // Inject git credential setup for private repo access.
            // Uses the GitHub App's installation token (not user credentials).
            let git_setup = if let Some(ref token) = ctx.github_token {
                format!(
                    "git config --global credential.helper '!f() {{ echo \"password={token}\"; }}; f'\n\
                     git config --global url.\"https://x-access-token@github.com/\".insteadOf \"https://github.com/\"\n"
                )
            } else {
                String::new()
            };

            // Prepend setup commands from the runner config.
            let setup = runner.and_then(|r| r.setup()).unwrap_or("");
            let setup_prefix = if setup.is_empty() {
                String::new()
            } else {
                format!("{setup}\n")
            };

            let container_command = format!("{git_setup}{setup_prefix}{full_command}");

            let config = serde_json::json!({
                "image": image,
                "command": ["sh", "-c", container_command],
                "env": env,
                "volumes": volumes,
                "working_dir": "/workspace",
            });

            return ("container".to_string(), config);
        }

        // No runner or host runner — run directly in shell.
        // cd into workspace if available.
        let full_command = if let Some(ref dir) = workspace_dir {
            format!("cd {dir}\n{full_command}")
        } else {
            full_command
        };
        let executor_name = if step.spawn { "spawn" } else { "shell" };
        let config = serde_json::json!({ "command": full_command });
        return (executor_name.to_string(), config);
    }

    if let Some(ref block_msg) = step.block {
        let config = serde_json::json!({ "message": block_msg });
        ("approval".to_string(), config)
    } else if let Some(ref trigger) = step.trigger {
        let config = serde_json::json!({
            "pipeline": trigger.pipeline,
            "env": trigger.env,
        });
        ("trigger".to_string(), config)
    } else if let Some(ref executor) = step.executor {
        let config = step.config.clone().unwrap_or(serde_json::Value::Null);
        (executor.clone(), config)
    } else {
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
        // Build variable bindings for Rhai evaluation.
        // Variables are set via `let x = "value";` prefix before the condition.
        let mut bindings = Vec::new();
        if let Some(ref branch) = ctx.branch {
            bindings.push(format!("let branch = \"{branch}\";"));
        } else {
            bindings.push("let branch = \"\";".to_string());
        }
        if let Some(ref event) = ctx.event {
            bindings.push(format!("let event = \"{event}\";"));
        } else {
            bindings.push("let event = \"\";".to_string());
        }
        // Convert single-quoted strings to double-quoted for Rhai compatibility.
        let cond = cond.replace('\'', "\"");
        format!("{} {cond}", bindings.join(" "))
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

    if let Some(retry) = pipeline.retry {
        config.max_retries = retry;
    }
    if let Some(timeout) = pipeline.timeout {
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
    fn compile_matrix_expansion() {
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

        // test-nightly, test-stable, build (with fan-in deps on both).
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
                "steps": [
                    {"key": "test", "command": "echo", "depends_on": ["nonexistent"]}
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
                "steps": [
                    {"key": "test", "command": "echo a"},
                    {"key": "test", "command": "echo b"}
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
                "steps": [
                    {"key": "test", "command": "echo", "executor": "shell"}
                ]
            }"#,
        )
        .unwrap();
        let err = validate(&pipeline).unwrap_err();
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
        assert!(validate(&pipeline).is_ok());
    }

    #[test]
    fn compile_commands_shorthand() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "build", "commands": ["cargo build", "cargo test"]}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        let cmd = result.flow_def.tasks[0].config["command"].as_str().unwrap();
        assert!(cmd.contains("cargo build && cargo test"));
    }

    #[test]
    fn compile_block_step() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "approve", "block": "Deploy to prod?"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].executor, "approval");
    }

    #[test]
    fn compile_trigger_step() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "deploy", "trigger": {"pipeline": "deploy-prod"}}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].executor, "trigger");
    }

    #[test]
    fn compile_auto_id() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"command": "echo hello"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].id.0, "step-0");
    }

    #[test]
    fn compile_simple_matrix() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {
                        "key": "test",
                        "command": "cargo test",
                        "matrix": ["default", "serde"]
                    }
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
        assert!(ids.contains(&"test-default"));
        assert!(ids.contains(&"test-serde"));
    }

    #[test]
    fn compile_container_with_command() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "test", "container": {"image": "node:20"}, "command": "npm test"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].executor, "container");
    }

    #[test]
    fn v1_compat_fields() {
        // v1 used "tasks", "id", "retries", "timeout_secs" — all aliased in v2 schema.
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "tasks": [
                    {"id": "test", "command": "echo", "retries": 3, "timeout_secs": 60}
                ],
                "retries": 1,
                "timeout_secs": 300
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].retries, Some(3));
        assert_eq!(result.flow_def.tasks[0].timeout_secs, Some(60));
        assert_eq!(result.queue_config.max_retries, 1);
        assert_eq!(result.queue_config.timeout_secs, 300);
    }

    #[test]
    fn compile_runner_wraps_in_container() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "runner": "rust:latest",
                "steps": [
                    {"key": "test", "command": "cargo test"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        // Should compile to container executor, not shell.
        assert_eq!(result.flow_def.tasks[0].executor, "container");
        assert_eq!(result.flow_def.tasks[0].config["image"], "rust:latest");
    }

    #[test]
    fn compile_step_runner_overrides_pipeline() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "runner": "rust:latest",
                "steps": [
                    {"key": "test", "command": "cargo test"},
                    {"key": "lint", "command": "npm run lint", "runner": "node:20"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].config["image"], "rust:latest");
        assert_eq!(result.flow_def.tasks[1].config["image"], "node:20");
    }

    #[test]
    fn compile_host_runner_uses_shell() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "runner": "rust:latest",
                "steps": [
                    {"key": "test", "command": "cargo test"},
                    {"key": "local", "command": "echo hi", "runner": "host"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].executor, "container");
        assert_eq!(result.flow_def.tasks[1].executor, "shell");
    }

    #[test]
    fn compile_use_def() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "defs": {
                    "rust": {
                        "runner": "rust:latest",
                        "timeout": 600
                    }
                },
                "steps": [
                    {"key": "test", "use": "rust", "command": "cargo test"},
                    {"key": "check", "use": "rust", "command": "cargo check"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        assert_eq!(result.flow_def.tasks[0].executor, "container");
        assert_eq!(result.flow_def.tasks[0].config["image"], "rust:latest");
        assert_eq!(result.flow_def.tasks[0].timeout_secs, Some(600));
        assert_eq!(result.flow_def.tasks[1].executor, "container");
        assert_eq!(result.flow_def.tasks[1].timeout_secs, Some(600));
    }

    #[test]
    fn compile_def_env_merge() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "defs": {
                    "base": {
                        "env": {"FROM_DEF": "1", "SHARED": "def"}
                    }
                },
                "steps": [
                    {"key": "test", "use": "base", "command": "echo", "env": {"FROM_STEP": "2", "SHARED": "step"}}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        let cmd = result.flow_def.tasks[0].config["command"].as_str().unwrap();
        assert!(cmd.contains("FROM_DEF"));
        assert!(cmd.contains("FROM_STEP"));
        // Step value should override def value.
        assert!(cmd.contains("SHARED=\"step\""));
    }

    #[test]
    fn compile_def_condition_and() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "defs": {
                    "main_only": {
                        "if": "branch == 'main'"
                    }
                },
                "steps": [
                    {"key": "deploy", "use": "main_only", "command": "echo deploy", "if": "event == 'push'"}
                ]
            }"#,
        )
        .unwrap();
        // Check that the resolved condition has both parts.
        let resolved = resolve_defs(&pipeline).unwrap();
        let cond = resolved.steps[0].condition.as_deref().unwrap();
        assert!(cond.contains("branch == 'main'"));
        assert!(cond.contains("event == 'push'"));
    }

    #[test]
    fn compile_def_step_overrides() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "defs": {
                    "base": {
                        "timeout": 600,
                        "retry": 3
                    }
                },
                "steps": [
                    {"key": "test", "use": "base", "command": "echo", "timeout": 30}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        // Step timeout overrides def.
        assert_eq!(result.flow_def.tasks[0].timeout_secs, Some(30));
        // Retry inherited from def.
        assert_eq!(result.flow_def.tasks[0].retries, Some(3));
    }

    #[test]
    fn compile_unknown_def_error() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "steps": [
                    {"key": "test", "use": "nonexistent", "command": "echo"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let err = compile(&pipeline, &ctx).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn compile_setup_prepended() {
        let pipeline: Pipeline = serde_json::from_str(
            r#"{
                "checkout": false,
                "runner": {"image": "rust:latest", "setup": "rustup component add clippy"},
                "steps": [
                    {"key": "clippy", "command": "cargo clippy"}
                ]
            }"#,
        )
        .unwrap();
        let ctx = BuildContext::default();
        let result = compile(&pipeline, &ctx).unwrap();
        let cmd = result.flow_def.tasks[0].config["command"]
            .as_array()
            .unwrap();
        let shell_cmd = cmd[2].as_str().unwrap();
        assert!(shell_cmd.contains("rustup component add clippy"));
        assert!(shell_cmd.contains("cargo clippy"));
        // Setup should come before the main command.
        let setup_pos = shell_cmd.find("rustup component add clippy").unwrap();
        let cmd_pos = shell_cmd.find("cargo clippy").unwrap();
        assert!(setup_pos < cmd_pos);
    }
}

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level pipeline definition.
///
/// The JSON format is designed for LLM/agent generation — no YAML,
/// no indentation sensitivity, publishable as a JSON Schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    /// Trigger events (informational in Phase 1; used by webhook receiver later).
    #[serde(default)]
    pub on: Vec<Trigger>,

    /// Whether to inject a checkout step as the DAG root. Default: true.
    #[serde(default = "default_true")]
    pub checkout: bool,

    /// Checkout configuration.
    #[serde(default)]
    pub checkout_config: Option<CheckoutConfig>,

    /// Global environment variables merged into every task.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Secret references mapped to Tasked queue secrets.
    #[serde(default)]
    pub secrets: HashMap<String, SecretSource>,

    /// Global retry default (overridable per-task).
    #[serde(default)]
    pub retries: Option<u32>,

    /// Global timeout in seconds (overridable per-task).
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Pipeline tasks.
    pub tasks: Vec<PipelineTask>,
}

fn default_true() -> bool {
    true
}

/// Trigger event types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Push {
        #[serde(default)]
        branches: Option<Vec<String>>,
    },
    PullRequest {
        #[serde(default)]
        branches: Option<Vec<String>>,
    },
    Schedule {
        cron: String,
    },
    Manual,
}

/// Git checkout configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutConfig {
    /// Clone depth (default: 1 for shallow).
    #[serde(default = "default_depth")]
    pub depth: u32,

    /// Fetch submodules.
    #[serde(default)]
    pub submodules: bool,

    /// Fetch LFS objects.
    #[serde(default)]
    pub lfs: bool,
}

fn default_depth() -> u32 {
    1
}

impl Default for CheckoutConfig {
    fn default() -> Self {
        Self {
            depth: 1,
            submodules: false,
            lfs: false,
        }
    }
}

/// A task in the pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineTask {
    /// Unique task identifier.
    pub id: String,

    /// Shell command shorthand — sugar for executor "shell".
    #[serde(default)]
    pub command: Option<String>,

    /// Explicit executor name (for non-shell tasks).
    #[serde(default)]
    pub executor: Option<String>,

    /// Explicit executor config (for non-shell tasks).
    #[serde(default)]
    pub config: Option<serde_json::Value>,

    /// Container shorthand — sugar for executor "container".
    #[serde(default)]
    pub container: Option<ContainerConfig>,

    /// Task-level environment variables (merged on top of global).
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Task dependencies.
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// Conditional execution expression.
    #[serde(rename = "if", default)]
    pub condition: Option<String>,

    /// Matrix expansion configuration.
    #[serde(default)]
    pub matrix: Option<MatrixConfig>,

    /// Retry count (overrides global).
    #[serde(default)]
    pub retries: Option<u32>,

    /// Timeout in seconds (overrides global).
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Cache configuration.
    #[serde(default)]
    pub cache: Option<CacheConfig>,

    /// Artifact configuration.
    #[serde(default)]
    pub artifacts: Option<ArtifactConfig>,

    /// Enable dynamic pipeline generation (maps to spawn executor).
    #[serde(default)]
    pub spawn: bool,

    /// Spawn output signal IDs (for downstream deferred deps).
    #[serde(default)]
    pub spawn_output: Vec<String>,
}

/// Container shorthand configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    pub image: String,

    #[serde(default)]
    pub command: Option<Vec<String>>,

    #[serde(default)]
    pub env: HashMap<String, String>,

    #[serde(default)]
    pub working_dir: Option<String>,
}

/// Matrix build expansion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixConfig {
    /// Named dimensions. Each key maps to a list of values.
    /// e.g. {"toolchain": ["stable", "nightly"], "os": ["linux", "macos"]}
    pub dimensions: HashMap<String, Vec<String>>,

    /// Combinations to exclude from the cartesian product.
    #[serde(default)]
    pub exclude: Vec<HashMap<String, String>>,
}

/// Cache restore/save configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Cache key. Supports `${file.hash:path}` for file content hashing.
    pub key: String,

    /// Paths to cache.
    pub paths: Vec<String>,

    /// Fallback keys tried in order if exact key misses.
    #[serde(default)]
    pub restore_keys: Vec<String>,
}

/// Artifact upload/download configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactConfig {
    /// Glob patterns of paths to upload after task succeeds.
    #[serde(default)]
    pub upload: Vec<String>,

    /// Task IDs whose artifacts to download before this task runs.
    #[serde(default)]
    pub download_from: Vec<String>,
}

/// Secret source configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretSource {
    /// Environment variable name to read from.
    #[serde(default)]
    pub env: Option<String>,

    /// File path to read from.
    #[serde(default)]
    pub file: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_pipeline() {
        let json = r#"{
            "tasks": [
                {"id": "test", "command": "cargo test"}
            ]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert_eq!(pipeline.tasks.len(), 1);
        assert_eq!(pipeline.tasks[0].id, "test");
        assert_eq!(pipeline.tasks[0].command.as_deref(), Some("cargo test"));
        assert!(pipeline.checkout); // default true
    }

    #[test]
    fn parse_full_pipeline() {
        let json = r#"{
            "on": [{"push": {"branches": ["main"]}}, "manual"],
            "checkout": true,
            "env": {"RUST_BACKTRACE": "1"},
            "secrets": {"TOKEN": {"env": "MY_TOKEN"}},
            "timeout_secs": 600,
            "retries": 1,
            "tasks": [
                {"id": "lint", "command": "cargo clippy"},
                {
                    "id": "test",
                    "command": "cargo test",
                    "depends_on": ["lint"],
                    "matrix": {
                        "dimensions": {"toolchain": ["stable", "nightly"]}
                    },
                    "cache": {
                        "key": "cargo-${matrix.toolchain}",
                        "paths": ["target/"]
                    }
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
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert_eq!(pipeline.tasks.len(), 4);
        assert_eq!(pipeline.env.get("RUST_BACKTRACE").unwrap(), "1");
        assert!(pipeline.tasks[1].matrix.is_some());
        assert!(pipeline.tasks[2].artifacts.is_some());
        assert_eq!(
            pipeline.tasks[3].condition.as_deref(),
            Some("branch == 'main'")
        );
    }

    #[test]
    fn parse_container_task() {
        let json = r#"{
            "tasks": [{
                "id": "docker-build",
                "container": {
                    "image": "docker:24-dind",
                    "command": ["docker", "build", "."]
                },
                "depends_on": ["build"]
            }]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        let task = &pipeline.tasks[0];
        assert!(task.container.is_some());
        assert_eq!(task.container.as_ref().unwrap().image, "docker:24-dind");
    }

    #[test]
    fn parse_spawn_task() {
        let json = r#"{
            "tasks": [
                {
                    "id": "discover",
                    "command": "./find-services.sh",
                    "spawn": true,
                    "spawn_output": ["complete"]
                },
                {
                    "id": "deploy-all",
                    "command": "echo done",
                    "depends_on": ["discover/complete"]
                }
            ]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert!(pipeline.tasks[0].spawn);
        assert_eq!(pipeline.tasks[0].spawn_output, vec!["complete"]);
    }
}

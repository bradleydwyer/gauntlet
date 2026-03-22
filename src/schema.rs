use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level pipeline definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    /// Pipeline steps.
    #[serde(alias = "tasks")]
    pub steps: Vec<Step>,

    /// Global environment variables merged into every step.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Git checkout configuration. `true` = shallow clone (default), `false` = skip.
    #[serde(default = "default_checkout")]
    pub checkout: CheckoutSetting,

    /// Trigger events (informational until webhook receiver is active).
    #[serde(default)]
    pub on: Vec<Trigger>,

    /// Secret references (env var or file).
    #[serde(default)]
    pub secrets: HashMap<String, SecretSource>,

    /// Global retry default (overridable per-step).
    #[serde(alias = "retries", default)]
    pub retry: Option<u32>,

    /// Global timeout in seconds (overridable per-step).
    #[serde(alias = "timeout_secs", default)]
    pub timeout: Option<u64>,

    /// Default runner for all steps. Steps without their own `runner` inherit this.
    #[serde(default)]
    pub runner: Option<RunnerConfig>,
}

fn default_checkout() -> CheckoutSetting {
    CheckoutSetting::Enabled(true)
}

/// Checkout can be a bool or a config object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CheckoutSetting {
    Enabled(bool),
    Config(CheckoutConfig),
}

impl CheckoutSetting {
    pub fn is_enabled(&self) -> bool {
        match self {
            Self::Enabled(b) => *b,
            Self::Config(_) => true,
        }
    }

    pub fn config(&self) -> CheckoutConfig {
        match self {
            Self::Enabled(true) => CheckoutConfig::default(),
            Self::Enabled(false) => CheckoutConfig::default(), // won't be used
            Self::Config(c) => c.clone(),
        }
    }
}

/// Git checkout configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutConfig {
    #[serde(default = "default_depth")]
    pub depth: u32,
    #[serde(default)]
    pub submodules: bool,
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

/// A step in the pipeline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Step {
    /// Unique identifier for `depends_on` references.
    #[serde(alias = "id", default)]
    pub key: Option<String>,

    /// Display name.
    #[serde(default)]
    pub label: Option<String>,

    // ── Step type (exactly one of these) ──
    /// Shell command shorthand.
    #[serde(default)]
    pub command: Option<String>,

    /// Multiple shell commands (joined with &&).
    #[serde(default)]
    pub commands: Option<Vec<String>>,

    /// Container configuration (runs command inside Docker).
    #[serde(default)]
    pub container: Option<ContainerConfig>,

    /// Approval gate message.
    #[serde(default)]
    pub block: Option<String>,

    /// Trigger a sub-pipeline.
    #[serde(default)]
    pub trigger: Option<TriggerConfig>,

    /// Raw tasked executor name (escape hatch).
    #[serde(default)]
    pub executor: Option<String>,

    /// Raw tasked executor config (used with `executor`).
    #[serde(default)]
    pub config: Option<serde_json::Value>,

    // ── Common fields ──
    /// Step dependencies.
    #[serde(default)]
    pub depends_on: DependsOn,

    /// Condition expression — step skipped if false.
    #[serde(rename = "if", default)]
    pub condition: Option<String>,

    /// Step-level environment variables.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Timeout in seconds.
    #[serde(alias = "timeout_secs", default)]
    pub timeout: Option<u64>,

    /// Auto-retry count.
    #[serde(alias = "retries", default)]
    pub retry: Option<u32>,

    /// Failure doesn't fail the pipeline.
    #[serde(default)]
    pub soft_fail: bool,

    /// Runner for this step (overrides pipeline-level default).
    #[serde(default)]
    pub runner: Option<RunnerConfig>,

    /// Matrix expansion.
    #[serde(default)]
    pub matrix: Option<MatrixSetting>,

    /// Artifact glob patterns to upload after step succeeds.
    #[serde(default)]
    pub artifacts: Option<ArtifactSetting>,

    /// Cache configuration.
    #[serde(default)]
    pub cache: Option<CacheConfig>,

    /// Enable dynamic pipeline generation (spawn executor).
    #[serde(default)]
    pub spawn: bool,

    /// Spawn output signal IDs (for downstream deferred deps).
    #[serde(default)]
    pub spawn_output: Vec<String>,
}

/// `depends_on` can be a single string or an array.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DependsOn {
    #[default]
    None,
    Single(String),
    Multiple(Vec<String>),
}

impl DependsOn {
    pub fn as_vec(&self) -> Vec<String> {
        match self {
            Self::None => vec![],
            Self::Single(s) => vec![s.clone()],
            Self::Multiple(v) => v.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::Single(_) => false,
            Self::Multiple(v) => v.is_empty(),
        }
    }
}

/// Matrix can be a simple string array or a multi-dimension config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MatrixSetting {
    /// Simple single-dimension: `["a", "b", "c"]`
    Simple(Vec<String>),
    /// Multi-dimension with named dimensions and optional exclusions.
    Multi(MatrixConfig),
}

/// Multi-dimension matrix configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixConfig {
    pub dimensions: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub exclude: Vec<HashMap<String, String>>,
}

/// Artifacts can be a simple glob array or a full config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArtifactSetting {
    /// Simple: `["target/release/myapp", "dist/**"]`
    Globs(Vec<String>),
    /// Full config with upload and download.
    Full(ArtifactConfig),
}

/// Full artifact configuration (v1 compat).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactConfig {
    #[serde(default)]
    pub upload: Vec<String>,
    #[serde(default)]
    pub download_from: Vec<String>,
}

/// Runner configuration — determines where a step executes.
///
/// Can be a simple string (Docker image name) or an object for more control.
/// - `"runner": "rust:latest"` — Docker container with this image
/// - `"runner": {"image": "rust:latest"}` — same, explicit form
/// - `"runner": {"type": "tart", "vm": "sonoma-base"}` — Tart VM
/// - `"runner": "host"` — run directly on the host (no isolation)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RunnerConfig {
    /// Short form: just a Docker image name, or "host" for no container.
    Image(String),
    /// Full form with explicit type and options.
    Full(RunnerSpec),
}

/// Full runner specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerSpec {
    /// Runner type: "docker" (default), "tart", "host".
    #[serde(rename = "type", default = "default_runner_type")]
    pub runner_type: String,
    /// Docker image (for docker runner).
    #[serde(default)]
    pub image: Option<String>,
    /// Tart VM name (for tart runner).
    #[serde(default)]
    pub vm: Option<String>,
}

fn default_runner_type() -> String {
    "docker".to_string()
}

impl RunnerConfig {
    /// Get the effective Docker image, if this is a Docker runner.
    pub fn docker_image(&self) -> Option<&str> {
        match self {
            Self::Image(s) if s != "host" => Some(s),
            Self::Full(spec) if spec.runner_type == "docker" => spec.image.as_deref(),
            _ => None,
        }
    }

    /// Is this a host runner (no container)?
    pub fn is_host(&self) -> bool {
        match self {
            Self::Image(s) => s == "host",
            Self::Full(spec) => spec.runner_type == "host",
        }
    }

    /// Is this a Tart VM runner?
    pub fn is_tart(&self) -> bool {
        match self {
            Self::Image(_) => false,
            Self::Full(spec) => spec.runner_type == "tart",
        }
    }

    /// Get the Tart VM name, if this is a Tart runner.
    pub fn tart_vm(&self) -> Option<&str> {
        match self {
            Self::Full(spec) if spec.runner_type == "tart" => spec.vm.as_deref(),
            _ => None,
        }
    }
}

/// Container step configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    pub image: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub working_dir: Option<String>,
}

/// Trigger step configuration (sub-pipeline).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerConfig {
    pub pipeline: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Cache restore/save configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    pub key: String,
    pub paths: Vec<String>,
    #[serde(default)]
    pub restore_keys: Vec<String>,
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

/// Secret source configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretSource {
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_v2() {
        let json = r#"{
            "steps": [
                {"command": "cargo test"}
            ]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert_eq!(pipeline.steps.len(), 1);
        assert_eq!(pipeline.steps[0].command.as_deref(), Some("cargo test"));
        assert!(pipeline.checkout.is_enabled());
    }

    #[test]
    fn parse_v1_compat() {
        let json = r#"{
            "tasks": [
                {"id": "test", "command": "cargo test", "timeout_secs": 300, "retries": 2}
            ],
            "timeout_secs": 600,
            "retries": 1
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert_eq!(pipeline.steps.len(), 1);
        assert_eq!(pipeline.steps[0].key.as_deref(), Some("test"));
        assert_eq!(pipeline.steps[0].timeout, Some(300));
        assert_eq!(pipeline.steps[0].retry, Some(2));
        assert_eq!(pipeline.timeout, Some(600));
        assert_eq!(pipeline.retry, Some(1));
    }

    #[test]
    fn parse_full_v2() {
        let json = r#"{
            "env": {"RUST_BACKTRACE": "1"},
            "checkout": {"depth": 1, "submodules": true},
            "steps": [
                {"key": "lint", "command": "cargo clippy -- -D warnings"},
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
                    "commands": ["docker build -t myapp .", "docker push myapp"],
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
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert_eq!(pipeline.steps.len(), 6);
        assert_eq!(pipeline.env.get("RUST_BACKTRACE").unwrap(), "1");

        // Checkout config
        let checkout = pipeline.checkout.config();
        assert!(checkout.submodules);

        // Matrix (simple)
        match pipeline.steps[1].matrix.as_ref().unwrap() {
            MatrixSetting::Simple(v) => assert_eq!(v.len(), 3),
            _ => panic!("expected simple matrix"),
        }

        // Artifacts (simple globs)
        match pipeline.steps[2].artifacts.as_ref().unwrap() {
            ArtifactSetting::Globs(v) => assert_eq!(v, &["target/release/myapp"]),
            _ => panic!("expected glob artifacts"),
        }

        // Multi-command
        assert_eq!(pipeline.steps[3].commands.as_ref().unwrap().len(), 2);

        // Block
        assert_eq!(
            pipeline.steps[4].block.as_deref(),
            Some("Deploy to production?")
        );

        // Condition
        assert_eq!(
            pipeline.steps[5].condition.as_deref(),
            None // deploy has no condition
        );
        assert_eq!(
            pipeline.steps[4].condition.as_deref(),
            Some("branch == 'main'")
        );
    }

    #[test]
    fn parse_depends_on_single() {
        let json = r#"{
            "steps": [
                {"key": "a", "command": "echo a"},
                {"key": "b", "command": "echo b", "depends_on": "a"}
            ]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert_eq!(pipeline.steps[1].depends_on.as_vec(), vec!["a"]);
    }

    #[test]
    fn parse_depends_on_array() {
        let json = r#"{
            "steps": [
                {"key": "a", "command": "echo a"},
                {"key": "b", "command": "echo b"},
                {"key": "c", "command": "echo c", "depends_on": ["a", "b"]}
            ]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert_eq!(pipeline.steps[2].depends_on.as_vec(), vec!["a", "b"]);
    }

    #[test]
    fn parse_container_with_command() {
        let json = r#"{
            "steps": [{
                "key": "test",
                "container": {"image": "node:20"},
                "command": "npm test"
            }]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        let step = &pipeline.steps[0];
        assert!(step.container.is_some());
        assert_eq!(step.container.as_ref().unwrap().image, "node:20");
        assert_eq!(step.command.as_deref(), Some("npm test"));
    }

    #[test]
    fn parse_executor_escape_hatch() {
        let json = r#"{
            "steps": [{
                "key": "notify",
                "executor": "slack",
                "config": {
                    "operation": "post_message",
                    "channel": "builds",
                    "text": "Build done"
                },
                "depends_on": ["build"]
            }]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        let step = &pipeline.steps[0];
        assert_eq!(step.executor.as_deref(), Some("slack"));
        assert!(step.config.is_some());
    }

    #[test]
    fn parse_multi_dimension_matrix() {
        let json = r#"{
            "steps": [{
                "key": "test",
                "command": "cargo test",
                "matrix": {
                    "dimensions": {
                        "toolchain": ["stable", "nightly"],
                        "target": ["x86_64", "aarch64"]
                    },
                    "exclude": [{"toolchain": "nightly", "target": "aarch64"}]
                }
            }]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        match pipeline.steps[0].matrix.as_ref().unwrap() {
            MatrixSetting::Multi(m) => {
                assert_eq!(m.dimensions.len(), 2);
                assert_eq!(m.exclude.len(), 1);
            }
            _ => panic!("expected multi matrix"),
        }
    }

    #[test]
    fn parse_trigger_step() {
        let json = r#"{
            "steps": [{
                "key": "deploy",
                "trigger": {
                    "pipeline": "deploy",
                    "env": {"TARGET": "staging"}
                }
            }]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        let step = &pipeline.steps[0];
        let trigger = step.trigger.as_ref().unwrap();
        assert_eq!(trigger.pipeline, "deploy");
        assert_eq!(trigger.env.get("TARGET").unwrap(), "staging");
    }

    #[test]
    fn parse_spawn_step() {
        let json = r#"{
            "steps": [
                {
                    "key": "discover",
                    "command": "./find-services.sh",
                    "spawn": true,
                    "spawn_output": ["done"]
                },
                {
                    "key": "deploy-all",
                    "command": "echo done",
                    "depends_on": ["discover/done"]
                }
            ]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert!(pipeline.steps[0].spawn);
        assert_eq!(pipeline.steps[0].spawn_output, vec!["done"]);
    }

    #[test]
    fn parse_soft_fail() {
        let json = r#"{
            "steps": [
                {"key": "lint", "command": "cargo clippy", "soft_fail": true}
            ]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert!(pipeline.steps[0].soft_fail);
    }

    #[test]
    fn parse_checkout_bool() {
        let json = r#"{"steps": [], "checkout": false}"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert!(!pipeline.checkout.is_enabled());
    }

    #[test]
    fn parse_checkout_config() {
        let json = r#"{"steps": [], "checkout": {"depth": 10, "lfs": true}}"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        assert!(pipeline.checkout.is_enabled());
        let cfg = pipeline.checkout.config();
        assert_eq!(cfg.depth, 10);
        assert!(cfg.lfs);
    }

    #[test]
    fn parse_artifact_full_config() {
        let json = r#"{
            "steps": [{
                "key": "build",
                "command": "make",
                "artifacts": {"upload": ["dist/*"], "download_from": ["prepare"]}
            }]
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).unwrap();
        match pipeline.steps[0].artifacts.as_ref().unwrap() {
            ArtifactSetting::Full(cfg) => {
                assert_eq!(cfg.upload, vec!["dist/*"]);
                assert_eq!(cfg.download_from, vec!["prepare"]);
            }
            _ => panic!("expected full artifact config"),
        }
    }
}

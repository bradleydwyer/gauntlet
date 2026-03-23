//! Configuration file support for gauntlet serve.
//!
//! Reads `~/.gauntlet/config.json` (or a custom path) to provide defaults
//! for CLI arguments. CLI flags always override config file values.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use tracing::debug;

/// Configuration from `~/.gauntlet/config.json`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub github_app_id: Option<u64>,
    #[serde(default)]
    pub github_private_key: Option<String>,
    #[serde(default)]
    pub webhook_secret: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default)]
    pub poll_interval_secs: Option<u64>,
    #[serde(default)]
    pub concurrency: Option<usize>,
    /// Secrets injected as env vars into builds.
    /// Keys are repo full names ("owner/repo") or "*" for global.
    #[serde(default)]
    pub secrets: HashMap<String, HashMap<String, String>>,
}

impl Config {
    /// Get secrets for a specific repo. Global ("*") secrets are the base,
    /// repo-specific secrets override.
    pub fn secrets_for_repo(&self, repo: &str) -> HashMap<String, String> {
        let mut merged = HashMap::new();
        if let Some(global) = self.secrets.get("*") {
            merged.extend(global.clone());
        }
        if let Some(repo_secrets) = self.secrets.get(repo) {
            merged.extend(repo_secrets.clone());
        }
        merged
    }

    /// Load config from the default path (~/.gauntlet/config.json).
    pub fn load_default() -> Self {
        let path = dirs::home_dir()
            .unwrap_or_default()
            .join(".gauntlet/config.json");
        Self::load_from(&path)
    }

    /// Load config from a specific path. Returns default if file doesn't exist.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(config) => {
                    debug!(path = %path.display(), "loaded config");
                    config
                }
                Err(e) => {
                    eprintln!("warning: failed to parse {}: {e}", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(), // File doesn't exist, use defaults.
        }
    }
}

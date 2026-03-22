use tasked::types::{TaskDef, TaskId};

/// Generate a cache restore TaskDef.
///
/// Phase 1: local filesystem cache at `~/.gauntlet/cache/`.
pub fn restore_task(task_id: &str, cache_key: &str, paths: &[String]) -> TaskDef {
    let restore_id = format!("{task_id}__cache_restore");
    let cache_dir = format!("${{HOME}}/.gauntlet/cache/{cache_key}");

    let mut commands = vec!["set -euo pipefail".to_string()];
    commands.push(format!("if [ -d \"{cache_dir}\" ]; then"));
    for path in paths {
        let expanded = expand_tilde(path);
        commands.push(format!("  mkdir -p \"$(dirname \"{expanded}\")\""));
        commands.push(format!(
            "  if [ -e \"{cache_dir}/{path_base}\" ]; then cp -a \"{cache_dir}/{path_base}\" \"{expanded}\"; fi",
            path_base = path_basename(path),
        ));
    }
    commands.push(format!("  echo \"cache restored from {cache_key}\""));
    commands.push("else".to_string());
    commands.push("  echo \"cache miss\"".to_string());
    commands.push("fi".to_string());

    TaskDef {
        id: TaskId(restore_id),
        executor: "shell".into(),
        config: serde_json::json!({ "command": commands.join("\n") }),
        input: None,
        depends_on: vec![],
        timeout_secs: Some(60),
        retries: Some(0),
        backoff: None,
        condition: None,
        spawn_output: vec![],
    }
}

/// Generate a cache save TaskDef.
pub fn save_task(task_id: &str, cache_key: &str, paths: &[String]) -> TaskDef {
    let save_id = format!("{task_id}__cache_save");
    let cache_dir = format!("${{HOME}}/.gauntlet/cache/{cache_key}");

    let mut commands = vec!["set -euo pipefail".to_string()];
    commands.push(format!("mkdir -p \"{cache_dir}\""));
    for path in paths {
        let expanded = expand_tilde(path);
        commands.push(format!(
            "if [ -e \"{expanded}\" ]; then cp -a \"{expanded}\" \"{cache_dir}/{path_base}\"; fi",
            path_base = path_basename(path),
        ));
    }
    commands.push(format!("echo \"cache saved to {cache_key}\""));

    TaskDef {
        id: TaskId(save_id),
        executor: "shell".into(),
        config: serde_json::json!({ "command": commands.join("\n") }),
        input: None,
        depends_on: vec![],
        timeout_secs: Some(60),
        retries: Some(0),
        backoff: None,
        condition: None,
        spawn_output: vec![],
    }
}

/// Expand `~/` to `$HOME/` for shell commands.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        format!("${{HOME}}/{rest}")
    } else {
        path.to_string()
    }
}

/// Get the last component of a path for use as cache entry name.
fn path_basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_task_has_correct_id() {
        let task = restore_task("build", "cargo-stable", &["target/".into()]);
        assert_eq!(task.id.0, "build__cache_restore");
        assert_eq!(task.executor, "shell");
    }

    #[test]
    fn save_task_has_correct_id() {
        let task = save_task("build", "cargo-stable", &["target/".into()]);
        assert_eq!(task.id.0, "build__cache_save");
    }

    #[test]
    fn tilde_expansion() {
        assert_eq!(expand_tilde("~/foo"), "${HOME}/foo");
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
    }

    #[test]
    fn basename() {
        assert_eq!(path_basename("target/"), "target");
        assert_eq!(path_basename("~/.cargo/registry/"), "registry");
        assert_eq!(path_basename("file.txt"), "file.txt");
    }
}

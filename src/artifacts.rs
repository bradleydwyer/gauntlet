use tasked::types::{TaskDef, TaskId};

/// Generate an artifact upload TaskDef.
///
/// Phase 1: local filesystem at `~/.gauntlet/artifacts/{flow_id}/{task_id}/`.
pub fn upload_task(task_id: &str, patterns: &[String]) -> TaskDef {
    let upload_id = format!("{task_id}__artifact_upload");
    let dest = format!("${{GAUNTLET_ARTIFACTS_DIR:-${{HOME}}/.gauntlet/artifacts}}/${{GAUNTLET_FLOW_ID}}/{task_id}");

    let mut commands = vec!["set -euo pipefail".to_string()];
    commands.push(format!("mkdir -p \"{dest}\""));
    for pattern in patterns {
        // Use shell glob expansion — cp will fail gracefully if no match.
        commands.push(format!("cp -a {pattern} \"{dest}/\" 2>/dev/null || true"));
    }
    commands.push(format!("echo \"artifacts uploaded to {dest}\""));

    TaskDef {
        id: TaskId(upload_id),
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

/// Generate an artifact download TaskDef.
pub fn download_task(task_id: &str, source_task_ids: &[String]) -> TaskDef {
    let download_id = format!("{task_id}__artifact_download");
    let base = "${GAUNTLET_ARTIFACTS_DIR:-${HOME}/.gauntlet/artifacts}/${GAUNTLET_FLOW_ID}";

    let mut commands = vec!["set -euo pipefail".to_string()];
    for source in source_task_ids {
        commands.push(format!(
            "if [ -d \"{base}/{source}\" ]; then cp -a \"{base}/{source}/\"* . 2>/dev/null || true; fi"
        ));
    }
    commands.push("echo \"artifacts downloaded\"".to_string());

    TaskDef {
        id: TaskId(download_id),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_task_id() {
        let task = upload_task("build", &["target/release/myapp".into()]);
        assert_eq!(task.id.0, "build__artifact_upload");
    }

    #[test]
    fn download_task_id() {
        let task = download_task("deploy", &["build".into()]);
        assert_eq!(task.id.0, "deploy__artifact_download");
    }
}

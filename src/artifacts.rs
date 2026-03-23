use tasked::types::{TaskDef, TaskId};

/// Generate an artifact upload TaskDef.
///
/// Copies specified paths from the step's workspace to a shared artifacts area.
/// The artifacts area path is provided via GAUNTLET_ARTIFACTS_DIR env var.
pub fn upload_task(task_id: &str, patterns: &[String], artifacts_dir: &str) -> TaskDef {
    let upload_id = format!("{task_id}__artifact_upload");
    let dest = format!("{artifacts_dir}/{task_id}");

    let mut commands = vec!["set -eu".to_string()];
    commands.push(format!("mkdir -p \"{dest}\""));
    for pattern in patterns {
        // Use cp -r for directories, -a for files. Handle globs.
        commands.push(format!(
            "for f in {pattern}; do [ -e \"$f\" ] && cp -a \"$f\" \"{dest}/\" || true; done"
        ));
    }

    TaskDef {
        id: TaskId(upload_id),
        executor: "shell".into(),
        config: serde_json::json!({ "command": commands.join("\n") }),
        input: None,
        depends_on: vec![],
        timeout_secs: Some(120),
        retries: Some(0),
        backoff: None,
        condition: None,
        spawn_output: vec![],
    }
}

/// Generate an artifact download TaskDef.
///
/// Copies artifacts from source steps into the current step's workspace.
pub fn download_task(task_id: &str, source_task_ids: &[String], artifacts_dir: &str) -> TaskDef {
    let download_id = format!("{task_id}__artifact_download");

    let mut commands = vec!["set -eu".to_string()];
    for source in source_task_ids {
        let src = format!("{artifacts_dir}/{source}");
        commands.push(format!(
            "if [ -d \"{src}\" ]; then cp -a \"{src}/\"* . 2>/dev/null || true; fi"
        ));
    }

    TaskDef {
        id: TaskId(download_id),
        executor: "shell".into(),
        config: serde_json::json!({ "command": commands.join("\n") }),
        input: None,
        depends_on: vec![],
        timeout_secs: Some(120),
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
        let task = upload_task("build", &["target/release/myapp".into()], "/tmp/artifacts");
        assert_eq!(task.id.0, "build__artifact_upload");
    }

    #[test]
    fn download_task_id() {
        let task = download_task("deploy", &["build".into()], "/tmp/artifacts");
        assert_eq!(task.id.0, "deploy__artifact_download");
    }
}

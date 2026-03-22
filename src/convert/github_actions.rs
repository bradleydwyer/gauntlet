use super::{ConvertError, Converter};
use crate::schema::{Pipeline, PipelineTask, Trigger};

/// Converts GitHub Actions workflow YAML to Gauntlet Pipeline JSON.
///
/// This is a best-effort structural mapper for one-time migration.
/// Complex features (reusable workflows, composite actions, expressions)
/// produce TODO comments in the output.
pub struct GitHubActionsConverter;

impl Converter for GitHubActionsConverter {
    fn convert(&self, source: &str) -> Result<Pipeline, ConvertError> {
        // Phase 1: basic structural conversion.
        // Parse the YAML, extract jobs, map to tasks.
        let yaml: serde_json::Value = serde_yaml_to_json(source)?;

        let jobs = yaml
            .get("jobs")
            .and_then(|j| j.as_object())
            .ok_or_else(|| ConvertError::Parse("no 'jobs' key found".into()))?;

        let mut tasks = Vec::new();

        for (job_id, job) in jobs {
            let steps = job
                .get("steps")
                .and_then(|s| s.as_array())
                .unwrap_or(&Vec::new())
                .clone();

            // Collect run commands from steps, skip `uses` steps with a TODO.
            let mut commands = Vec::new();
            for step in &steps {
                if let Some(run) = step.get("run").and_then(|r| r.as_str()) {
                    commands.push(run.to_string());
                } else if let Some(uses) = step.get("uses").and_then(|u| u.as_str()) {
                    commands.push(format!("# TODO: convert action '{uses}'"));
                }
            }

            let combined_command = if commands.is_empty() {
                "echo 'no steps converted'".to_string()
            } else {
                commands.join("\n")
            };

            // Map `needs` to `depends_on`.
            let depends_on = match job.get("needs") {
                Some(serde_json::Value::String(s)) => vec![s.clone()],
                Some(serde_json::Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
                _ => vec![],
            };

            // Map `if` condition.
            let condition = job
                .get("if")
                .and_then(|v| v.as_str())
                .map(|s| format!("# TODO: convert expression: {s}"));

            tasks.push(PipelineTask {
                id: job_id.clone(),
                command: Some(combined_command),
                executor: None,
                config: None,
                container: None,
                env: Default::default(),
                depends_on,
                condition,
                matrix: None, // TODO: convert strategy.matrix
                retries: None,
                timeout_secs: None,
                cache: None,
                artifacts: None,
                spawn: false,
                spawn_output: vec![],
            });
        }

        // Convert triggers.
        let triggers = convert_triggers(&yaml);

        Ok(Pipeline {
            on: triggers,
            checkout: true,
            checkout_config: None,
            env: Default::default(),
            secrets: Default::default(),
            retries: None,
            timeout_secs: None,
            tasks,
        })
    }
}

fn convert_triggers(yaml: &serde_json::Value) -> Vec<Trigger> {
    let mut triggers = Vec::new();

    match yaml.get("on") {
        Some(serde_json::Value::Array(arr)) => {
            for item in arr {
                if let Some(s) = item.as_str() {
                    match s {
                        "push" => triggers.push(Trigger::Push { branches: None }),
                        "pull_request" => {
                            triggers.push(Trigger::PullRequest { branches: None })
                        }
                        _ => {}
                    }
                }
            }
        }
        Some(serde_json::Value::Object(obj)) => {
            if obj.contains_key("push") {
                triggers.push(Trigger::Push { branches: None });
            }
            if obj.contains_key("pull_request") {
                triggers.push(Trigger::PullRequest { branches: None });
            }
        }
        Some(serde_json::Value::String(s)) => match s.as_str() {
            "push" => triggers.push(Trigger::Push { branches: None }),
            "pull_request" => triggers.push(Trigger::PullRequest { branches: None }),
            _ => {}
        },
        _ => {}
    }

    triggers
}

/// Minimal YAML to JSON conversion using serde.
/// In Phase 1, we use serde_json::Value as the intermediate representation
/// and do manual parsing rather than adding a YAML dependency.
fn serde_yaml_to_json(_yaml_str: &str) -> Result<serde_json::Value, ConvertError> {
    // For Phase 1, we parse a simplified subset.
    // A proper implementation would use the `serde_yaml` crate.
    // For now, return an error suggesting the user install serde_yaml.
    Err(ConvertError::Unsupported(
        "YAML parsing not yet implemented — install Phase 1.5 for full GHA conversion. \
         For now, manually convert your workflow to Gauntlet JSON format."
            .into(),
    ))
}

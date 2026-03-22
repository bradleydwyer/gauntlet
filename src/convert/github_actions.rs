use super::{ConvertError, Converter};
use crate::schema::{CheckoutSetting, DependsOn, Pipeline, Step, Trigger};

/// Converts GitHub Actions workflow YAML to Gauntlet Pipeline JSON.
///
/// This is a best-effort structural mapper for one-time migration.
/// Complex features (reusable workflows, composite actions, expressions)
/// produce TODO comments in the output.
pub struct GitHubActionsConverter;

impl Converter for GitHubActionsConverter {
    fn convert(&self, source: &str) -> Result<Pipeline, ConvertError> {
        let yaml: serde_json::Value = serde_yaml_to_json(source)?;

        let jobs = yaml
            .get("jobs")
            .and_then(|j| j.as_object())
            .ok_or_else(|| ConvertError::Parse("no 'jobs' key found".into()))?;

        let mut steps = Vec::new();

        for (job_id, job) in jobs {
            let job_steps = job
                .get("steps")
                .and_then(|s| s.as_array())
                .unwrap_or(&Vec::new())
                .clone();

            let mut commands = Vec::new();
            for step in &job_steps {
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

            let depends_on = match job.get("needs") {
                Some(serde_json::Value::String(s)) => DependsOn::Single(s.clone()),
                Some(serde_json::Value::Array(arr)) => DependsOn::Multiple(
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect(),
                ),
                _ => DependsOn::None,
            };

            let condition = job
                .get("if")
                .and_then(|v| v.as_str())
                .map(|s| format!("# TODO: convert expression: {s}"));

            steps.push(Step {
                key: Some(job_id.clone()),
                command: Some(combined_command),
                depends_on,
                condition,
                ..Default::default()
            });
        }

        let triggers = convert_triggers(&yaml);

        Ok(Pipeline {
            steps,
            on: triggers,
            checkout: CheckoutSetting::Enabled(true),
            env: Default::default(),
            secrets: Default::default(),
            retry: None,
            timeout: None,
            runner: None,
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
                        "pull_request" => triggers.push(Trigger::PullRequest { branches: None }),
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

fn serde_yaml_to_json(_yaml_str: &str) -> Result<serde_json::Value, ConvertError> {
    Err(ConvertError::Unsupported(
        "YAML parsing not yet implemented. Manually convert your workflow to Gauntlet JSON format."
            .into(),
    ))
}

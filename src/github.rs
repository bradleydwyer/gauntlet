use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("GitHub API error: {status} {body}")]
    Api { status: u16, body: String },
}

// ── Commit Status API ──

#[derive(Debug, Clone, Copy)]
pub enum CommitState {
    Pending,
    Success,
    Failure,
    Error,
}

impl CommitState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Error => "error",
        }
    }
}

// ── Checks API ──

/// Status of a check run.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
}

/// Conclusion of a completed check run.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Cancelled,
    TimedOut,
    ActionRequired,
    Skipped,
}

/// Output summary attached to a check run.
#[derive(Debug, Clone, Serialize)]
pub struct CheckOutput {
    pub title: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<CheckAnnotation>,
}

/// Annotation on a specific file/line in a check run.
#[derive(Debug, Clone, Serialize)]
pub struct CheckAnnotation {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub annotation_level: AnnotationLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationLevel {
    Notice,
    Warning,
    Failure,
}

/// Response from creating a check run.
#[derive(Debug, Deserialize)]
pub struct CheckRun {
    pub id: u64,
}

pub struct GitHubClient {
    token: String,
    api_base: String,
    client: Client,
}

impl GitHubClient {
    pub fn new(token: String) -> Self {
        Self {
            token,
            api_base: "https://api.github.com".to_string(),
            client: Client::new(),
        }
    }

    /// Override the API base URL (for testing).
    #[cfg(test)]
    fn with_api_base(mut self, base: String) -> Self {
        self.api_base = base;
        self
    }

    /// Send a POST/PATCH request to the GitHub API.
    async fn api_request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, GitHubError> {
        let resp = self
            .client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "gauntlet-ci")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(body)
            .send()
            .await?;

        if resp.status().is_success() {
            Ok(resp)
        } else {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Err(GitHubError::Api { status, body })
        }
    }

    // ── Commit Status API ──

    /// Report a commit status to GitHub.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_commit_status(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
        context: &str,
        state: CommitState,
        description: &str,
        target_url: Option<&str>,
    ) -> Result<(), GitHubError> {
        let url = format!("{}/repos/{owner}/{repo}/statuses/{sha}", self.api_base);

        let mut body = serde_json::json!({
            "state": state.as_str(),
            "context": context,
            "description": description,
        });

        if let Some(url) = target_url {
            body["target_url"] = serde_json::Value::String(url.to_string());
        }

        self.api_request(reqwest::Method::POST, &url, &body).await?;
        Ok(())
    }

    // ── Checks API ──

    /// Create a new check run on a commit.
    pub async fn create_check_run(
        &self,
        owner: &str,
        repo: &str,
        name: &str,
        head_sha: &str,
        status: CheckStatus,
        details_url: Option<&str>,
    ) -> Result<CheckRun, GitHubError> {
        let url = format!("{}/repos/{owner}/{repo}/check-runs", self.api_base);

        let mut body = serde_json::json!({
            "name": name,
            "head_sha": head_sha,
            "status": status,
        });

        if let Some(details_url) = details_url {
            body["details_url"] = serde_json::Value::String(details_url.to_string());
        }

        let resp = self.api_request(reqwest::Method::POST, &url, &body).await?;
        let check_run: CheckRun = resp.json().await?;
        Ok(check_run)
    }

    /// Update an existing check run with status, conclusion, and/or output.
    pub async fn update_check_run(
        &self,
        owner: &str,
        repo: &str,
        check_run_id: u64,
        status: Option<CheckStatus>,
        conclusion: Option<CheckConclusion>,
        output: Option<&CheckOutput>,
    ) -> Result<(), GitHubError> {
        let url = format!(
            "{}/repos/{owner}/{repo}/check-runs/{check_run_id}",
            self.api_base
        );

        let mut body = serde_json::json!({});

        if let Some(status) = status {
            body["status"] = serde_json::to_value(status).unwrap();
        }
        if let Some(conclusion) = conclusion {
            body["conclusion"] = serde_json::to_value(conclusion).unwrap();
            // A conclusion implies the check is completed.
            body["status"] = serde_json::to_value(CheckStatus::Completed).unwrap();
        }
        if let Some(output) = output {
            body["output"] = serde_json::to_value(output).unwrap();
        }

        self.api_request(reqwest::Method::PATCH, &url, &body)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_status_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_value(CheckStatus::InProgress).unwrap(),
            serde_json::json!("in_progress")
        );
        assert_eq!(
            serde_json::to_value(CheckStatus::Queued).unwrap(),
            serde_json::json!("queued")
        );
        assert_eq!(
            serde_json::to_value(CheckStatus::Completed).unwrap(),
            serde_json::json!("completed")
        );
    }

    #[test]
    fn check_conclusion_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_value(CheckConclusion::Success).unwrap(),
            serde_json::json!("success")
        );
        assert_eq!(
            serde_json::to_value(CheckConclusion::TimedOut).unwrap(),
            serde_json::json!("timed_out")
        );
        assert_eq!(
            serde_json::to_value(CheckConclusion::ActionRequired).unwrap(),
            serde_json::json!("action_required")
        );
    }

    #[test]
    fn check_output_skips_none_text_and_empty_annotations() {
        let output = CheckOutput {
            title: "Build".into(),
            summary: "All good".into(),
            text: None,
            annotations: vec![],
        };
        let json = serde_json::to_value(&output).unwrap();
        assert!(!json.as_object().unwrap().contains_key("text"));
        assert!(!json.as_object().unwrap().contains_key("annotations"));
    }

    #[test]
    fn check_output_includes_text_and_annotations_when_present() {
        let output = CheckOutput {
            title: "Lint".into(),
            summary: "1 warning".into(),
            text: Some("Details here".into()),
            annotations: vec![CheckAnnotation {
                path: "src/main.rs".into(),
                start_line: 10,
                end_line: 10,
                annotation_level: AnnotationLevel::Warning,
                message: "unused variable".into(),
            }],
        };
        let json = serde_json::to_value(&output).unwrap();
        assert_eq!(json["text"], "Details here");
        assert_eq!(json["annotations"].as_array().unwrap().len(), 1);
        assert_eq!(json["annotations"][0]["annotation_level"], "warning");
    }

    #[test]
    fn commit_state_as_str() {
        assert_eq!(CommitState::Pending.as_str(), "pending");
        assert_eq!(CommitState::Success.as_str(), "success");
        assert_eq!(CommitState::Failure.as_str(), "failure");
        assert_eq!(CommitState::Error.as_str(), "error");
    }
}

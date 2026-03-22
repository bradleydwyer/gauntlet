use reqwest::Client;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("GitHub API error: {status} {body}")]
    Api { status: u16, body: String },
}

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
        let url = format!(
            "{}/repos/{owner}/{repo}/statuses/{sha}",
            self.api_base
        );

        let mut body = serde_json::json!({
            "state": state.as_str(),
            "context": context,
            "description": description,
        });

        if let Some(url) = target_url {
            body["target_url"] = serde_json::Value::String(url.to_string());
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "gauntlet-ci")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&body)
            .send()
            .await?;

        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Err(GitHubError::Api { status, body })
        }
    }
}

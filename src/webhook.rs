use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Error)]
pub enum WebhookError {
    #[error("failed to parse webhook payload: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("missing required field: {0}")]
    MissingField(&'static str),
}

#[derive(Debug, Clone)]
pub enum GitHubEvent {
    Push {
        repo_full_name: String,
        branch: String,
        sha: String,
        sender: String,
    },
    PullRequest {
        repo_full_name: String,
        action: String,
        number: u64,
        head_sha: String,
        head_branch: String,
        sender: String,
    },
}

/// Verify the HMAC-SHA256 signature from the `X-Hub-Signature-256` header.
///
/// The header value is expected in the format `sha256=<hex digest>`.
pub fn verify_signature(payload: &[u8], signature: &str, secret: &str) -> bool {
    let Some(hex_digest) = signature.strip_prefix("sha256=") else {
        return false;
    };

    let Ok(expected) = hex::decode(hex_digest) else {
        return false;
    };

    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };

    mac.update(payload);
    mac.verify_slice(&expected).is_ok()
}

/// Parse a GitHub webhook event from the event type header and raw JSON payload.
///
/// Returns `Ok(None)` for event types we don't handle (issues, stars, etc.).
pub fn parse_event(event_type: &str, payload: &[u8]) -> Result<Option<GitHubEvent>, WebhookError> {
    match event_type {
        "push" => parse_push(payload),
        "pull_request" => parse_pull_request(payload),
        _ => Ok(None),
    }
}

fn json_str<'a>(
    value: &'a serde_json::Value,
    field: &'static str,
) -> Result<&'a str, WebhookError> {
    value
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or(WebhookError::MissingField(field))
}

fn parse_push(payload: &[u8]) -> Result<Option<GitHubEvent>, WebhookError> {
    let v: serde_json::Value = serde_json::from_slice(payload)?;

    let git_ref = json_str(&v, "ref")?;

    // Ignore tag pushes.
    if git_ref.starts_with("refs/tags/") {
        return Ok(None);
    }

    let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);

    let repo_full_name = v
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(|n| n.as_str())
        .ok_or(WebhookError::MissingField("repository.full_name"))?;

    let sha = json_str(&v, "after")?;

    let sender = v
        .get("sender")
        .and_then(|s| s.get("login"))
        .and_then(|l| l.as_str())
        .ok_or(WebhookError::MissingField("sender.login"))?;

    Ok(Some(GitHubEvent::Push {
        repo_full_name: repo_full_name.to_string(),
        branch: branch.to_string(),
        sha: sha.to_string(),
        sender: sender.to_string(),
    }))
}

fn parse_pull_request(payload: &[u8]) -> Result<Option<GitHubEvent>, WebhookError> {
    let v: serde_json::Value = serde_json::from_slice(payload)?;

    let action = json_str(&v, "action")?;

    // Only handle actions that represent new or updated code.
    if !matches!(action, "opened" | "synchronize" | "reopened") {
        return Ok(None);
    }

    let pr = v
        .get("pull_request")
        .ok_or(WebhookError::MissingField("pull_request"))?;

    let repo_full_name = v
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(|n| n.as_str())
        .ok_or(WebhookError::MissingField("repository.full_name"))?;

    let number = v
        .get("number")
        .and_then(|n| n.as_u64())
        .ok_or(WebhookError::MissingField("number"))?;

    let head = pr
        .get("head")
        .ok_or(WebhookError::MissingField("pull_request.head"))?;

    let head_sha = json_str(head, "sha")?;

    let head_branch = head
        .get("ref")
        .and_then(|r| r.as_str())
        .ok_or(WebhookError::MissingField("pull_request.head.ref"))?;

    let sender = v
        .get("sender")
        .and_then(|s| s.get("login"))
        .and_then(|l| l.as_str())
        .ok_or(WebhookError::MissingField("sender.login"))?;

    Ok(Some(GitHubEvent::PullRequest {
        repo_full_name: repo_full_name.to_string(),
        action: action.to_string(),
        number,
        head_sha: head_sha.to_string(),
        head_branch: head_branch.to_string(),
        sender: sender.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SECRET: &str = "test-webhook-secret";

    fn compute_signature(payload: &[u8], secret: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let result = mac.finalize().into_bytes();
        format!("sha256={}", hex::encode(result))
    }

    // --- Signature verification ---

    #[test]
    fn verify_valid_signature() {
        let payload = b"hello world";
        let sig = compute_signature(payload, TEST_SECRET);
        assert!(verify_signature(payload, &sig, TEST_SECRET));
    }

    #[test]
    fn reject_invalid_signature() {
        let payload = b"hello world";
        let sig = compute_signature(b"different payload", TEST_SECRET);
        assert!(!verify_signature(payload, &sig, TEST_SECRET));
    }

    #[test]
    fn reject_wrong_secret() {
        let payload = b"hello world";
        let sig = compute_signature(payload, "wrong-secret");
        assert!(!verify_signature(payload, &sig, TEST_SECRET));
    }

    #[test]
    fn reject_missing_prefix() {
        assert!(!verify_signature(b"payload", "not-prefixed", TEST_SECRET));
    }

    #[test]
    fn reject_invalid_hex() {
        assert!(!verify_signature(b"payload", "sha256=zzzz", TEST_SECRET));
    }

    // --- Push event parsing ---

    fn push_payload(git_ref: &str) -> serde_json::Value {
        serde_json::json!({
            "ref": git_ref,
            "after": "abc123def456",
            "repository": { "full_name": "owner/repo" },
            "sender": { "login": "octocat" }
        })
    }

    #[test]
    fn parse_push_event() {
        let payload = push_payload("refs/heads/main");
        let bytes = serde_json::to_vec(&payload).unwrap();

        let event = parse_event("push", &bytes).unwrap().unwrap();
        match event {
            GitHubEvent::Push {
                repo_full_name,
                branch,
                sha,
                sender,
            } => {
                assert_eq!(repo_full_name, "owner/repo");
                assert_eq!(branch, "main");
                assert_eq!(sha, "abc123def456");
                assert_eq!(sender, "octocat");
            }
            _ => panic!("expected Push event"),
        }
    }

    #[test]
    fn parse_push_feature_branch() {
        let payload = push_payload("refs/heads/feature/my-branch");
        let bytes = serde_json::to_vec(&payload).unwrap();

        let event = parse_event("push", &bytes).unwrap().unwrap();
        match event {
            GitHubEvent::Push { branch, .. } => {
                assert_eq!(branch, "feature/my-branch");
            }
            _ => panic!("expected Push event"),
        }
    }

    #[test]
    fn ignore_tag_push() {
        let payload = push_payload("refs/tags/v1.0.0");
        let bytes = serde_json::to_vec(&payload).unwrap();

        let event = parse_event("push", &bytes).unwrap();
        assert!(event.is_none());
    }

    // --- Pull request event parsing ---

    fn pr_payload(action: &str) -> serde_json::Value {
        serde_json::json!({
            "action": action,
            "number": 42,
            "pull_request": {
                "head": {
                    "sha": "def789",
                    "ref": "feature-branch"
                }
            },
            "repository": { "full_name": "owner/repo" },
            "sender": { "login": "octocat" }
        })
    }

    #[test]
    fn parse_pr_opened() {
        let payload = pr_payload("opened");
        let bytes = serde_json::to_vec(&payload).unwrap();

        let event = parse_event("pull_request", &bytes).unwrap().unwrap();
        match event {
            GitHubEvent::PullRequest {
                repo_full_name,
                action,
                number,
                head_sha,
                head_branch,
                sender,
            } => {
                assert_eq!(repo_full_name, "owner/repo");
                assert_eq!(action, "opened");
                assert_eq!(number, 42);
                assert_eq!(head_sha, "def789");
                assert_eq!(head_branch, "feature-branch");
                assert_eq!(sender, "octocat");
            }
            _ => panic!("expected PullRequest event"),
        }
    }

    #[test]
    fn parse_pr_synchronize() {
        let payload = pr_payload("synchronize");
        let bytes = serde_json::to_vec(&payload).unwrap();

        let event = parse_event("pull_request", &bytes).unwrap();
        assert!(event.is_some());
    }

    #[test]
    fn parse_pr_reopened() {
        let payload = pr_payload("reopened");
        let bytes = serde_json::to_vec(&payload).unwrap();

        let event = parse_event("pull_request", &bytes).unwrap();
        assert!(event.is_some());
    }

    #[test]
    fn ignore_pr_closed() {
        let payload = pr_payload("closed");
        let bytes = serde_json::to_vec(&payload).unwrap();

        let event = parse_event("pull_request", &bytes).unwrap();
        assert!(event.is_none());
    }

    #[test]
    fn ignore_pr_labeled() {
        let payload = pr_payload("labeled");
        let bytes = serde_json::to_vec(&payload).unwrap();

        let event = parse_event("pull_request", &bytes).unwrap();
        assert!(event.is_none());
    }

    // --- Unknown event types ---

    #[test]
    fn unknown_event_returns_none() {
        let bytes = b"{}";
        assert!(parse_event("issues", bytes).unwrap().is_none());
        assert!(parse_event("star", bytes).unwrap().is_none());
        assert!(parse_event("fork", bytes).unwrap().is_none());
    }

    // --- Error cases ---

    #[test]
    fn invalid_json_returns_error() {
        let result = parse_event("push", b"not json");
        assert!(result.is_err());
    }

    #[test]
    fn push_missing_ref_returns_error() {
        let payload = serde_json::json!({
            "after": "abc123",
            "repository": { "full_name": "owner/repo" },
            "sender": { "login": "octocat" }
        });
        let bytes = serde_json::to_vec(&payload).unwrap();
        let result = parse_event("push", &bytes);
        assert!(result.is_err());
    }
}

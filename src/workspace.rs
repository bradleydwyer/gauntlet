use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;
use tracing::{debug, info, warn};

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("git clone failed: {0}")]
    CloneFailed(String),
    #[error("git fetch failed: {0}")]
    FetchFailed(String),
    #[error("git checkout failed: {0}")]
    CheckoutFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct WorkspaceManager {
    base_dir: PathBuf,
}

impl WorkspaceManager {
    pub fn new(base_dir: &Path) -> Self {
        Self {
            base_dir: base_dir.to_path_buf(),
        }
    }

    /// Build the workspace path for a given repo.
    fn workspace_path(&self, repo_full_name: &str) -> PathBuf {
        self.base_dir.join("workspaces").join(repo_full_name)
    }

    /// Ensure a repo is cloned and checked out at the given SHA.
    /// Returns the path to the workspace directory.
    pub async fn prepare(
        &self,
        repo_full_name: &str,
        sha: &str,
        token: &str,
    ) -> Result<PathBuf, WorkspaceError> {
        let workspace = self.workspace_path(repo_full_name);

        if workspace.exists() {
            self.fetch_and_checkout(&workspace, repo_full_name, sha, token)
                .await?;
        } else {
            self.clone_repo(repo_full_name, token, &workspace).await?;
            self.checkout(&workspace, sha).await?;
        }

        Ok(workspace)
    }

    async fn clone_repo(
        &self,
        repo_full_name: &str,
        token: &str,
        workspace: &Path,
    ) -> Result<(), WorkspaceError> {
        if let Some(parent) = workspace.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let url = format!("https://x-access-token:{token}@github.com/{repo_full_name}.git");

        info!(repo = repo_full_name, path = %workspace.display(), "cloning repository");

        let output = Command::new("git")
            .args(["clone", "--depth", "1", &url])
            .arg(workspace)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorkspaceError::CloneFailed(stderr.into_owned()));
        }

        Ok(())
    }

    async fn fetch_and_checkout(
        &self,
        workspace: &Path,
        repo_full_name: &str,
        sha: &str,
        token: &str,
    ) -> Result<(), WorkspaceError> {
        debug!(sha, path = %workspace.display(), "fetching sha");

        // Update remote URL with fresh token (tokens are short-lived).
        let url = format!("https://x-access-token:{token}@github.com/{repo_full_name}.git");
        let _ = Command::new("git")
            .args(["remote", "set-url", "origin", &url])
            .current_dir(workspace)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await;

        let output = Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(workspace)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorkspaceError::FetchFailed(stderr.into_owned()));
        }

        self.checkout(workspace, sha).await
    }

    async fn checkout(&self, workspace: &Path, reference: &str) -> Result<(), WorkspaceError> {
        debug!(reference, path = %workspace.display(), "checking out ref");

        let output = Command::new("git")
            .args(["checkout", reference])
            .current_dir(workspace)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorkspaceError::CheckoutFailed(stderr.into_owned()));
        }

        Ok(())
    }

    /// Clean up old workspaces that haven't been used recently.
    /// Returns the number of workspaces removed.
    pub async fn cleanup(&self, max_age: Duration) -> Result<usize, WorkspaceError> {
        let workspaces_dir = self.base_dir.join("workspaces");
        if !workspaces_dir.exists() {
            return Ok(0);
        }

        let mut removed = 0;

        // Iterate owner directories
        let mut owners = tokio::fs::read_dir(&workspaces_dir).await?;
        while let Some(owner_entry) = owners.next_entry().await? {
            if !owner_entry.file_type().await?.is_dir() {
                continue;
            }

            let mut repos = tokio::fs::read_dir(owner_entry.path()).await?;
            while let Some(repo_entry) = repos.next_entry().await? {
                if !repo_entry.file_type().await?.is_dir() {
                    continue;
                }

                let metadata = repo_entry.metadata().await?;
                let modified = metadata.modified()?;
                let age = modified.elapsed().unwrap_or(Duration::ZERO);

                if age > max_age {
                    let path = repo_entry.path();
                    info!(path = %path.display(), ?age, "removing stale workspace");
                    tokio::fs::remove_dir_all(&path).await?;
                    removed += 1;
                }
            }

            // Remove owner dir if now empty
            let owner_path = owner_entry.path();
            let mut remaining = tokio::fs::read_dir(&owner_path).await?;
            if remaining.next_entry().await?.is_none() {
                debug!(path = %owner_path.display(), "removing empty owner directory");
                tokio::fs::remove_dir_all(&owner_path).await?;
            }
        }

        if removed > 0 {
            warn!(removed, "cleaned up stale workspaces");
        }

        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_path_construction() {
        let manager = WorkspaceManager::new(Path::new("/tmp/gauntlet"));
        let path = manager.workspace_path("octocat/hello-world");
        assert_eq!(
            path,
            PathBuf::from("/tmp/gauntlet/workspaces/octocat/hello-world")
        );
    }

    #[test]
    fn workspace_path_preserves_structure() {
        let manager = WorkspaceManager::new(Path::new("/home/ci/.gauntlet"));
        let path = manager.workspace_path("my-org/my-repo");
        assert_eq!(
            path,
            PathBuf::from("/home/ci/.gauntlet/workspaces/my-org/my-repo")
        );
    }

    #[tokio::test]
    async fn cleanup_empty_dir_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = WorkspaceManager::new(tmp.path());
        let removed = manager.cleanup(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn cleanup_removes_old_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = WorkspaceManager::new(tmp.path());

        // Create a fake workspace directory
        let workspace = tmp.path().join("workspaces/test-owner/test-repo");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        // With max_age of 0, everything is "old"
        let removed = manager.cleanup(Duration::ZERO).await.unwrap();
        assert_eq!(removed, 1);
        assert!(!workspace.exists());
    }

    #[tokio::test]
    async fn cleanup_keeps_recent_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = WorkspaceManager::new(tmp.path());

        // Create a fake workspace directory (just created, so it's recent)
        let workspace = tmp.path().join("workspaces/test-owner/test-repo");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        // With a large max_age, the workspace should be kept
        let removed = manager.cleanup(Duration::from_secs(86400)).await.unwrap();
        assert_eq!(removed, 0);
        assert!(workspace.exists());
    }
}

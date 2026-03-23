//! Git worktree management for clean local builds.
//!
//! Creates an isolated checkout via `git worktree` with uncommitted changes applied.
//! Detects Rust path dependency patches and creates worktrees for sibling repos.

use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, info, warn};

/// A managed worktree build environment.
pub struct BuildWorktree {
    /// Root temp directory containing all worktrees.
    pub root: PathBuf,
    /// Path to the main repo worktree.
    pub repo_dir: PathBuf,
    /// Extra volume mounts for Docker: (host_path, container_path).
    pub extra_mounts: Vec<(String, String)>,
}

impl Drop for BuildWorktree {
    fn drop(&mut self) {
        cleanup_worktrees(&self.root);
    }
}

/// Create a build worktree from the current repo.
///
/// 1. Creates a clean checkout via `git worktree add`
/// 2. Applies uncommitted changes (staged + unstaged)
/// 3. Copies `.cargo/config.toml` if present
/// 4. Detects path deps and creates worktrees for sibling repos
pub fn create_build_worktree() -> Result<BuildWorktree, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let id = &uuid::Uuid::new_v4().to_string()[..8];
    let root = std::env::temp_dir().join(format!("gauntlet-run-{id}"));
    let repo_dir = root.join("repo");

    std::fs::create_dir_all(&root).map_err(|e| format!("mkdir: {e}"))?;

    // Create detached worktree from HEAD.
    info!("creating worktree at {}", repo_dir.display());
    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &repo_dir.to_string_lossy(),
            "HEAD",
        ])
        .current_dir(&cwd)
        .output()
        .map_err(|e| format!("git worktree add: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {stderr}"));
    }

    // Apply unstaged changes to tracked files.
    let diff = Command::new("git")
        .args(["diff"])
        .current_dir(&cwd)
        .output()
        .map_err(|e| format!("git diff: {e}"))?;

    if !diff.stdout.is_empty() {
        let apply = Command::new("git")
            .args(["apply", "--allow-empty"])
            .current_dir(&repo_dir)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(ref mut stdin) = child.stdin {
                    stdin.write_all(&diff.stdout)?;
                }
                child.wait()
            });
        if let Err(e) = apply {
            warn!(error = %e, "failed to apply unstaged diff");
        }
    }

    // Apply staged changes (including new files).
    let staged = Command::new("git")
        .args(["diff", "--cached"])
        .current_dir(&cwd)
        .output()
        .map_err(|e| format!("git diff --cached: {e}"))?;

    if !staged.stdout.is_empty() {
        let apply = Command::new("git")
            .args(["apply", "--allow-empty"])
            .current_dir(&repo_dir)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(ref mut stdin) = child.stdin {
                    stdin.write_all(&staged.stdout)?;
                }
                child.wait()
            });
        if let Err(e) = apply {
            warn!(error = %e, "failed to apply staged diff");
        }
    }

    // Copy .cargo/config.toml if it exists (gitignored but affects build).
    let cargo_config = cwd.join(".cargo/config.toml");
    if cargo_config.exists() {
        let dest = repo_dir.join(".cargo");
        let _ = std::fs::create_dir_all(&dest);
        let _ = std::fs::copy(&cargo_config, dest.join("config.toml"));
        debug!("copied .cargo/config.toml into worktree");
    }

    // Detect path deps and create sibling worktrees.
    let extra_mounts = detect_and_mount_path_deps(&cwd, &root, &repo_dir)?;

    Ok(BuildWorktree {
        root,
        repo_dir,
        extra_mounts,
    })
}

/// Parse .cargo/config.toml for [patch] path deps that reference outside the repo.
/// Create worktrees for sibling repos and return Docker mount pairs.
fn detect_and_mount_path_deps(
    cwd: &Path,
    root: &Path,
    _repo_dir: &Path,
) -> Result<Vec<(String, String)>, String> {
    let cargo_config = cwd.join(".cargo/config.toml");
    if !cargo_config.exists() {
        return Ok(vec![]);
    }

    let content =
        std::fs::read_to_string(&cargo_config).map_err(|e| format!("read config.toml: {e}"))?;

    let mut mounts = Vec::new();

    // Parse path = "..." entries from [patch.*] sections.
    // Simple line-by-line parsing — not a full TOML parser.
    for line in content.lines() {
        let line = line.trim();
        if !line.contains("path") || !line.contains('"') {
            continue;
        }

        // Extract path value: path = "../tasked/tasked" or path = "../foo"
        let path_value = extract_path_value(line);
        if let Some(rel_path) = path_value {
            // Resolve relative to cwd.
            let abs_path = cwd.join(&rel_path);
            let abs_path = abs_path
                .canonicalize()
                .unwrap_or_else(|_| cwd.join(&rel_path));

            // Check if it references outside the repo.
            if abs_path.starts_with(cwd) {
                continue; // Inside the repo, already in the worktree.
            }

            // Find the git repo root for this path.
            let repo_root = find_git_root(&abs_path);
            if repo_root.is_none() {
                warn!(path = %rel_path, "path dep is not in a git repo, skipping");
                continue;
            }
            let sibling_root = repo_root.unwrap();
            let sibling_name = sibling_root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            // Check if we already created a worktree for this sibling.
            let sibling_worktree = root.join(&sibling_name);
            if !sibling_worktree.exists() {
                info!(
                    sibling = %sibling_name,
                    "creating worktree for path dependency"
                );
                let output = Command::new("git")
                    .args([
                        "worktree",
                        "add",
                        "--detach",
                        &sibling_worktree.to_string_lossy(),
                        "HEAD",
                    ])
                    .current_dir(&sibling_root)
                    .output()
                    .map_err(|e| format!("git worktree add for {sibling_name}: {e}"))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!(
                        sibling = %sibling_name,
                        error = %stderr,
                        "failed to create sibling worktree"
                    );
                    continue;
                }
            }

            // Calculate the mount path inside Docker.
            // The rel_path is e.g. "../tasked/tasked" from the repo.
            // Inside Docker, /workspace is the repo. We need ../tasked to resolve.
            // Mount the sibling worktree at the right relative position.
            //
            // Approach: mount at /deps/<sibling_name> and rewrite the path.
            // Actually simpler: figure out what relative path from /workspace
            // would reach the dep. Since /workspace is the repo, and the dep
            // is at rel_path from the repo, we need to mount the sibling root
            // such that the relative path resolves.
            //
            // e.g., path = "../tasked/tasked" means from /workspace, go up one
            // level to find "tasked". So mount the sibling at /tasked (sibling of /workspace).
            // The container path needs to be the parent of /workspace + sibling_name.
            let container_path = format!("/{sibling_name}");
            mounts.push((
                sibling_worktree.to_string_lossy().to_string(),
                container_path,
            ));
        }
    }

    if !mounts.is_empty() {
        info!(
            count = mounts.len(),
            "detected path dependencies, mounting sibling repos"
        );
    }

    Ok(mounts)
}

/// Extract path value from a TOML line like: `tasked = { path = "../tasked/tasked" }`
fn extract_path_value(line: &str) -> Option<String> {
    // Find `path = "..."` or `path = '...'`
    let path_idx = line.find("path")?;
    let after_path = &line[path_idx + 4..];
    let eq_idx = after_path.find('=')?;
    let after_eq = after_path[eq_idx + 1..].trim();

    if let Some(stripped) = after_eq.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else if let Some(stripped) = after_eq.strip_prefix('\'') {
        let end = stripped.find('\'')?;
        Some(stripped[..end].to_string())
    } else {
        None
    }
}

/// Find the git repo root containing a path.
fn find_git_root(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(if path.is_dir() { path } else { path.parent()? })
        .output()
        .ok()?;

    if output.status.success() {
        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some(PathBuf::from(root))
    } else {
        None
    }
}

/// Clean up all worktrees under the root directory.
fn cleanup_worktrees(root: &Path) {
    // Find all worktrees we created and remove them.
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Try to find the parent git repo to remove the worktree properly.
                let _ = Command::new("git")
                    .args(["worktree", "remove", "--force", &path.to_string_lossy()])
                    .output();
            }
        }
    }
    // Remove the root temp directory.
    let _ = std::fs::remove_dir_all(root);
    debug!(path = %root.display(), "cleaned up build worktrees");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_path_double_quotes() {
        let line = r#"tasked = { path = "../tasked/tasked", features = ["docker"] }"#;
        assert_eq!(
            extract_path_value(line),
            Some("../tasked/tasked".to_string())
        );
    }

    #[test]
    fn extract_path_single_quotes() {
        let line = "foo = { path = '../foo' }";
        assert_eq!(extract_path_value(line), Some("../foo".to_string()));
    }

    #[test]
    fn extract_path_no_path() {
        let line = r#"tasked = { git = "https://github.com/foo/bar" }"#;
        assert_eq!(extract_path_value(line), None);
    }

    #[test]
    fn extract_path_equals_style() {
        let line = r#"path = "../other/crate""#;
        assert_eq!(extract_path_value(line), Some("../other/crate".to_string()));
    }
}

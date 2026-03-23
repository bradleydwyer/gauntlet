//! Docker image caching for setup commands.
//!
//! When a runner has a `setup` command, we build a derived Docker image
//! with the setup baked in as a layer. This avoids running the setup
//! command in every container.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::process::Command;
use tracing::{debug, info};

/// Build or retrieve a cached Docker image with setup commands pre-applied.
///
/// Returns the cached image name (e.g., `gauntlet-cache:a1b2c3d4`).
/// If the image already exists, returns immediately.
/// If not, builds it from a temporary Dockerfile.
pub fn ensure_setup_image(base_image: &str, setup: &str) -> Result<String, String> {
    let tag = cache_tag(base_image, setup);
    let image_name = format!("gauntlet-cache:{tag}");

    // Check if cached image exists.
    let check = Command::new("docker")
        .args(["image", "inspect", &image_name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    if let Ok(status) = check
        && status.success()
    {
        debug!(image = %image_name, "using cached setup image");
        return Ok(image_name);
    }

    // Build the cached image.
    info!(
        base = %base_image,
        image = %image_name,
        "building cached setup image"
    );

    let dockerfile = format!("FROM {base_image}\nRUN {setup}\n");
    let output = Command::new("docker")
        .args(["build", "-t", &image_name, "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(dockerfile.as_bytes())?;
            }
            child.wait_with_output()
        })
        .map_err(|e| format!("docker build failed: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("docker build failed: {stderr}"));
    }

    info!(image = %image_name, "cached setup image built");
    Ok(image_name)
}

/// Compute a short hash tag from the base image + setup command.
fn cache_tag(base_image: &str, setup: &str) -> String {
    let mut hasher = DefaultHasher::new();
    base_image.hash(&mut hasher);
    setup.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_tag_deterministic() {
        let a = cache_tag("rust:latest", "rustup component add clippy");
        let b = cache_tag("rust:latest", "rustup component add clippy");
        assert_eq!(a, b);
    }

    #[test]
    fn cache_tag_differs_by_setup() {
        let a = cache_tag("rust:latest", "rustup component add clippy");
        let b = cache_tag("rust:latest", "rustup component add rustfmt");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_tag_differs_by_image() {
        let a = cache_tag("rust:latest", "echo hi");
        let b = cache_tag("rust:1.80", "echo hi");
        assert_ne!(a, b);
    }
}

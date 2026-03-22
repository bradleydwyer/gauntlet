use crate::compiler::BuildContext;
use crate::schema::CheckoutConfig;
use tasked::types::TaskDef;

/// The synthetic task ID for the checkout step.
pub const CHECKOUT_TASK_ID: &str = "__checkout";

/// Generate a checkout TaskDef based on the build context.
pub fn checkout_task(config: &CheckoutConfig, ctx: &BuildContext) -> TaskDef {
    let depth_flag = if config.depth > 0 {
        format!("--depth {}", config.depth)
    } else {
        String::new()
    };

    let submodule_cmd = if config.submodules {
        "\ngit submodule update --init --recursive"
    } else {
        ""
    };

    let lfs_cmd = if config.lfs { "\ngit lfs pull" } else { "" };

    // If we have a specific ref, fetch and checkout.
    // If running locally with no ref, this is essentially a noop that verifies git state.
    let command = if let Some(ref git_ref) = ctx.git_ref {
        format!(
            "set -euo pipefail\ngit fetch origin {depth_flag} {git_ref}\ngit checkout FETCH_HEAD{submodule_cmd}{lfs_cmd}",
        )
    } else {
        // Local mode: just verify we're in a git repo and report the current ref.
        format!(
            "set -euo pipefail\ngit rev-parse --git-dir > /dev/null 2>&1\necho \"ref: $(git rev-parse --short HEAD)\"{submodule_cmd}{lfs_cmd}",
        )
    };

    TaskDef {
        id: CHECKOUT_TASK_ID.into(),
        executor: "shell".into(),
        config: serde_json::json!({ "command": command }),
        input: None,
        depends_on: vec![],
        timeout_secs: Some(120),
        retries: Some(1),
        backoff: None,
        condition: None,
        spawn_output: vec![],
    }
}

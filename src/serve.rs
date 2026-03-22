//! Gauntlet serve daemon — webhook receiver + poller + embedded tasked engine.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use tasked::engine::{Engine, EngineConfig};
use tasked::executor::NoopExecutor;
use tasked::executor::approval::ApprovalExecutor;
use tasked::executor::delay::DelayExecutor;
use tasked::executor::http::HttpExecutor;
use tasked::executor::shell::ShellExecutor;
use tasked::executor::spawn::SpawnExecutor;
use tasked::store::memory::MemoryStorage;
use tasked::types::{FlowState, QueueConfig, QueueId};

use crate::compiler::{self, BuildContext};
use crate::github::{CheckConclusion, CheckStatus, GitHubClient};
use crate::github_app::GitHubApp;
use crate::schema::Pipeline;
use crate::webhook::{self, GitHubEvent};
use crate::workspace::WorkspaceManager;

/// Configuration for the serve daemon.
pub struct ServeConfig {
    pub port: u16,
    pub data_dir: PathBuf,
    pub github_app: Arc<GitHubApp>,
    pub webhook_secret: Option<String>,
    pub poll_interval_secs: u64,
    pub concurrency: usize,
}

/// Shared state for the HTTP server and build system.
struct AppState {
    engine: Arc<Engine>,
    github_app: Arc<GitHubApp>,
    workspace: WorkspaceManager,
    webhook_secret: Option<String>,
    /// Track last seen SHA per repo+branch to avoid duplicate builds.
    last_seen: Mutex<HashMap<String, String>>,
    /// Track active builds: flow_id → (repo, sha, check_run_id).
    active_builds: Mutex<HashMap<String, BuildInfo>>,
}

struct BuildInfo {
    repo: String,
    sha: String,
    check_run_id: Option<u64>,
}

/// Run the gauntlet serve daemon.
pub async fn run(config: ServeConfig) {
    let workspace = WorkspaceManager::new(&config.data_dir);

    // Create embedded tasked engine.
    let storage = Arc::new(MemoryStorage::new());
    let engine_config = EngineConfig {
        poll_interval: std::time::Duration::from_millis(500),
        ..EngineConfig::default()
    };
    let mut engine = Engine::new(storage, engine_config);

    // Register executors.
    engine.register_executor("shell", Arc::new(ShellExecutor));
    engine.register_executor("http", Arc::new(HttpExecutor::new()));
    engine.register_executor("noop", Arc::new(NoopExecutor));
    engine.register_executor("delay", Arc::new(DelayExecutor));
    engine.register_executor("approval", Arc::new(ApprovalExecutor));

    // Register Docker executors if available.
    {
        use tasked::executor::agent::AgentExecutor;
        use tasked::executor::container::ContainerExecutor;
        use tasked::executor::container::docker::DockerBackend;

        if let Ok(backend) = DockerBackend::new() {
            engine.register_executor("container", Arc::new(ContainerExecutor::new(backend)));
            if let Ok(agent_backend) = DockerBackend::new() {
                engine.register_executor(
                    "agent",
                    Arc::new(AgentExecutor::new(ContainerExecutor::new(agent_backend))),
                );
            }
        }
    }

    // Spawn executor (registered last so it sees all others).
    let executors = engine.executors().clone();
    engine.register_executor("spawn", Arc::new(SpawnExecutor::new(executors)));

    // Create the "builds" queue.
    let queue_id = QueueId::from("builds");
    let queue_config = QueueConfig {
        concurrency: config.concurrency,
        ..QueueConfig::default()
    };
    let _ = engine.create_queue(&queue_id, queue_config).await;

    let engine = Arc::new(engine);

    // Start the engine loop in the background.
    let engine_handle = engine.clone();
    let engine_task = tokio::spawn(async move {
        engine_handle.run().await;
    });

    let state = Arc::new(AppState {
        engine: engine.clone(),
        github_app: config.github_app.clone(),
        workspace,
        webhook_secret: config.webhook_secret.clone(),
        last_seen: Mutex::new(HashMap::new()),
        active_builds: Mutex::new(HashMap::new()),
    });

    // Build HTTP server.
    let app = Router::new()
        .route("/webhook/github", post(handle_webhook))
        .route("/status", get(handle_status))
        .with_state(state.clone());

    let addr = format!("0.0.0.0:{}", config.port);
    info!(port = config.port, "gauntlet serve starting");

    if config.webhook_secret.is_some() {
        info!("webhook mode: listening for GitHub push events");
    } else {
        info!(
            interval_secs = config.poll_interval_secs,
            "poll mode: no webhook secret configured"
        );
    }

    // Start polling if no webhook secret (poll mode).
    let poll_state = state.clone();
    let poll_interval = config.poll_interval_secs;
    let poll_task = tokio::spawn(async move {
        if poll_state.webhook_secret.is_some() {
            // Webhook mode — don't poll. Just wait forever.
            std::future::pending::<()>().await;
        } else {
            poll_loop(poll_state, poll_interval).await;
        }
    });

    // Start build completion monitor.
    let monitor_state = state.clone();
    let monitor_task = tokio::spawn(async move {
        build_monitor(monitor_state).await;
    });

    // Run HTTP server.
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!("listening on {addr}");
    axum::serve(listener, app).await.unwrap();

    // Clean up (unreachable in practice).
    engine_task.abort();
    poll_task.abort();
    monitor_task.abort();
}

// ── Webhook handler ──

async fn handle_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Verify signature if webhook secret is configured.
    if let Some(ref secret) = state.webhook_secret {
        let signature = headers
            .get("x-hub-signature-256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !webhook::verify_signature(&body, signature, secret) {
            return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
        }
    }

    let event_type = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let event = match webhook::parse_event(event_type, &body) {
        Ok(Some(event)) => event,
        Ok(None) => return (StatusCode::OK, "ignored").into_response(),
        Err(e) => {
            warn!(error = %e, "failed to parse webhook");
            return (StatusCode::BAD_REQUEST, "parse error").into_response();
        }
    };

    // Trigger build.
    let state_clone = state.clone();
    tokio::spawn(async move {
        if let Err(e) = trigger_build(&state_clone, &event).await {
            error!(error = %e, "build trigger failed");
        }
    });

    (StatusCode::OK, "build triggered").into_response()
}

async fn handle_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let builds = state.active_builds.lock().await;
    let active: Vec<serde_json::Value> = builds
        .iter()
        .map(|(flow_id, info)| {
            serde_json::json!({
                "flow_id": flow_id,
                "repo": info.repo,
                "sha": &info.sha[..7.min(info.sha.len())],
            })
        })
        .collect();

    Json(serde_json::json!({
        "status": "ok",
        "active_builds": active,
    }))
}

// ── Build triggering ──

async fn trigger_build(state: &AppState, event: &GitHubEvent) -> Result<(), String> {
    let (repo, branch, sha) = match event {
        GitHubEvent::Push {
            repo_full_name,
            branch,
            sha,
            ..
        } => (repo_full_name.clone(), branch.clone(), sha.clone()),
        GitHubEvent::PullRequest {
            repo_full_name,
            head_sha,
            head_branch,
            ..
        } => (
            repo_full_name.clone(),
            head_branch.clone(),
            head_sha.clone(),
        ),
    };

    // Dedup: skip if we've already seen this SHA for this repo+branch.
    let dedup_key = format!("{repo}:{branch}");
    {
        let mut seen = state.last_seen.lock().await;
        if seen.get(&dedup_key) == Some(&sha) {
            return Ok(());
        }
        seen.insert(dedup_key, sha.clone());
    }

    info!(repo = %repo, branch = %branch, sha = &sha[..7.min(sha.len())], "triggering build");

    // Get GitHub token.
    let token = state
        .github_app
        .token()
        .await
        .map_err(|e| format!("failed to get GitHub token: {e}"))?;

    // Prepare workspace.
    let workspace_dir = state
        .workspace
        .prepare(&repo, &sha, &token)
        .await
        .map_err(|e| format!("workspace prepare failed: {e}"))?;

    // Load pipeline.
    let pipeline_path = workspace_dir.join(".gauntlet/pipeline.json");
    if !pipeline_path.exists() {
        info!(repo = %repo, "no .gauntlet/pipeline.json found, skipping");
        return Ok(());
    }

    let pipeline_json = std::fs::read_to_string(&pipeline_path)
        .map_err(|e| format!("failed to read pipeline: {e}"))?;
    let mut pipeline: Pipeline = serde_json::from_str(&pipeline_json)
        .map_err(|e| format!("failed to parse pipeline: {e}"))?;

    // Workspace manager already cloned and checked out the SHA.
    // Disable the pipeline's checkout step to avoid double-checkout.
    pipeline.checkout = crate::schema::CheckoutSetting::Enabled(false);

    // Compile.
    let ctx = BuildContext {
        repo_dir: Some(workspace_dir.to_string_lossy().to_string()),
        git_ref: None, // Already checked out by workspace manager.
        branch: Some(branch.clone()),
        event: Some(match event {
            GitHubEvent::Push { .. } => "push".to_string(),
            GitHubEvent::PullRequest { .. } => "pull_request".to_string(),
        }),
        env_overrides: HashMap::new(),
    };

    let compile_result =
        compiler::compile(&pipeline, &ctx).map_err(|e| format!("compilation failed: {e}"))?;

    // Create check run on GitHub.
    let (owner, repo_name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repo format: {repo}"))?;

    let github = GitHubClient::new(token.clone());
    let check_run = github
        .create_check_run(
            owner,
            repo_name,
            "gauntlet",
            &sha,
            CheckStatus::InProgress,
            None::<&str>,
        )
        .await
        .map_err(|e| format!("failed to create check run: {e}"))?;

    // Submit flow to engine.
    let queue_id = QueueId::from("builds");

    // Ensure queue exists.
    let _ = state
        .engine
        .create_queue(&queue_id, compile_result.queue_config)
        .await;

    let flow = state
        .engine
        .submit_flow(&queue_id, compile_result.flow_def)
        .await
        .map_err(|e| format!("failed to submit flow: {e}"))?;

    let flow_id = flow.id.0.clone();
    info!(flow_id = %flow_id, repo = %repo, sha = &sha[..7.min(sha.len())], "build submitted");

    // Track the build.
    state.active_builds.lock().await.insert(
        flow_id,
        BuildInfo {
            repo: repo.clone(),
            sha: sha.clone(),
            check_run_id: Some(check_run.id),
        },
    );

    Ok(())
}

// ── Poll loop ──

async fn poll_loop(state: Arc<AppState>, interval_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));

    loop {
        interval.tick().await;

        let token = match state.github_app.token().await {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "failed to get GitHub token for polling");
                continue;
            }
        };

        if let Err(e) = poll_repos(&state, &token).await {
            warn!(error = %e, "poll cycle failed");
        }
    }
}

async fn poll_repos(state: &AppState, token: &str) -> Result<(), String> {
    let client = reqwest::Client::new();

    // List repos (sorted by most recently pushed).
    let resp = client
        .get("https://api.github.com/user/repos")
        .query(&[("sort", "pushed"), ("per_page", "100")])
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "gauntlet-ci")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("list repos failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("list repos returned {}", resp.status()));
    }

    let repos: Vec<serde_json::Value> = resp
        .json()
        .await
        .map_err(|e| format!("parse repos failed: {e}"))?;

    for repo in &repos {
        let full_name = repo["full_name"].as_str().unwrap_or("");
        let default_branch = repo["default_branch"].as_str().unwrap_or("main");

        if full_name.is_empty() {
            continue;
        }

        // Check for new commits on default branch.
        let commits_url = format!(
            "https://api.github.com/repos/{full_name}/commits?sha={default_branch}&per_page=1"
        );
        let resp = client
            .get(&commits_url)
            .header("Authorization", format!("Bearer {token}"))
            .header("User-Agent", "gauntlet-ci")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await;

        let resp = match resp {
            Ok(r) if r.status().is_success() => r,
            _ => continue,
        };

        let commits: Vec<serde_json::Value> = match resp.json().await {
            Ok(c) => c,
            Err(_) => continue,
        };

        if let Some(commit) = commits.first() {
            let sha = commit["sha"].as_str().unwrap_or("").to_string();
            if sha.is_empty() {
                continue;
            }

            let dedup_key = format!("{full_name}:{default_branch}");
            let already_seen = {
                let seen = state.last_seen.lock().await;
                seen.get(&dedup_key) == Some(&sha)
            };

            if !already_seen {
                let event = GitHubEvent::Push {
                    repo_full_name: full_name.to_string(),
                    branch: default_branch.to_string(),
                    sha: sha.clone(),
                    sender: String::new(),
                };

                if let Err(e) = trigger_build(state, &event).await {
                    warn!(repo = %full_name, error = %e, "poll-triggered build failed");
                }
            }
        }
    }

    Ok(())
}

// ── Build completion monitor ──

async fn build_monitor(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));

    loop {
        interval.tick().await;

        let builds: Vec<(String, BuildInfo)> = {
            let guard = state.active_builds.lock().await;
            guard
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        BuildInfo {
                            repo: v.repo.clone(),
                            sha: v.sha.clone(),
                            check_run_id: v.check_run_id,
                        },
                    )
                })
                .collect()
        };

        for (flow_id, build_info) in &builds {
            let flow = match state
                .engine
                .get_flow(&tasked::types::FlowId(flow_id.clone()))
                .await
            {
                Ok(Some(f)) => f,
                _ => continue,
            };

            if !flow.state.is_terminal() {
                continue;
            }

            // Flow is done — update GitHub.
            let token = match state.github_app.token().await {
                Ok(t) => t,
                Err(e) => {
                    warn!(error = %e, "failed to get token for check run update");
                    continue;
                }
            };

            let (owner, repo_name) = match build_info.repo.split_once('/') {
                Some(pair) => pair,
                None => continue,
            };

            let github = GitHubClient::new(token.clone());

            // Update check run.
            if let Some(check_run_id) = build_info.check_run_id {
                let conclusion = match flow.state {
                    FlowState::Succeeded => CheckConclusion::Success,
                    FlowState::Failed => CheckConclusion::Failure,
                    FlowState::Cancelled => CheckConclusion::Cancelled,
                    _ => continue,
                };

                // Fetch task details for the output.
                let tasks = state
                    .engine
                    .get_flow_tasks(&tasked::types::FlowId(flow_id.clone()))
                    .await
                    .unwrap_or_default();

                let mut text = format_build_output(&tasks);
                // GitHub Checks API limits text to 65535 characters.
                if text.len() > 60000 {
                    text.truncate(60000);
                    text.push_str("\n\n... (output truncated)");
                }

                let output = crate::github::CheckOutput {
                    title: format!(
                        "gauntlet: {}",
                        match flow.state {
                            FlowState::Succeeded => "passed",
                            FlowState::Failed => "failed",
                            FlowState::Cancelled => "cancelled",
                            _ => "unknown",
                        }
                    ),
                    summary: format!(
                        "{}/{} tasks succeeded",
                        flow.tasks_succeeded, flow.task_count
                    ),
                    text: Some(text),
                    annotations: vec![],
                };

                if let Err(e) = github
                    .update_check_run(
                        owner,
                        repo_name,
                        check_run_id,
                        Some(CheckStatus::Completed),
                        Some(conclusion),
                        Some(&output),
                    )
                    .await
                {
                    warn!(error = %e, flow_id, "failed to update check run");
                }
            }

            // Also set commit status.
            let commit_state = match flow.state {
                FlowState::Succeeded => crate::github::CommitState::Success,
                FlowState::Failed => crate::github::CommitState::Failure,
                _ => crate::github::CommitState::Error,
            };

            let _ = github
                .set_commit_status(
                    owner,
                    repo_name,
                    &build_info.sha,
                    "gauntlet",
                    commit_state,
                    "",
                    None,
                )
                .await;

            info!(
                flow_id,
                repo = %build_info.repo,
                state = %flow.state,
                "build completed"
            );

            // Remove from active builds.
            state.active_builds.lock().await.remove(flow_id);
        }
    }
}

/// Format task results as markdown for the GitHub check run output.
fn format_build_output(tasks: &[tasked::types::Task]) -> String {
    use tasked::types::TaskState;

    let mut lines = Vec::new();

    for task in tasks {
        let icon = match task.state {
            TaskState::Succeeded => "✅",
            TaskState::Failed => "❌",
            TaskState::Cancelled => "⏭️",
            TaskState::Running => "🔄",
            _ => "⏳",
        };

        let duration = match (task.started_at, task.completed_at) {
            (Some(start), Some(end)) => {
                let secs = (end - start).num_seconds();
                if secs >= 60 {
                    format!("{}m{}s", secs / 60, secs % 60)
                } else {
                    format!("{secs}s")
                }
            }
            _ => "-".to_string(),
        };

        lines.push(format!("{icon} **{}** ({duration})", task.id.0));

        // Show error message for failed tasks.
        if let Some(ref error) = task.error {
            lines.push(format!("```\n{error}\n```"));
        }

        // Show stdout/stderr for completed tasks.
        if let Some(ref output) = task.output {
            let stdout = output.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let stderr = output.get("stderr").and_then(|v| v.as_str()).unwrap_or("");

            if !stdout.is_empty() {
                let trimmed = truncate(stdout, 2000);
                lines.push(format!(
                    "<details><summary>stdout</summary>\n\n```\n{trimmed}\n```\n</details>"
                ));
            }
            if !stderr.is_empty() {
                let trimmed = truncate(stderr, 2000);
                lines.push(format!(
                    "<details><summary>stderr</summary>\n\n```\n{trimmed}\n```\n</details>"
                ));
            }
        }

        lines.push(String::new());
    }

    lines.join("\n")
}

/// Truncate a string to max_len, appending "..." if truncated.
fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len { s } else { &s[..max_len] }
}

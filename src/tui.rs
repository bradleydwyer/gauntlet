use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use tasked::engine::Engine;
use tasked::types::{ExecuteResult, FlowId, FlowState, Task, TaskState};

use crate::compiler::CompileMetadata;
use crate::github::{CommitState, GitHubClient};

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";

pub struct TuiConfig {
    pub verbose: bool,
    pub quiet: bool,
    pub auto_approve: bool,
    pub github: Option<GitHubClient>,
    pub github_repo: Option<(String, String)>, // (owner, repo)
    pub github_sha: Option<String>,
}

/// Run a flow with TUI output. Returns exit code.
pub async fn run_flow(
    engine: Arc<Engine>,
    flow_id: &FlowId,
    task_count: usize,
    metadata: &CompileMetadata,
    config: &TuiConfig,
) -> i32 {
    let flow_short = &flow_id.0[..8.min(flow_id.0.len())];

    if !config.quiet {
        println!("{YELLOW}▸{RESET} {DIM}Flow {flow_short} submitted ({task_count} tasks){RESET}");
    }

    // Spawn the engine loop.
    let engine_loop = engine.clone();
    let _engine_handle = tokio::spawn(async move {
        engine_loop.run().await;
    });

    let start = Instant::now();

    let initial_tasks = engine.get_flow_tasks(flow_id).await.unwrap_or_default();
    let max_len = initial_tasks
        .iter()
        .filter(|t| !config.quiet || !metadata.synthetic_tasks.contains(&t.id.0))
        .map(|t| display_name(&t.id.0, metadata).len())
        .max()
        .unwrap_or(4);

    let mut displayed: HashMap<String, &str> = HashMap::new();
    let mut running_lines: usize = 0;
    let mut approvals_handled: HashSet<String> = HashSet::new();

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let tasks = engine.get_flow_tasks(flow_id).await.unwrap_or_default();

        let mut newly_done: Vec<&Task> = Vec::new();
        let mut still_running: Vec<&Task> = Vec::new();
        let mut newly_running: Vec<&Task> = Vec::new();

        for task in &tasks {
            // Skip synthetic tasks in non-verbose mode.
            if !config.verbose && metadata.synthetic_tasks.contains(&task.id.0) {
                continue;
            }

            let prev = *displayed.get(&task.id.0).unwrap_or(&"none");
            match task.state {
                TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
                    if prev != "done" =>
                {
                    newly_done.push(task);
                }
                TaskState::Running if prev == "none" => {
                    newly_running.push(task);
                }
                TaskState::Running if prev == "running" => {
                    still_running.push(task);
                }
                _ => {}
            }
        }

        // Handle approval tasks.
        for task in &tasks {
            if task.state == TaskState::Running
                && !approvals_handled.contains(&task.id.0)
                && task
                    .output
                    .as_ref()
                    .and_then(|o| o.get("awaiting_approval"))
                    .and_then(|v| v.as_bool())
                    == Some(true)
            {
                approvals_handled.insert(task.id.0.clone());
                let message = task
                    .output
                    .as_ref()
                    .and_then(|o| o.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Approval required");

                erase_running_lines(&mut running_lines);

                let name = display_name(&task.id.0, metadata);
                let padded = format!("{name:<max_len$}");

                let approved = if config.auto_approve {
                    println!("\x1b[35m?\x1b[0m [{padded}]  {message} {DIM}(auto-approved){RESET}");
                    true
                } else {
                    print!("\x1b[35m?\x1b[0m [{padded}]  {message} {BOLD}[y/N]{RESET} ");
                    std::io::stdout().flush().unwrap();
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input).unwrap();
                    let answer = input.trim().to_lowercase();
                    answer == "y" || answer == "yes"
                };

                let result = if approved {
                    ExecuteResult::Success {
                        output: Some(serde_json::json!({"approved": true, "approved_by": "cli"})),
                    }
                } else {
                    ExecuteResult::Failed {
                        error: "rejected by user".to_string(),
                        retryable: false,
                    }
                };

                if let Err(e) = engine.handle_task_result(task, result).await {
                    eprintln!("Error handling approval: {e}");
                }
            }
        }

        if newly_done.is_empty() && newly_running.is_empty() {
            // Check flow completion.
            let flow = match engine.get_flow(flow_id).await {
                Ok(Some(f)) => f,
                Ok(None) => {
                    eprintln!("Flow disappeared unexpectedly");
                    return 1;
                }
                Err(e) => {
                    eprintln!("Error fetching flow: {e}");
                    return 1;
                }
            };

            if flow.state.is_terminal() {
                erase_running_lines(&mut running_lines);

                if !config.quiet {
                    let elapsed = format!("{:.1}s", start.elapsed().as_secs_f64());
                    println!();
                    match flow.state {
                        FlowState::Succeeded => println!(
                            "{GREEN}✓{RESET} {GREEN}{BOLD}Pipeline complete{RESET}  {DIM}{}/{} tasks succeeded ({elapsed}){RESET}",
                            flow.tasks_succeeded, flow.task_count
                        ),
                        FlowState::Failed => {
                            println!(
                                "{RED}✗{RESET} {RED}{BOLD}Pipeline failed{RESET}  {DIM}{} succeeded, {} failed ({elapsed}){RESET}",
                                flow.tasks_succeeded, flow.tasks_failed
                            );
                            // Print failure details.
                            for task in &tasks {
                                if task.state == TaskState::Failed {
                                    let name = display_name(&task.id.0, metadata);
                                    if let Some(ref error) = task.error {
                                        println!("  {RED}✗{RESET} {name}: {error}");
                                    }
                                }
                            }
                        }
                        FlowState::Cancelled => {
                            println!("{DIM}– Pipeline cancelled ({elapsed}){RESET}")
                        }
                        _ => {}
                    }
                }

                // Report final GitHub status.
                if let Some(ref github) = config.github {
                    report_github_status(github, config, flow_id, &flow.state, &tasks, metadata)
                        .await;
                }

                return match flow.state {
                    FlowState::Succeeded => 0,
                    _ => 1,
                };
            }

            continue;
        }

        if !config.quiet {
            // Erase previous running lines.
            erase_running_lines(&mut running_lines);

            // Print newly completed tasks.
            for task in &newly_done {
                let name = display_name(&task.id.0, metadata);
                let padded = format!("{name:<max_len$}");
                let matrix_ann = matrix_annotation(&task.id.0, metadata);

                match task.state {
                    TaskState::Succeeded => {
                        println!("  {GREEN}✓{RESET} [{padded}]  succeeded{matrix_ann}");
                    }
                    TaskState::Failed => {
                        let err = task.error.as_deref().unwrap_or("unknown error");
                        println!("  {RED}✗{RESET} [{padded}]  failed: {err}{matrix_ann}");
                    }
                    TaskState::Cancelled => {
                        println!("  {DIM}– [{padded}]  cancelled{matrix_ann}{RESET}");
                    }
                    _ => {}
                }
                displayed.insert(task.id.0.clone(), "done");
            }

            // Print running lines (will be erased on next iteration).
            let running: Vec<&Task> = still_running
                .iter()
                .chain(newly_running.iter())
                .copied()
                .collect();
            running_lines = running.len();

            for task in &running {
                let name = display_name(&task.id.0, metadata);
                let padded = format!("{name:<max_len$}");
                let matrix_ann = matrix_annotation(&task.id.0, metadata);
                println!("  {CYAN}●{RESET} [{padded}]  running...{matrix_ann}");
                displayed.insert(task.id.0.clone(), "running");
            }
        }
    }
}

fn erase_running_lines(count: &mut usize) {
    for _ in 0..*count {
        print!("\x1b[A\x1b[2K");
    }
    *count = 0;
}

/// Get a display name for a task, using the original ID for matrix-expanded tasks.
fn display_name(task_id: &str, metadata: &CompileMetadata) -> String {
    if let Some(origin) = metadata.task_origins.get(task_id) {
        origin.clone()
    } else {
        task_id.to_string()
    }
}

/// Get matrix annotation string for display.
fn matrix_annotation(task_id: &str, metadata: &CompileMetadata) -> String {
    if let Some(combo) = metadata.matrix_values.get(task_id) {
        let pairs: Vec<String> = {
            let mut keys: Vec<&String> = combo.keys().collect();
            keys.sort();
            keys.iter()
                .map(|k| format!("{}={}", k, combo[*k]))
                .collect()
        };
        format!("  {DIM}({})  {RESET}", pairs.join(", "))
    } else {
        String::new()
    }
}

/// Report GitHub commit status for the pipeline.
async fn report_github_status(
    github: &GitHubClient,
    config: &TuiConfig,
    _flow_id: &FlowId,
    flow_state: &FlowState,
    _tasks: &[Task],
    _metadata: &CompileMetadata,
) {
    let (owner, repo) = match config.github_repo.as_ref() {
        Some(r) => r,
        None => return,
    };
    let sha = match config.github_sha.as_ref() {
        Some(s) => s,
        None => return,
    };

    let (state, description) = match flow_state {
        FlowState::Succeeded => (CommitState::Success, "Pipeline succeeded"),
        FlowState::Failed => (CommitState::Failure, "Pipeline failed"),
        _ => (CommitState::Error, "Pipeline cancelled"),
    };

    if let Err(e) = github
        .set_commit_status(owner, repo, sha, "gauntlet", state, description, None)
        .await
    {
        eprintln!("{DIM}Warning: failed to report GitHub status: {e}{RESET}");
    }
}

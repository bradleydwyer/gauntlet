use std::sync::Arc;

use clap::{Parser, Subcommand};
use tasked::engine::{Engine, EngineConfig};
use tasked::executor::approval::ApprovalExecutor;
use tasked::executor::delay::DelayExecutor;
use tasked::executor::http::HttpExecutor;
use tasked::executor::NoopExecutor;
use tasked::executor::shell::ShellExecutor;
use tasked::executor::spawn::SpawnExecutor;
use tasked::store::memory::MemoryStorage;
use tasked::types::QueueId;

use gauntlet::compiler::{self, BuildContext};
use gauntlet::github::GitHubClient;
use gauntlet::schema::Pipeline;
use gauntlet::tui::{self, TuiConfig};

#[derive(Parser)]
#[command(name = "gauntlet", about = "CI pipeline runner powered by Tasked")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Execute a pipeline locally.
    Run {
        /// Pipeline file path.
        #[arg(default_value = ".gauntlet/pipeline.json")]
        file: String,

        /// Git ref to checkout (branch, tag, SHA).
        #[arg(long, name = "ref")]
        git_ref: Option<String>,

        /// Skip the checkout step.
        #[arg(long)]
        no_checkout: bool,

        /// Disable caching.
        #[arg(long)]
        no_cache: bool,

        /// Max parallel tasks.
        #[arg(long, default_value_t = num_cpus())]
        concurrency: u32,

        /// Only run specific tasks and their dependencies (comma-separated).
        #[arg(long)]
        filter: Option<String>,

        /// Pin a matrix dimension: KEY=VALUE.
        #[arg(long = "matrix", value_name = "KEY=VALUE")]
        matrix_pins: Vec<String>,

        /// Override or add environment variables: KEY=VALUE.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env_overrides: Vec<String>,

        /// Provide secrets: KEY=VALUE.
        #[arg(long = "secret", value_name = "KEY=VALUE")]
        secret_overrides: Vec<String>,

        /// Compile and print FlowDef JSON without executing.
        #[arg(long)]
        dry_run: bool,

        /// Auto-approve all approval tasks.
        #[arg(long)]
        auto_approve: bool,

        /// Report commit status to GitHub.
        #[arg(long)]
        github_status: bool,

        /// GitHub API token.
        #[arg(long, env = "GITHUB_TOKEN")]
        github_token: Option<String>,

        /// GitHub repository (owner/repo).
        #[arg(long, env = "GITHUB_REPOSITORY")]
        github_repo: Option<String>,

        /// Git commit SHA for GitHub status reporting.
        #[arg(long, env = "GITHUB_SHA")]
        github_sha: Option<String>,

        /// Show full output including synthetic tasks.
        #[arg(long, short)]
        verbose: bool,

        /// Only show final result.
        #[arg(long, short)]
        quiet: bool,
    },

    /// Validate a pipeline definition.
    Validate {
        /// Pipeline file path.
        #[arg(default_value = ".gauntlet/pipeline.json")]
        file: String,

        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
    },

    /// Print the pipeline JSON schema.
    Schema,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let cli = Cli::parse();

    let code = match cli.command {
        Commands::Run {
            file,
            git_ref,
            no_checkout,
            no_cache,
            concurrency,
            filter,
            matrix_pins,
            env_overrides,
            secret_overrides,
            dry_run,
            auto_approve,
            github_status,
            github_token,
            github_repo,
            github_sha,
            verbose,
            quiet,
        } => {
            run_pipeline(RunConfig {
                file,
                git_ref,
                no_checkout,
                no_cache,
                concurrency,
                filter,
                matrix_pins,
                env_overrides,
                secret_overrides,
                dry_run,
                auto_approve,
                github_status,
                github_token,
                github_repo,
                github_sha,
                verbose,
                quiet,
            })
            .await
        }
        Commands::Validate { file, format } => validate_pipeline(&file, &format),
        Commands::Schema => {
            print_schema();
            0
        }
    };

    std::process::exit(code);
}

struct RunConfig {
    file: String,
    git_ref: Option<String>,
    no_checkout: bool,
    no_cache: bool,
    concurrency: u32,
    filter: Option<String>,
    matrix_pins: Vec<String>,
    env_overrides: Vec<String>,
    secret_overrides: Vec<String>,
    dry_run: bool,
    auto_approve: bool,
    github_status: bool,
    github_token: Option<String>,
    github_repo: Option<String>,
    github_sha: Option<String>,
    verbose: bool,
    quiet: bool,
}

async fn run_pipeline(config: RunConfig) -> i32 {
    // Read pipeline file.
    let content = match std::fs::read_to_string(&config.file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error reading pipeline file '{}': {e}", config.file);
            return 2;
        }
    };

    let mut pipeline: Pipeline = match serde_json::from_str(&content) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error parsing pipeline JSON: {e}");
            return 2;
        }
    };

    // Apply CLI overrides.
    if config.no_checkout {
        pipeline.checkout = false;
    }

    // Parse env overrides.
    let mut env_overrides = std::collections::HashMap::new();
    for kv in &config.env_overrides {
        if let Some((k, v)) = kv.split_once('=') {
            env_overrides.insert(k.to_string(), v.to_string());
        }
    }

    // Build context.
    let branch = config
        .git_ref
        .clone()
        .or_else(|| git_current_branch().ok());
    let sha = config
        .github_sha
        .clone()
        .or_else(|| git_current_sha().ok());

    let ctx = BuildContext {
        repo_dir: Some(".".into()),
        git_ref: config.git_ref.clone(),
        branch,
        event: None,
        env_overrides,
    };

    // Compile.
    let result = match compiler::compile(&pipeline, &ctx) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Compilation error: {e}");
            return 2;
        }
    };

    // Dry run: just print the FlowDef.
    if config.dry_run {
        let json = serde_json::to_string_pretty(&result.flow_def).unwrap();
        println!("{json}");
        return 0;
    }

    // Create engine with in-memory storage.
    let storage: Arc<dyn tasked::store::Storage> = Arc::new(MemoryStorage::new());
    let mut engine = Engine::new(
        storage,
        EngineConfig {
            poll_interval: std::time::Duration::from_millis(100),
            ..EngineConfig::default()
        },
    );

    register_executors(&mut engine);
    let engine = Arc::new(engine);

    // Create queue.
    let queue_id = QueueId("gauntlet".into());
    let mut queue_config = result.queue_config;
    queue_config.concurrency = config.concurrency as usize;

    // Apply secret overrides from CLI.
    for kv in &config.secret_overrides {
        if let Some((k, v)) = kv.split_once('=') {
            // Inline secrets: set as env vars so Tasked's interpolation picks them up.
            let env_name = format!("GAUNTLET_SECRET_{k}");
            // SAFETY: We are single-threaded at this point (before engine starts).
            unsafe { std::env::set_var(&env_name, v) };
            let secrets = queue_config.secrets.get_or_insert_with(Default::default);
            secrets.insert(
                k.to_string(),
                tasked::types::SecretRef {
                    env: Some(env_name),
                    file: None,
                },
            );
        }
    }

    if let Err(e) = engine.create_queue(&queue_id, queue_config).await {
        eprintln!("Error creating queue: {e}");
        return 1;
    }

    // Submit flow.
    let flow = match engine.submit_flow(&queue_id, result.flow_def).await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error submitting flow: {e}");
            return 1;
        }
    };

    // GitHub client.
    let github = if config.github_status {
        config
            .github_token
            .as_ref()
            .map(|token| GitHubClient::new(token.clone()))
    } else {
        None
    };

    let github_repo = config.github_repo.as_ref().and_then(|r| {
        r.split_once('/').map(|(o, r)| (o.to_string(), r.to_string()))
    });

    let tui_config = TuiConfig {
        verbose: config.verbose,
        quiet: config.quiet,
        auto_approve: config.auto_approve,
        github,
        github_repo,
        github_sha: sha,
    };

    tui::run_flow(engine, &flow.id, flow.task_count, &result.metadata, &tui_config).await
}

fn validate_pipeline(file: &str, format: &str) -> i32 {
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error reading file '{file}': {e}");
            return 2;
        }
    };

    let pipeline: Pipeline = match serde_json::from_str(&content) {
        Ok(p) => p,
        Err(e) => {
            if format == "json" {
                println!(
                    "{}",
                    serde_json::json!({"valid": false, "error": e.to_string()})
                );
            } else {
                eprintln!("Parse error: {e}");
            }
            return 2;
        }
    };

    let ctx = BuildContext::default();
    match compiler::compile(&pipeline, &ctx) {
        Ok(result) => {
            if format == "json" {
                println!(
                    "{}",
                    serde_json::json!({
                        "valid": true,
                        "tasks": result.flow_def.tasks.len(),
                        "synthetic_tasks": result.metadata.synthetic_tasks.len(),
                    })
                );
            } else {
                println!(
                    "✓ Valid pipeline: {} tasks ({} synthetic)",
                    result.flow_def.tasks.len(),
                    result.metadata.synthetic_tasks.len(),
                );
            }
            0
        }
        Err(e) => {
            if format == "json" {
                println!(
                    "{}",
                    serde_json::json!({"valid": false, "error": e.to_string()})
                );
            } else {
                eprintln!("Validation error: {e}");
            }
            2
        }
    }
}

fn print_schema() {
    // Print a human-readable description of the pipeline format.
    // A full JSON Schema generation would use schemars crate.
    println!(
        r#"{{
  "on": ["push", "pull_request", {{"schedule": {{"cron": "..."}}}}, "manual"],
  "checkout": true,
  "checkout_config": {{"depth": 1, "submodules": false, "lfs": false}},
  "env": {{"KEY": "VALUE"}},
  "secrets": {{"NAME": {{"env": "ENV_VAR"}} | {{"file": "/path"}}}},
  "retries": 1,
  "timeout_secs": 600,
  "tasks": [
    {{
      "id": "unique-id",
      "command": "shell command",
      "executor": "shell|container|http|delay|approval|noop",
      "config": {{}},
      "container": {{"image": "...", "command": ["..."], "env": {{}}}},
      "env": {{"KEY": "VALUE"}},
      "depends_on": ["other-task-id"],
      "if": "branch == 'main'",
      "matrix": {{"dimensions": {{"key": ["val1", "val2"]}}, "exclude": []}},
      "retries": 2,
      "timeout_secs": 300,
      "cache": {{"key": "...", "paths": ["..."], "restore_keys": ["..."]}},
      "artifacts": {{"upload": ["path/..."], "download_from": ["task-id"]}},
      "spawn": false,
      "spawn_output": ["signal-id"]
    }}
  ]
}}"#
    );
}

fn register_executors(engine: &mut Engine) {
    engine.register_executor("shell", Arc::new(ShellExecutor));
    engine.register_executor("http", Arc::new(HttpExecutor::new()));
    engine.register_executor("noop", Arc::new(NoopExecutor));
    engine.register_executor("delay", Arc::new(DelayExecutor));
    engine.register_executor("approval", Arc::new(ApprovalExecutor));

    // Container executor (optional — requires Docker).
    {
        use tasked::executor::agent::AgentExecutor;
        use tasked::executor::container::{ContainerExecutor, docker::DockerBackend};

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

    // Spawn executor: registered last so it can see all other executors.
    {
        let executors = engine.executors().clone();
        engine.register_executor("spawn", Arc::new(SpawnExecutor::new(executors)));
    }
}

fn num_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
}

fn git_current_branch() -> Result<String, std::io::Error> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_current_sha() -> Result<String, std::io::Error> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

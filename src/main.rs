use std::sync::Arc;

use clap::{Parser, Subcommand};
use tasked::engine::{Engine, EngineConfig};
use tasked::executor::NoopExecutor;
use tasked::executor::approval::ApprovalExecutor;
use tasked::executor::delay::DelayExecutor;
use tasked::executor::http::HttpExecutor;
use tasked::executor::shell::ShellExecutor;
use tasked::executor::spawn::SpawnExecutor;
use tasked::store::memory::MemoryStorage;
use tasked::types::QueueId;

use gauntlet::compiler::{self, BuildContext};
use gauntlet::github::GitHubClient;
use gauntlet::schema::Pipeline;
use gauntlet::tui::{self, TuiConfig};

#[derive(Subcommand)]
enum SecretAction {
    /// Set a secret. Use --repo for repo-specific, or omit for global.
    Set {
        /// Secret name.
        name: String,
        /// Secret value.
        value: String,
        /// Repository (owner/repo). Omit for global secret.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Remove a secret.
    Remove {
        /// Secret name.
        name: String,
        /// Repository (owner/repo). Omit for global secret.
        #[arg(long)]
        repo: Option<String>,
    },
    /// List all secrets (names only, values hidden).
    List,
}

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

    /// View build logs.
    Logs {
        /// Flow ID to view logs for. If omitted, lists recent builds.
        flow_id: Option<String>,

        /// Specific step to view.
        #[arg(long)]
        step: Option<String>,

        /// Gauntlet server URL.
        #[arg(long, default_value = "http://localhost:7711")]
        server: String,
    },

    /// Manage build secrets.
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },

    /// Run the CI daemon (webhook receiver + optional poller).
    /// Reads defaults from ~/.gauntlet/config.json.
    Serve {
        /// Data directory for builds, workspaces, and state.
        #[arg(long, env = "GAUNTLET_DATA_DIR")]
        data_dir: Option<String>,

        /// GitHub App ID.
        #[arg(long, env = "GITHUB_APP_ID")]
        github_app_id: Option<u64>,

        /// Path to GitHub App private key PEM file.
        #[arg(long, env = "GITHUB_PRIVATE_KEY")]
        github_private_key: Option<String>,

        /// Port to listen on.
        #[arg(long)]
        port: Option<u16>,

        /// Webhook secret for GitHub webhook signature verification.
        /// If set, enables webhook mode (disables polling).
        #[arg(long, env = "GITHUB_WEBHOOK_SECRET")]
        webhook_secret: Option<String>,

        /// Poll interval in seconds (only used in poll mode).
        #[arg(long)]
        poll_interval: Option<u64>,

        /// Max concurrent build tasks.
        #[arg(long)]
        concurrency: Option<usize>,
    },
}

#[tokio::main]
async fn main() {
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
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "warn".into()),
                )
                .init();
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
        Commands::Logs {
            flow_id,
            step,
            server,
        } => {
            fetch_logs(&server, flow_id.as_deref(), step.as_deref()).await;
            0
        }
        Commands::Secret { action } => {
            manage_secret(action);
            0
        }
        Commands::Serve {
            data_dir,
            github_app_id,
            github_private_key,
            port,
            webhook_secret,
            poll_interval,
            concurrency,
        } => {
            // In TUI mode, send logs to a file so they don't interfere.
            // In non-TTY mode, send to stderr.
            let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
            if is_tty {
                let log_dir = dirs::home_dir().unwrap_or_default().join(".gauntlet");
                let _ = std::fs::create_dir_all(&log_dir);
                let log_file = std::fs::File::create(log_dir.join("serve.log")).unwrap();
                tracing_subscriber::fmt()
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| "gauntlet=info,tasked=info".into()),
                    )
                    .with_writer(log_file)
                    .with_ansi(false)
                    .init();
            } else {
                tracing_subscriber::fmt()
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| "gauntlet=info,tasked=info".into()),
                    )
                    .init();
            }

            // Load config file, CLI args override.
            let cfg = gauntlet::config::Config::load_default();

            let data_dir = data_dir
                .or(cfg.data_dir)
                .unwrap_or_else(|| "~/.gauntlet".to_string());
            let data_dir = shellexpand::tilde(&data_dir).to_string();

            let app_id = github_app_id.or(cfg.github_app_id).unwrap_or_else(|| {
                eprintln!("error: --github-app-id is required (or set in ~/.gauntlet/config.json)");
                std::process::exit(1);
            });

            let private_key_path = github_private_key
                .or(cfg.github_private_key)
                .unwrap_or_else(|| {
                    eprintln!("error: --github-private-key is required (or set in ~/.gauntlet/config.json)");
                    std::process::exit(1);
                });
            let private_key = shellexpand::tilde(&private_key_path).to_string();

            let webhook_secret = webhook_secret.or(cfg.webhook_secret);
            let port = port.or(cfg.port).unwrap_or(7711);
            let poll_interval = poll_interval.or(cfg.poll_interval_secs).unwrap_or(30);
            let concurrency = concurrency.or(cfg.concurrency).unwrap_or(8);

            let github_app = match gauntlet::github_app::GitHubApp::from_pem_file(
                app_id,
                std::path::Path::new(&private_key),
            ) {
                Ok(app) => std::sync::Arc::new(app),
                Err(e) => {
                    eprintln!("Failed to load GitHub App key: {e}");
                    std::process::exit(1);
                }
            };

            gauntlet::serve::run(gauntlet::serve::ServeConfig {
                port,
                data_dir: std::path::PathBuf::from(&data_dir),
                github_app,
                webhook_secret,
                poll_interval_secs: poll_interval,
                concurrency,
                config: gauntlet::config::Config::load_default(),
            })
            .await;

            0
        }
    };

    std::process::exit(code);
}

#[allow(dead_code)]
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
        pipeline.checkout = gauntlet::schema::CheckoutSetting::Enabled(false);
    }

    // Parse env overrides.
    let mut env_overrides = std::collections::HashMap::new();
    for kv in &config.env_overrides {
        if let Some((k, v)) = kv.split_once('=') {
            env_overrides.insert(k.to_string(), v.to_string());
        }
    }

    // Build context.
    let branch = config.git_ref.clone().or_else(|| git_current_branch().ok());
    let sha = config.github_sha.clone().or_else(|| git_current_sha().ok());

    let ctx = BuildContext {
        repo_dir: Some(
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        ),
        git_ref: config.git_ref.clone(),
        branch,
        event: None,
        env_overrides,
        github_token: None,
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
        r.split_once('/')
            .map(|(o, r)| (o.to_string(), r.to_string()))
    });

    let tui_config = TuiConfig {
        verbose: config.verbose,
        quiet: config.quiet,
        auto_approve: config.auto_approve,
        github,
        github_repo,
        github_sha: sha,
    };

    tui::run_flow(
        engine,
        &flow.id,
        flow.task_count,
        &result.metadata,
        &tui_config,
    )
    .await
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

async fn fetch_logs(server: &str, flow_id: Option<&str>, step: Option<&str>) {
    let client = reqwest::Client::new();

    match (flow_id, step) {
        (Some(fid), Some(s)) => {
            // Fetch specific step log.
            let url = format!("{server}/builds/{fid}/logs/{s}");
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    if let Some(stdout) = body.get("stdout").and_then(|v| v.as_str())
                        && !stdout.is_empty()
                    {
                        println!("{stdout}");
                    }
                    if let Some(stderr) = body.get("stderr").and_then(|v| v.as_str())
                        && !stderr.is_empty()
                    {
                        eprintln!("{stderr}");
                    }
                    if let Some(error) = body.get("error").and_then(|v| v.as_str()) {
                        eprintln!("\x1b[31mERROR: {error}\x1b[0m");
                    }
                }
                Ok(resp) => eprintln!("error: {}", resp.status()),
                Err(e) => eprintln!("error: {e}"),
            }
        }
        (Some(fid), None) => {
            // Fetch all step logs for a build.
            let url = format!("{server}/builds/{fid}/logs");
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    if let Some(tasks) = body.get("tasks").and_then(|v| v.as_array()) {
                        for task in tasks {
                            let step_name =
                                task.get("step").and_then(|v| v.as_str()).unwrap_or("?");
                            let state = task.get("state").and_then(|v| v.as_str()).unwrap_or("?");
                            println!("\x1b[1m--- {step_name} ({state}) ---\x1b[0m");
                            if let Some(stdout) = task.get("stdout").and_then(|v| v.as_str())
                                && !stdout.is_empty()
                            {
                                println!("{stdout}");
                            }
                            if let Some(stderr) = task.get("stderr").and_then(|v| v.as_str())
                                && !stderr.is_empty()
                            {
                                eprintln!("{stderr}");
                            }
                            if let Some(error) = task.get("error").and_then(|v| v.as_str()) {
                                eprintln!("\x1b[31mERROR: {error}\x1b[0m");
                            }
                        }
                    }
                }
                Ok(resp) => eprintln!("error: {}", resp.status()),
                Err(e) => eprintln!("error: {e}"),
            }
        }
        (None, _) => {
            // List recent builds from status endpoint.
            let url = format!("{server}/status");
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&body).unwrap_or_default()
                    );

                    // Also list disk logs.
                    let logs_dir = dirs::home_dir().unwrap_or_default().join(".gauntlet/logs");
                    if logs_dir.exists() {
                        println!("\nRecent builds (on disk):");
                        let mut entries: Vec<_> = std::fs::read_dir(&logs_dir)
                            .into_iter()
                            .flatten()
                            .flatten()
                            .collect();
                        entries.sort_by_key(|e| {
                            std::cmp::Reverse(e.metadata().ok().and_then(|m| m.modified().ok()))
                        });
                        for entry in entries.iter().take(10) {
                            let name = entry.file_name();
                            let summary_path = entry.path().join("summary.txt");
                            let summary =
                                std::fs::read_to_string(&summary_path).unwrap_or_default();
                            let first_line = summary.lines().next().unwrap_or("");
                            println!("  {} — {first_line}", name.to_string_lossy());
                        }
                    }
                }
                Ok(resp) => eprintln!("error: {}", resp.status()),
                Err(e) => eprintln!("error: {e} (is gauntlet serve running?)"),
            }
        }
    }
}

fn manage_secret(action: SecretAction) {
    let config_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".gauntlet/config.json");

    // Read existing config as raw JSON (preserve unknown fields).
    let mut config: serde_json::Value = if config_path.exists() {
        let content = std::fs::read_to_string(&config_path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let secrets = config
        .as_object_mut()
        .unwrap()
        .entry("secrets")
        .or_insert(serde_json::json!({}));

    match action {
        SecretAction::Set { name, value, repo } => {
            let scope = repo.as_deref().unwrap_or("*");
            let scope_map = secrets
                .as_object_mut()
                .unwrap()
                .entry(scope)
                .or_insert(serde_json::json!({}));
            scope_map
                .as_object_mut()
                .unwrap()
                .insert(name.clone(), serde_json::json!(value));
            println!("set {name} for {scope}");
        }
        SecretAction::Remove { name, repo } => {
            let scope = repo.as_deref().unwrap_or("*");
            if let Some(scope_map) = secrets.get_mut(scope)
                && let Some(obj) = scope_map.as_object_mut()
            {
                obj.remove(&name);
                println!("removed {name} from {scope}");
            }
        }
        SecretAction::List => {
            if let Some(obj) = secrets.as_object() {
                for (scope, scope_secrets) in obj {
                    if let Some(keys) = scope_secrets.as_object() {
                        for key in keys.keys() {
                            println!("{scope}: {key} = ****");
                        }
                    }
                }
            }
            if secrets.as_object().is_none_or(|o| o.is_empty()) {
                println!("no secrets configured");
            }
            return; // Don't write config for list.
        }
    }

    // Write config back.
    let json = serde_json::to_string_pretty(&config).unwrap();
    std::fs::write(&config_path, json).unwrap();
}

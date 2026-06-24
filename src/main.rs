//! Recalld entry point.
//!
//! Responsibilities:
//! 1. Parse CLI arguments
//! 2. Load and validate configuration
//! 3. Initialize tracing
//! 4. Dispatch to the requested subcommand (serve / mcp / daemon)

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use recalld::config::{RecalldConfig, loader};
use recalld::{Recalld, RecalldError};

/// Recalld — AI memory system with biologically-inspired decay
#[derive(Parser, Debug)]
#[command(name = "recalld", version, about)]
struct Cli {
    /// Path to configuration file (searches ./recalld.toml then ~/.recalld/config.toml)
    #[arg(short, long, global = true)]
    config: Option<std::path::PathBuf>,

    /// Override the data directory
    #[arg(short, long, env = "RECALLD_DATA_DIR", global = true)]
    data_dir: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the HTTP API server (default)
    Serve {
        /// Override the listen address
        #[arg(short, long, env = "RECALLD_BIND", default_value = "127.0.0.1:7680")]
        bind: std::net::SocketAddr,

        /// Log level filter (e.g., "info", "recalld=debug,tower_http=info")
        #[arg(short, long, env = "RECALLD_LOG_LEVEL")]
        log_level: Option<String>,

        /// Output logs as JSON (for structured log aggregation)
        #[arg(long)]
        log_json: bool,

        /// Override the server port
        #[arg(short, long, env = "RECALLD_PORT")]
        port: Option<u16>,
    },

    /// Run as an MCP server over stdio (for Claude Code, Cursor, etc.)
    Mcp {
        /// Log level filter (logs go to stderr, never stdout)
        #[arg(short, long, env = "RECALLD_LOG_LEVEL")]
        log_level: Option<String>,
    },

    /// Manage the Recalld daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Run external benchmarks (LoCoMo, etc.)
    #[cfg(feature = "bench")]
    Bench {
        #[command(subcommand)]
        target: BenchTarget,

        /// Output format: "human" or "json"
        #[arg(long, default_value = "human", global = true)]
        format: String,
    },
}

#[derive(Subcommand, Debug)]
enum DaemonAction {
    /// Start the daemon
    Start {
        /// Run in the foreground (default: runs in background)
        #[arg(long)]
        foreground: bool,

        /// Idle timeout in minutes before auto-shutdown (0 = no timeout)
        #[arg(long, default_value = "30", env = "RECALLD_DAEMON_IDLE_TIMEOUT")]
        idle_timeout: u64,

        /// Log level filter
        #[arg(short, long, env = "RECALLD_LOG_LEVEL")]
        log_level: Option<String>,
    },

    /// Stop a running daemon
    Stop,

    /// Check daemon status
    Status,
}

#[cfg(feature = "bench")]
#[derive(Subcommand, Debug)]
enum BenchTarget {
    /// Run the LoCoMo benchmark (long-term conversational memory QA)
    Locomo {
        /// Path to the locomo10.json dataset file
        #[arg(long, default_value = "src/bench/data/locomo10.json")]
        data: std::path::PathBuf,

        /// Number of retrieved memories per question
        #[arg(long, default_value = "20")]
        top_k: usize,

        /// Model name for answer generation and enrichment
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,

        /// Model name for answer judging (uses a separate model to avoid self-grading bias)
        #[arg(long, default_value = "claude-haiku-4-5")]
        judge_model: String,

        /// OpenAI-compatible LLM server URL (e.g. http://host:8000).
        /// If set, uses this instead of Claude via Vertex/Anthropic.
        #[arg(long)]
        llm_url: Option<String>,

        /// Skip adversarial questions (category 5) for comparison with
        /// systems like Mem0 that exclude them from reported scores.
        #[arg(long)]
        skip_adversarial: bool,

        /// Run retrieval diagnostics only (no LLM calls for QA). Checks
        /// whether gold answer key terms appear in retrieved memories.
        #[arg(long)]
        diagnose: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command.as_ref().unwrap_or(&Command::Serve {
        bind: "127.0.0.1:7680".parse().unwrap(),
        log_level: None,
        log_json: false,
        port: None,
    }) {
        Command::Serve {
            log_level,
            log_json,
            ..
        } => {
            let level = log_level.as_deref().unwrap_or("info");
            if let Err(e) = init_tracing(level, *log_json, false) {
                eprintln!("fatal: failed to initialize tracing: {e}");
                return ExitCode::FAILURE;
            }
        }
        Command::Mcp { log_level } => {
            let level = log_level.as_deref().unwrap_or("info");
            if let Err(e) = init_tracing(level, false, true) {
                eprintln!("fatal: failed to initialize tracing: {e}");
                return ExitCode::FAILURE;
            }
        }
        Command::Daemon { action } => {
            let level = match action {
                DaemonAction::Start { log_level, .. } => log_level.as_deref().unwrap_or("info"),
                _ => "info",
            };
            let stderr_only = matches!(
                action,
                DaemonAction::Start {
                    foreground: true,
                    ..
                }
            );
            if let Err(e) = init_tracing(level, false, stderr_only) {
                eprintln!("fatal: failed to initialize tracing: {e}");
                return ExitCode::FAILURE;
            }
        }
        #[cfg(feature = "bench")]
        Command::Bench { .. } => {}
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("recalld-worker")
        .build()
        .unwrap_or_else(|e| {
            tracing::error!(%e, "failed to build tokio runtime");
            std::process::exit(1);
        });

    runtime.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> ExitCode {
    let config = match load_config(&cli) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(%e, "configuration error");
            return ExitCode::FAILURE;
        }
    };

    match cli.command.unwrap_or(Command::Serve {
        bind: "127.0.0.1:7680".parse().unwrap(),
        log_level: None,
        log_json: false,
        port: None,
    }) {
        Command::Serve { bind, port, .. } => run_serve(config, bind, port).await,
        Command::Mcp { .. } => run_mcp(config).await,
        Command::Daemon { action } => run_daemon(config, action).await,
        #[cfg(feature = "bench")]
        Command::Bench { target, format } => run_bench(config, target, &format).await,
    }
}

#[cfg(feature = "bench")]
async fn run_bench(config: RecalldConfig, target: BenchTarget, format: &str) -> ExitCode {
    match target {
        BenchTarget::Locomo {
            data,
            top_k,
            model,
            judge_model,
            llm_url,
            skip_adversarial,
            diagnose,
        } => {
            if !data.exists() {
                eprintln!("error: dataset not found: {}", data.display());
                eprintln!("  Download it with:");
                eprintln!(
                    "  curl -L https://github.com/snap-research/locomo/raw/refs/heads/main/data/locomo10.json -o locomo10.json"
                );
                return ExitCode::FAILURE;
            }
            if diagnose {
                let llm =
                    match recalld::bench::claude::LlmClient::new(model.clone(), llm_url.as_deref())
                    {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("LLM backend required for benchmark: {e}");
                            return ExitCode::FAILURE;
                        }
                    };
                match recalld::bench::locomo::run_diagnose(
                    &config,
                    &data,
                    top_k,
                    skip_adversarial,
                    &llm,
                )
                .await
                {
                    Ok(()) => return ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("diagnostic error: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            match recalld::bench::locomo::run(
                &config,
                &data,
                top_k,
                &model,
                &judge_model,
                llm_url.as_deref(),
                format,
                skip_adversarial,
            )
            .await
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("benchmark error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

async fn run_serve(
    config: RecalldConfig,
    bind: std::net::SocketAddr,
    port: Option<u16>,
) -> ExitCode {
    let bind = if let Some(p) = port {
        std::net::SocketAddr::new(bind.ip(), p)
    } else {
        bind
    };

    let system = match Recalld::new(config).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(%e, "startup failed");
            return ExitCode::FAILURE;
        }
    };

    // Build AppState from Recalld subsystems via API adapters.
    let api_config = recalld::api::ApiConfig {
        bind_address: bind.ip().to_string(),
        port: bind.port(),
        ..recalld::api::ApiConfig::default()
    };

    let app_state = {
        use recalld::api::adapters::*;

        let search: Arc<dyn recalld::api::SearchPipeline> = Arc::new(SearchPipelineAdapter::new(
            system.query_engine().clone(),
            system.embedding().clone(),
            system.vector_index().clone(),
        ));
        let storage: Arc<dyn recalld::api::StorageEngine> = Arc::new(StorageEngineAdapter::new(
            system.storage().clone(),
            system.cache().clone(),
            system.embedding().clone(),
        ));
        let cache: Arc<dyn recalld::api::RecordCache> = Arc::new(RecordCacheAdapter::new(
            system.cache().clone(),
            system.storage().clone(),
        ));
        let graph: Arc<dyn recalld::api::RelationshipGraph> =
            Arc::new(RelationshipGraphAdapter::new(system.graph().clone()));
        let decay: Arc<dyn recalld::api::FsrsEngine> =
            Arc::new(FsrsEngineAdapter::new(system.storage().clone(), true));
        let namespaces: Arc<dyn recalld::api::NamespaceRegistry> =
            Arc::new(NamespaceRegistryAdapter::new(system.storage().clone()));
        let metrics: Arc<dyn recalld::api::MetricsCollector> = Arc::new(NoopMetricsCollector);

        recalld::api::AppState::new(search, storage, cache, graph, decay, namespaces, metrics)
    };

    // Start the API server (blocks until shutdown signal).
    match recalld::api::serve(app_state, api_config).await {
        Ok(()) => {
            tracing::info!("API server exited cleanly");
        }
        Err(e) => {
            tracing::error!(%e, "API server error");
            return ExitCode::FAILURE;
        }
    }

    // Run Recalld shutdown sequence.
    match system.serve(bind).await {
        Ok(()) => {
            tracing::info!("recalld exited cleanly");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(%e, "shutdown error");
            ExitCode::FAILURE
        }
    }
}

async fn run_mcp(config: RecalldConfig) -> ExitCode {
    tracing::info!("Recalld MCP server starting (stdio)");

    let socket = recalld::daemon::socket_path();

    let bridge = match try_daemon_connection(&socket).await {
        Ok(bridge) => {
            tracing::info!(socket = %socket.display(), "connected to daemon");
            bridge
        }
        Err(_) => match auto_start_daemon(&config, &socket).await {
            Ok(bridge) => {
                tracing::info!("auto-started daemon, connected");
                bridge
            }
            Err(e) => {
                tracing::warn!(%e, "daemon unavailable, falling back to direct mode");
                return run_mcp_direct(config).await;
            }
        },
    };

    let handler: Arc<dyn recalld::mcp::McpHandler> = Arc::new(bridge);
    let server = Arc::new(tokio::sync::Mutex::new(recalld::mcp::McpServer::new(
        handler,
    )));

    match recalld::mcp::run_stdio(server).await {
        Ok(()) => {
            tracing::info!("MCP server exited cleanly");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(%e, "MCP server error");
            ExitCode::FAILURE
        }
    }
}

async fn run_mcp_direct(config: RecalldConfig) -> ExitCode {
    let system = match Recalld::new(config).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(%e, "startup failed");
            return ExitCode::FAILURE;
        }
    };

    let bridge = {
        use recalld::mcp::bridge_adapters::*;

        let search: Arc<dyn recalld::mcp::bridge::SearchPipeline> =
            Arc::new(McpSearchAdapter::new(
                system.query_engine().clone(),
                system.embedding().clone(),
                system.storage().clone(),
                system.graph().clone(),
            ));
        let storage: Arc<dyn recalld::mcp::bridge::StorageEngine> =
            Arc::new(McpStorageAdapter::new(
                system.storage().clone(),
                system.cache().clone(),
                system.embedding().clone(),
                system.vector_index().clone(),
                system.fts_index().clone(),
                system.entity_index().clone(),
                system.graph().clone(),
                std::sync::Arc::new(system.config().clone()),
            ));
        let namespaces: Arc<dyn recalld::mcp::bridge::NamespaceRegistry> =
            Arc::new(McpNamespaceAdapter::new(system.storage().clone()));
        let health: Arc<dyn recalld::mcp::bridge::HealthChecker> =
            Arc::new(McpHealthAdapter::new(system.storage().clone()));

        recalld::mcp::bridge::McpBridge {
            search,
            storage,
            namespaces,
            health,
        }
    };

    let handler: Arc<dyn recalld::mcp::McpHandler> = Arc::new(bridge);
    let server = Arc::new(tokio::sync::Mutex::new(recalld::mcp::McpServer::new(
        handler,
    )));

    match recalld::mcp::run_stdio(server).await {
        Ok(()) => {
            tracing::info!("MCP server exited cleanly");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(%e, "MCP server error");
            ExitCode::FAILURE
        }
    }
}

async fn try_daemon_connection(
    socket: &Path,
) -> Result<recalld::mcp::bridge::McpBridge, Box<dyn std::error::Error>> {
    use recalld::daemon::bridge_adapters::*;

    let client = Arc::new(recalld::daemon::DaemonClient::connect(socket).await?);
    client.ping().await?;

    Ok(recalld::mcp::bridge::McpBridge {
        search: Arc::new(RemoteSearchAdapter::new(client.clone())),
        storage: Arc::new(RemoteStorageAdapter::new(client.clone())),
        namespaces: Arc::new(RemoteNamespaceAdapter::new(client.clone())),
        health: Arc::new(RemoteHealthAdapter::new(client.clone())),
    })
}

async fn auto_start_daemon(
    config: &RecalldConfig,
    socket: &Path,
) -> Result<recalld::mcp::bridge::McpBridge, Box<dyn std::error::Error>> {
    use std::process::Command as StdCommand;

    let exe = std::env::current_exe()?;

    let log_dir = socket.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(log_dir)?;
    let log_file = std::fs::File::create(log_dir.join("daemon.log"))?;
    let stderr_log = log_file.try_clone()?;

    let mut cmd = StdCommand::new(&exe);
    cmd.arg("daemon").arg("start").arg("--foreground");

    // Forward config path if it was explicitly set
    if let Ok(config_path) = std::env::var("RECALLD_CONFIG") {
        cmd.arg("--config").arg(config_path);
    }
    if !config.storage.data_dir.is_empty() {
        cmd.arg("--data-dir").arg(&config.storage.data_dir);
    }

    cmd.stdout(log_file).stderr(stderr_log);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    cmd.spawn()?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if tokio::time::Instant::now() > deadline {
            return Err("daemon did not start within 10 seconds".into());
        }
        if let Ok(client) = recalld::daemon::DaemonClient::connect(socket).await {
            let client = Arc::new(client);
            if client.ping().await.is_ok() {
                use recalld::daemon::bridge_adapters::*;
                return Ok(recalld::mcp::bridge::McpBridge {
                    search: Arc::new(RemoteSearchAdapter::new(client.clone())),
                    storage: Arc::new(RemoteStorageAdapter::new(client.clone())),
                    namespaces: Arc::new(RemoteNamespaceAdapter::new(client.clone())),
                    health: Arc::new(RemoteHealthAdapter::new(client.clone())),
                });
            }
        }
    }
}

// ── Daemon commands ─────────────────────────────────────────────────

async fn run_daemon(config: RecalldConfig, action: DaemonAction) -> ExitCode {
    match action {
        DaemonAction::Start {
            foreground,
            idle_timeout,
            ..
        } => {
            let timeout = if idle_timeout == 0 {
                Duration::from_secs(u64::MAX / 2)
            } else {
                Duration::from_secs(idle_timeout * 60)
            };
            if foreground {
                run_daemon_foreground(config, timeout).await
            } else {
                run_daemon_background(&config).await
            }
        }
        DaemonAction::Stop => run_daemon_stop().await,
        DaemonAction::Status => run_daemon_status().await,
    }
}

async fn run_daemon_foreground(config: RecalldConfig, idle_timeout: Duration) -> ExitCode {
    let socket = recalld::daemon::socket_path();

    if recalld::daemon::is_daemon_alive(&socket).unwrap_or(false) {
        tracing::error!(socket = %socket.display(), "daemon already running");
        return ExitCode::FAILURE;
    }

    let system = match Recalld::new(config).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(%e, "startup failed");
            return ExitCode::FAILURE;
        }
    };

    let server = recalld::daemon::DaemonServer::new(system);

    match server.run(&socket, idle_timeout).await {
        Ok(()) => {
            tracing::info!("daemon exited cleanly");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(%e, "daemon error");
            ExitCode::FAILURE
        }
    }
}

async fn run_daemon_background(config: &RecalldConfig) -> ExitCode {
    let socket = recalld::daemon::socket_path();

    if recalld::daemon::is_daemon_alive(&socket).unwrap_or(false) {
        eprintln!("daemon already running at {}", socket.display());
        return ExitCode::FAILURE;
    }

    match auto_start_daemon(config, &socket).await {
        Ok(_bridge) => {
            let pid = recalld::daemon::lifecycle::read_pid_file(&recalld::daemon::pid_path())
                .ok()
                .flatten()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".into());
            eprintln!("daemon started (pid: {pid}, socket: {})", socket.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to start daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_daemon_stop() -> ExitCode {
    let socket = recalld::daemon::socket_path();

    if !recalld::daemon::is_daemon_alive(&socket).unwrap_or(false) {
        eprintln!("no daemon running");
        return ExitCode::FAILURE;
    }

    match recalld::daemon::DaemonClient::connect(&socket).await {
        Ok(client) => match client.call("shutdown", serde_json::json!({})).await {
            Ok(_) => {
                eprintln!("daemon shutting down...");
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                while socket.exists() && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                if socket.exists() {
                    eprintln!("warning: daemon may not have shut down cleanly");
                    ExitCode::FAILURE
                } else {
                    eprintln!("daemon stopped");
                    ExitCode::SUCCESS
                }
            }
            Err(e) => {
                eprintln!("failed to send shutdown: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("failed to connect to daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_daemon_status() -> ExitCode {
    let socket = recalld::daemon::socket_path();
    let pid_path = recalld::daemon::pid_path();

    if !recalld::daemon::is_daemon_alive(&socket).unwrap_or(false) {
        eprintln!("daemon is not running");
        if socket.exists() {
            eprintln!("  stale socket: {}", socket.display());
        }
        return ExitCode::FAILURE;
    }

    let pid = recalld::daemon::lifecycle::read_pid_file(&pid_path)
        .ok()
        .flatten()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "unknown".into());

    match recalld::daemon::DaemonClient::connect(&socket).await {
        Ok(client) => match client.call("check_health", serde_json::json!({})).await {
            Ok(health) => {
                eprintln!("daemon is running");
                eprintln!("  pid: {pid}");
                eprintln!("  socket: {}", socket.display());
                if let Ok(status) =
                    serde_json::from_value::<recalld::mcp::bridge::HealthStatus>(health)
                {
                    eprintln!("  status: {}", status.status);
                    eprintln!("  uptime: {}s", status.uptime_secs);
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("daemon is running but health check failed: {e}");
                ExitCode::from(2)
            }
        },
        Err(e) => {
            eprintln!("daemon socket exists but connection failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn load_config(cli: &Cli) -> std::result::Result<RecalldConfig, RecalldError> {
    let cli_overrides = loader::CliOverrides {
        config_path: cli.config.clone(),
        data_dir: cli.data_dir.clone(),
        ..Default::default()
    };

    let config_path = cli.config.as_deref();

    let config = loader::load_config(config_path, &cli_overrides).map_err(|errors| {
        let message = errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        RecalldError::Init {
            step: "load_config",
            message,
            source: None,
        }
    })?;

    Ok(config)
}

/// Initialize the tracing subscriber.
///
/// When `stderr_only` is true (MCP mode), all output goes to stderr
/// because stdout is the protocol channel.
fn init_tracing(
    filter: &str,
    json: bool,
    stderr_only: bool,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let env_filter = EnvFilter::try_new(filter)?;

    if stderr_only {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().with_writer(std::io::stderr).with_target(true))
            .init();
    } else if json {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().json())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().with_target(true).with_thread_ids(false))
            .init();
    }

    Ok(())
}

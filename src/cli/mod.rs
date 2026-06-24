//! Recalld CLI — LLM-first command-line interface.
//!
//! Talks to a running Recalld API server over HTTP.
//! Default output is JSON (machine-readable). Use `--format human`
//! for colored tables during debugging.

mod client;
mod commands;
mod config;
mod import;
mod output;

pub use client::RecalldClient;
pub use commands::Cli;
pub use commands::OutputFormat;
pub use config::CliConfig;
pub use output::{HumanFormatter, JsonFormatter, OutputFormatter};

use std::io::{self, Write};

use thiserror::Error;

use commands::{
    Command, ExportArgs, ExportFormat, ForgetArgs, GetArgs, HealthArgs, ImportArgs, InspectArgs,
    ListArgs, NamespaceAction, NamespacesArgs, RecallArgs, ReinforceArgs, StoreArgs, SweepArgs,
};

/// Errors that can occur during CLI operations.
#[derive(Debug, Error)]
pub enum CliError {
    /// An HTTP transport error (connection refused, timeout, etc.).
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The API server returned a non-2xx status code.
    #[error("API returned error {status}: {body}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Response body text.
        body: String,
    },

    /// Failed to parse the API response as expected JSON.
    #[error("failed to parse API response: {0}")]
    Parse(#[from] serde_json::Error),

    /// A configuration error (malformed config file, etc.).
    #[error("configuration error: {0}")]
    Config(String),

    /// An invalid UUID was provided.
    #[error("invalid UUID: {0}")]
    InvalidId(#[from] uuid::Error),

    /// A catch-all for other errors.
    #[error("{0}")]
    Other(String),
}

/// Convenience alias for CLI operations.
pub type Result<T> = std::result::Result<T, CliError>;

/// Run the CLI with parsed arguments and loaded config.
///
/// This is the main dispatch function called from `src/bin/recalld.rs`.
/// It creates the HTTP client, selects the output formatter, and
/// dispatches to the appropriate command handler.
pub async fn run(cli: Cli, config: CliConfig) -> Result<()> {
    let client = RecalldClient::new(&config.server_url)?;
    let format = cli.format.clone().unwrap_or(config.default_format.clone());
    let formatter: Box<dyn OutputFormatter> = match format {
        OutputFormat::Json => Box::new(JsonFormatter),
        OutputFormat::Human => Box::new(HumanFormatter),
    };

    match cli.command {
        Command::Store(args) => cmd_store(&client, &*formatter, args).await,
        Command::Recall(args) => cmd_recall(&client, &*formatter, args).await,
        Command::Get(args) => cmd_get(&client, &*formatter, args).await,
        Command::Forget(args) => cmd_forget(&client, &*formatter, args).await,
        Command::Reinforce(args) => cmd_reinforce(&client, &*formatter, args).await,
        Command::Inspect(args) => cmd_inspect(&client, &*formatter, args).await,
        Command::Namespaces(args) => cmd_namespaces(&client, &*formatter, args).await,
        Command::Sweep(args) => cmd_sweep(&client, &*formatter, args).await,
        Command::Status => cmd_status(&client, &*formatter).await,
        Command::Export(args) => cmd_export(&client, &*formatter, args).await,
        Command::List(args) => cmd_list(&client, &*formatter, args).await,
        Command::Import(args) => cmd_import(&client, &*formatter, args, &config).await,
        Command::Health(args) => {
            // Health command may override the global format flag
            let health_fmt: Box<dyn OutputFormatter> = match args.format.clone().or(cli.format.clone()) {
                Some(OutputFormat::Human) => Box::new(HumanFormatter),
                _ => Box::new(JsonFormatter),
            };
            cmd_health(&client, &*health_fmt, args).await
        }
    }
}

/// Print formatted output to stdout.
pub(crate) fn print_out(s: &str) {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(s.as_bytes()).ok();
    handle.flush().ok();
}

/// Print formatted error to stderr.
pub(crate) fn print_err(s: &str) {
    let stderr = io::stderr();
    let mut handle = stderr.lock();
    handle.write_all(s.as_bytes()).ok();
    handle.flush().ok();
}

// ── Command Handlers ──────────────────────────────────────────────

/// Handle the `store` command: send memory content to the API.
async fn cmd_store(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: StoreArgs,
) -> Result<()> {
    let namespace = args.namespace.as_deref();
    let result = client
        .store_memory(&args.text, &args.tags, namespace, args.parent_id.as_ref())
        .await?;
    print_out(&fmt.store(&result));
    Ok(())
}

/// Handle the `recall` command: search memories by query.
async fn cmd_recall(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: RecallArgs,
) -> Result<()> {
    let result = client
        .search_memories(
            &args.query,
            args.limit,
            args.namespace.as_deref(),
            args.include_ghosts,
            &args.tags,
            args.depth,
            args.min_strength,
        )
        .await?;
    print_out(&fmt.recall(&result));
    Ok(())
}

/// Handle the `get` command: retrieve a memory by ID.
async fn cmd_get(client: &RecalldClient, fmt: &dyn OutputFormatter, args: GetArgs) -> Result<()> {
    let memory = client.get_memory(&args.id).await?;
    print_out(&fmt.get(&memory));
    Ok(())
}

/// Handle the `forget` command: delete a memory by ID.
///
/// In interactive mode, prompts for confirmation unless `--yes` is set.
async fn cmd_forget(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: ForgetArgs,
) -> Result<()> {
    // In human mode, ask for confirmation unless --yes is set.
    if !args.yes {
        // Fetch the memory first to show what will be deleted.
        if let Ok(memory) = client.get_memory(&args.id).await {
            eprint!(
                "Delete memory {}? (summary: \"{}\")\nConfirm [y/N]: ",
                args.id,
                &memory.summary[..memory.summary.len().min(80)]
            );
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).ok();
            if !input.trim().eq_ignore_ascii_case("y") {
                eprintln!("Aborted.");
                return Ok(());
            }
        }
    }
    let result = client.delete_memory(&args.id).await?;
    print_out(&fmt.forget(&result));
    Ok(())
}

/// Handle the `reinforce` command: manually reinforce a memory.
async fn cmd_reinforce(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: ReinforceArgs,
) -> Result<()> {
    let result = client.reinforce_memory(&args.id).await?;
    print_out(&fmt.reinforce(&result));
    Ok(())
}

/// Handle the `inspect` command: show full debug view of a memory.
async fn cmd_inspect(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: InspectArgs,
) -> Result<()> {
    let view = client.inspect_memory(&args.id).await?;
    print_out(&fmt.inspect(&view));
    Ok(())
}

/// Handle the `namespaces` command group: list, create, or stats.
async fn cmd_namespaces(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: NamespacesArgs,
) -> Result<()> {
    match args.action {
        NamespaceAction::List => {
            let namespaces = client.list_namespaces().await?;
            print_out(&fmt.namespaces_list(&namespaces));
        }
        NamespaceAction::Create(create_args) => {
            let ns = client
                .create_namespace(
                    &create_args.name,
                    create_args.dim,
                    create_args.initial_stability,
                )
                .await?;
            print_out(&fmt.namespaces_list(&[ns]));
        }
        NamespaceAction::Stats(stats_args) => {
            let stats = client.namespace_stats(stats_args.name.as_deref()).await?;
            print_out(&fmt.namespace_stats(&stats));
        }
    }
    Ok(())
}

/// Handle the `sweep` command: trigger a decay sweep.
async fn cmd_sweep(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: SweepArgs,
) -> Result<()> {
    let result = client
        .sweep(args.dry_run, args.namespace.as_deref())
        .await?;
    print_out(&fmt.sweep(&result));
    Ok(())
}

/// Handle the `status` command: show system health.
async fn cmd_status(client: &RecalldClient, fmt: &dyn OutputFormatter) -> Result<()> {
    let status = client.status().await?;
    print_out(&fmt.status(&status));
    Ok(())
}

/// Handle the `export` command: bulk export memories.
async fn cmd_export(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: ExportArgs,
) -> Result<()> {
    let memories = client
        .export(
            args.namespace.as_deref(),
            args.include_text,
            args.include_embeddings,
        )
        .await?;

    match args.export_format {
        ExportFormat::Json => {
            // For JSON format, output as a single array.
            let json = serde_json::to_string(&memories)
                .map_err(|e| CliError::Other(format!("failed to serialize export: {e}")))?;
            print_out(&json);
            print_out("\n");
        }
        ExportFormat::Jsonl => {
            // For JSONL, one record per line.
            for memory in &memories {
                print_out(&fmt.export_record(memory));
                print_out("\n");
            }
        }
    }
    Ok(())
}

/// Handle the `list` command: list memories with filters.
async fn cmd_list(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: ListArgs,
) -> Result<()> {
    let sort = match args.sort {
        commands::SortField::Created => "created",
        commands::SortField::Accessed => "accessed",
        commands::SortField::Strength => "strength",
        commands::SortField::Stability => "stability",
    };
    let order = match args.order {
        commands::SortOrder::Asc => "asc",
        commands::SortOrder::Desc => "desc",
    };

    let result = client
        .list_memories(
            args.namespace.as_deref(),
            args.phase.as_deref(),
            &args.tags,
            sort,
            order,
            args.limit,
            args.offset,
        )
        .await?;

    print_out(&fmt.list(&result));
    Ok(())
}

/// Handle the `import` command: delegates to [`import::cmd_import`].
async fn cmd_import(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: ImportArgs,
    config: &CliConfig,
) -> Result<()> {
    import::cmd_import(client, fmt, args, config).await
}

/// Handle the `health` command: comprehensive decay health report.
async fn cmd_health(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: HealthArgs,
) -> Result<()> {
    let report = client.health_report(args.namespace.as_deref()).await?;
    print_out(&fmt.health(&report));
    Ok(())
}

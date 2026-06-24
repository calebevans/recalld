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
fn print_out(s: &str) {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(s.as_bytes()).ok();
    handle.flush().ok();
}

/// Print formatted error to stderr.
#[allow(dead_code)]
fn print_err(s: &str) {
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
            print_out(&serde_json::to_string(&memories).unwrap());
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

/// Handle the `import` command: read memories from a file and store
/// them via the API.
async fn cmd_import(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: ImportArgs,
    config: &CliConfig,
) -> Result<()> {
    use std::collections::HashMap;

    use commands::ImportFormat;
    use output::{ImportDryRunResult, ImportResult, MemoryView};

    // 1. Determine the import format.
    let format = match args.import_format {
        Some(f) => f,
        None => {
            if args.file == "-" {
                return Err(CliError::Other(
                    "--import-format is required when reading from stdin (-)".to_string(),
                ));
            }
            import::detect_format(&args.file).ok_or_else(|| {
                CliError::Other(format!(
                    "cannot detect format from file extension '{}'; use --import-format json|jsonl",
                    args.file
                ))
            })?
        }
    };

    // 2. Read the input into parsed records.
    let records: Vec<(usize, Result<MemoryView>)> = if args.file == "-" {
        // Read from stdin.
        let stdin = io::stdin();
        let handle = stdin.lock();
        match format {
            ImportFormat::Json => {
                let memories = import::read_json(handle)?;
                memories
                    .into_iter()
                    .enumerate()
                    .map(|(i, m)| (i + 1, Ok(m)))
                    .collect()
            }
            ImportFormat::Jsonl => import::read_jsonl(handle),
        }
    } else {
        // Read from file.
        let file = std::fs::File::open(&args.file).map_err(|e| {
            CliError::Other(format!("failed to open '{}': {e}", args.file))
        })?;
        let reader = std::io::BufReader::new(file);
        match format {
            ImportFormat::Json => {
                let memories = import::read_json(reader)?;
                memories
                    .into_iter()
                    .enumerate()
                    .map(|(i, m)| (i + 1, Ok(m)))
                    .collect()
            }
            ImportFormat::Jsonl => import::read_jsonl(reader),
        }
    };

    // 3. Separate successfully parsed records from parse failures.
    let mut valid: Vec<(usize, MemoryView)> = Vec::new();
    let mut parse_errors: Vec<String> = Vec::new();

    for (line_num, result) in records {
        match result {
            Ok(memory) => {
                if memory.summary.is_empty() {
                    let msg = format!("Record {line_num}: missing or empty summary");
                    if args.continue_on_error {
                        parse_errors.push(msg);
                        continue;
                    } else {
                        return Err(CliError::Other(msg));
                    }
                }
                valid.push((line_num, memory));
            }
            Err(e) => {
                let msg = format!("Record {line_num}: {e}");
                if args.continue_on_error {
                    parse_errors.push(msg);
                } else {
                    return Err(CliError::Other(msg));
                }
            }
        }
    }

    // 4. If --dry-run, collect stats and exit.
    if args.dry_run {
        let mut ns_counts: HashMap<String, u64> = HashMap::new();
        let mut tag_counts: HashMap<String, u64> = HashMap::new();
        let mut total_summary_len: u64 = 0;
        let mut total_full_text_len: u64 = 0;
        let mut full_text_count: u64 = 0;

        for (_, memory) in &valid {
            let record_ns = if memory.namespace.is_empty() {
                &config.default_namespace
            } else {
                &memory.namespace
            };
            let ns = args
                .namespace
                .as_deref()
                .unwrap_or(record_ns)
                .to_string();
            *ns_counts.entry(ns).or_insert(0) += 1;

            for tag in &memory.tags {
                *tag_counts.entry(tag.clone()).or_insert(0) += 1;
            }

            total_summary_len += memory.summary.len() as u64;
            if let Some(ref ft) = memory.full_text {
                total_full_text_len += ft.len() as u64;
                full_text_count += 1;
            }
        }

        // Get top tags (most frequent, up to 10).
        let mut tags_sorted: Vec<(String, u64)> = tag_counts.into_iter().collect();
        tags_sorted.sort_by(|a, b| b.1.cmp(&a.1));
        let top_tags: Vec<String> = tags_sorted
            .into_iter()
            .take(10)
            .map(|(tag, _)| tag)
            .collect();

        let total = valid.len() as u64;
        let result = ImportDryRunResult {
            total_records: total + parse_errors.len() as u64,
            namespaces: ns_counts,
            top_tags,
            avg_summary_length: if total > 0 {
                total_summary_len / total
            } else {
                0
            },
            avg_full_text_length: if full_text_count > 0 {
                total_full_text_len / full_text_count
            } else {
                0
            },
            would_import: total,
            would_skip: parse_errors.len() as u64,
        };

        print_out(&fmt.import_dry_run(&result));
        return Ok(());
    }

    // 5. Import loop — store each record via the API.
    let start = std::time::Instant::now();
    let total = valid.len();
    let mut imported: u64 = 0;
    let mut skipped: u64 = 0;
    let mut failed: u64 = parse_errors.len() as u64;
    let mut failed_records: Vec<String> = parse_errors;
    let mut ns_counts: HashMap<String, u64> = HashMap::new();

    // Progress reporting interval: every 10% (minimum 1).
    let progress_interval = std::cmp::max(total / 10, 1);
    let is_human = fmt.error(&CliError::Other(String::new())).contains("Error:");

    for (idx, (line_num, memory)) in valid.into_iter().enumerate() {
        let record_ns = if memory.namespace.is_empty() {
            &config.default_namespace
        } else {
            &memory.namespace
        };
        let target_ns = args
            .namespace
            .as_deref()
            .unwrap_or(record_ns);

        // Duplicate detection.
        if args.skip_duplicates {
            match client
                .search_memories(&memory.summary, 1, Some(target_ns), false, &[], 0, None)
                .await
            {
                Ok(search_result) => {
                    if let Some(hit) = search_result.memories.first() {
                        if hit.score >= 0.95 {
                            if is_human {
                                print_err(&format!(
                                    "Skipped duplicate: \"{}\" (matched ID: {})\n",
                                    &memory.summary[..memory.summary.len().min(60)],
                                    &hit.memory.id[..8]
                                ));
                            }
                            skipped += 1;
                            continue;
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("Record {line_num}: duplicate search failed: {e}");
                    if args.continue_on_error {
                        failed_records.push(msg.clone());
                        failed += 1;
                        if is_human {
                            print_err(&format!("{}\n", msg));
                        }
                        continue;
                    } else {
                        return Err(CliError::Other(msg));
                    }
                }
            }
        }

        // Store the memory.
        match client
            .store_memory_raw(
                &memory.summary,
                memory.full_text.as_deref(),
                &memory.tags,
                Some(target_ns),
            )
            .await
        {
            Ok(_) => {
                imported += 1;
                *ns_counts.entry(target_ns.to_string()).or_insert(0) += 1;
                if is_human && (idx + 1) % progress_interval == 0 {
                    print_err(&format!(
                        "Progress: {}/{} imported\n",
                        idx + 1,
                        total
                    ));
                }
            }
            Err(e) => {
                let msg = format!("Record {line_num}: store failed: {e}");
                if args.continue_on_error {
                    failed_records.push(msg.clone());
                    failed += 1;
                    if is_human {
                        print_err(&format!("{}\n", msg));
                    }
                } else {
                    return Err(CliError::Other(msg));
                }
            }
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    let result = ImportResult {
        imported,
        skipped,
        failed,
        failed_records,
        namespaces: ns_counts,
        duration_ms,
    };

    print_out(&fmt.import_result(&result));
    Ok(())
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

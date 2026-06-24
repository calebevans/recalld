//! Import helpers — file parsing, format detection, and the import command handler.
//!
//! Provides functions for reading exported memory files in JSON array
//! or JSONL (one-object-per-line) format, plus the main [`cmd_import`]
//! handler used by the CLI dispatch in [`crate::cli::run`].

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use crate::cli::client::RecalldClient;
use crate::cli::commands::{ImportArgs, ImportFormat};
use crate::cli::config::CliConfig;
use crate::cli::output::{ImportDryRunResult, ImportResult, MemoryView, OutputFormatter};
use crate::cli::{print_err, print_out, CliError};

/// Read all memories from a JSON array file.
///
/// Loads the entire file into memory and deserializes as `Vec<MemoryView>`.
/// For very large files (>100MB), users should use JSONL format instead.
pub fn read_json(reader: impl Read) -> crate::cli::Result<Vec<MemoryView>> {
    let records: Vec<MemoryView> = serde_json::from_reader(reader)?;
    Ok(records)
}

/// Read all memories from a JSONL file (one JSON object per line).
///
/// Returns a vector of results — each line is independently parsed so
/// callers can decide whether to skip or abort on errors.
pub fn read_jsonl(reader: impl Read) -> Vec<(usize, crate::cli::Result<MemoryView>)> {
    BufReader::new(reader)
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let line_num = i + 1;
            match line {
                Err(e) => Some((line_num, Err(CliError::Other(format!("I/O error on line {line_num}: {e}"))))),
                Ok(line) => {
                    let trimmed = line.trim();
                    // Skip empty lines
                    if trimmed.is_empty() {
                        return None;
                    }
                    let result = serde_json::from_str::<MemoryView>(trimmed)
                        .map_err(|e| CliError::Other(format!("parse error on line {line_num}: {e}")));
                    Some((line_num, result))
                }
            }
        })
        .collect()
}

/// Auto-detect import format from file extension.
///
/// Returns `None` if the extension is not recognized, in which case the
/// caller should require an explicit `--import-format` flag.
pub fn detect_format(path: &str) -> Option<ImportFormat> {
    let ext = Path::new(path).extension()?.to_str()?;
    match ext {
        "json" => Some(ImportFormat::Json),
        "jsonl" => Some(ImportFormat::Jsonl),
        _ => None,
    }
}

/// Handle the `import` command: read memories from a file and store
/// them via the API.
pub async fn cmd_import(
    client: &RecalldClient,
    fmt: &dyn OutputFormatter,
    args: ImportArgs,
    config: &CliConfig,
) -> crate::cli::Result<()> {
    // 1. Determine the import format.
    let format = match args.import_format {
        Some(f) => f,
        None => {
            if args.file == "-" {
                return Err(CliError::Other(
                    "--import-format is required when reading from stdin (-)".to_string(),
                ));
            }
            detect_format(&args.file).ok_or_else(|| {
                CliError::Other(format!(
                    "cannot detect format from file extension '{}'; use --import-format json|jsonl",
                    args.file
                ))
            })?
        }
    };

    // 2. Read the input into parsed records.
    let records: Vec<(usize, crate::cli::Result<MemoryView>)> = if args.file == "-" {
        // Read from stdin.
        let stdin = io::stdin();
        let handle = stdin.lock();
        match format {
            ImportFormat::Json => {
                let memories = read_json(handle)?;
                memories
                    .into_iter()
                    .enumerate()
                    .map(|(i, m)| (i + 1, Ok(m)))
                    .collect()
            }
            ImportFormat::Jsonl => read_jsonl(handle),
        }
    } else {
        // Read from file.
        let file = std::fs::File::open(&args.file).map_err(|e| {
            CliError::Other(format!("failed to open '{}': {e}", args.file))
        })?;
        let reader = std::io::BufReader::new(file);
        match format {
            ImportFormat::Json => {
                let memories = read_json(reader)?;
                memories
                    .into_iter()
                    .enumerate()
                    .map(|(i, m)| (i + 1, Ok(m)))
                    .collect()
            }
            ImportFormat::Jsonl => read_jsonl(reader),
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

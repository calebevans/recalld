//! Import helpers — file parsing and format detection.
//!
//! Provides functions for reading exported memory files in JSON array
//! or JSONL (one-object-per-line) format. Used by the `import` command
//! handler in [`crate::cli::cmd_import`].

use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use crate::cli::commands::ImportFormat;
use crate::cli::output::MemoryView;
use crate::cli::CliError;

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

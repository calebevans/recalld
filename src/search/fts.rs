//! SQLite FTS5 full-text search index.
//!
//! Replaces the per-namespace in-memory BM25 indexes with a single
//! on-disk SQLite database using FTS5 for tokenization, stemming,
//! and BM25 scoring. Namespaces are distinguished by a `namespace_id`
//! column in the `id_map` table rather than separate index instances.

use std::path::Path;

use rusqlite::OptionalExtension;

use crate::model::{MemoryId, NamespaceId};

// ---------------------------------------------------------------------------
// FtsError
// ---------------------------------------------------------------------------

/// Errors that can occur during FTS5 operations.
#[derive(Debug, thiserror::Error)]
pub enum FtsError {
    /// A SQLite error occurred.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The memory ID bytes were invalid.
    #[error("invalid memory ID bytes")]
    InvalidMemoryId,
}

// ---------------------------------------------------------------------------
// FtsIndex
// ---------------------------------------------------------------------------

/// SQLite FTS5 full-text search index.
///
/// Replaces the per-namespace in-memory BM25 indexes with a single
/// on-disk SQLite database using FTS5 for tokenization, stemming,
/// and BM25 scoring. Namespaces are distinguished by a `namespace_id`
/// column in the `id_map` table rather than separate index instances.
pub struct FtsIndex {
    conn: rusqlite::Connection,
}

impl FtsIndex {
    /// Open or create the FTS5 index database.
    ///
    /// The database file is stored at `{data_dir}/fts.db`.
    /// Tables are created if they do not already exist.
    pub fn new(data_dir: &Path) -> Result<Self, FtsError> {
        let db_path = data_dir.join("fts.db");
        let conn = rusqlite::Connection::open(&db_path)?;

        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            ",
        )?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS id_map (
                rowid         INTEGER PRIMARY KEY AUTOINCREMENT,
                namespace_id  INTEGER NOT NULL,
                memory_id     BLOB    NOT NULL UNIQUE
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS fts_content USING fts5(
                summary,
                full_text,
                tags,
                content='',
                contentless_delete=1,
                tokenize='porter unicode61'
            );
            ",
        )?;

        Ok(Self { conn })
    }

    /// Add or update a document in the FTS index.
    ///
    /// If a document with the same `memory_id` already exists, it is
    /// removed first (upsert semantics matching the old `Bm25Index::add`).
    ///
    /// The `tags` slice is joined with spaces so FTS5 tokenizes each tag
    /// as a separate term.
    pub fn add(
        &self,
        namespace_id: NamespaceId,
        memory_id: MemoryId,
        summary: &str,
        full_text: Option<&str>,
        tags: &[String],
    ) -> Result<(), FtsError> {
        let tx = self.conn.unchecked_transaction()?;

        // Remove existing entry if present (upsert).
        self.remove(memory_id)?;

        // Insert into id_map to get a rowid.
        self.conn.execute(
            "INSERT INTO id_map (namespace_id, memory_id) VALUES (?1, ?2)",
            rusqlite::params![namespace_id.get(), memory_id.as_bytes().as_slice()],
        )?;
        let rowid = self.conn.last_insert_rowid();

        // Insert into FTS5 with explicit rowid.
        let tags_text = tags.join(" ");
        self.conn.execute(
            "INSERT INTO fts_content (rowid, summary, full_text, tags) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![rowid, summary, full_text.unwrap_or(""), &tags_text],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Remove a document from the FTS index by memory ID.
    ///
    /// Returns `Ok(true)` if the document was found and removed,
    /// `Ok(false)` if no document with that ID existed.
    pub fn remove(&self, memory_id: MemoryId) -> Result<bool, FtsError> {
        // Look up the rowid.
        let rowid: Option<i64> = self
            .conn
            .query_row(
                "SELECT rowid FROM id_map WHERE memory_id = ?1",
                rusqlite::params![memory_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;

        let Some(rowid) = rowid else {
            return Ok(false);
        };

        // Delete from FTS5 first (contentless_delete requires this).
        self.conn.execute(
            "DELETE FROM fts_content WHERE rowid = ?1",
            rusqlite::params![rowid],
        )?;

        // Delete from id_map.
        self.conn.execute(
            "DELETE FROM id_map WHERE rowid = ?1",
            rusqlite::params![rowid],
        )?;

        Ok(true)
    }

    /// Search the FTS index for documents matching the query.
    ///
    /// Results are filtered to the given namespace and scored using
    /// FTS5's built-in `bm25()` function with column weights:
    ///   - summary:   5.0  (highest -- the most curated text)
    ///   - full_text:  1.0  (lowest -- verbose, noisy)
    ///   - tags:       2.0  (medium -- structured keywords)
    ///
    /// FTS5's `bm25()` returns *negative* scores where more negative
    /// means more relevant. We negate them so the pipeline receives
    /// positive scores consistent with the old BM25 index.
    ///
    /// Returns up to `k` results as `(MemoryId, score)` pairs sorted
    /// by descending relevance.
    pub fn search(
        &self,
        namespace_id: NamespaceId,
        query: &str,
        k: usize,
    ) -> Result<Vec<(MemoryId, f32)>, FtsError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Escape the query for FTS5 syntax safety.
        let safe_query = escape_query(query);
        if safe_query.is_empty() {
            return Ok(Vec::new());
        }

        let mut stmt = self.conn.prepare_cached(
            "SELECT m.memory_id, -bm25(fts_content, 5.0, 1.0, 2.0) AS score
             FROM fts_content
             JOIN id_map m ON m.rowid = fts_content.rowid
             WHERE fts_content MATCH ?1
               AND m.namespace_id = ?2
             ORDER BY score DESC
             LIMIT ?3",
        )?;

        let results = stmt.query_map(
            rusqlite::params![&safe_query, namespace_id.get(), k as i64],
            |row| {
                let uuid_bytes: Vec<u8> = row.get(0)?;
                let score: f64 = row.get(1)?;
                Ok((uuid_bytes, score))
            },
        )?;

        let mut out = Vec::with_capacity(k);
        for result in results {
            let (uuid_bytes, score) = result?;
            if uuid_bytes.len() == 16 {
                let bytes: [u8; 16] = uuid_bytes.try_into().unwrap();
                let memory_id = MemoryId::from_bytes(bytes);
                out.push((memory_id, score as f32));
            }
        }

        Ok(out)
    }

    /// Whether the index contains no documents.
    pub fn is_empty(&self) -> Result<bool, FtsError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM id_map", [], |row| row.get(0))?;
        Ok(count == 0)
    }
}

/// Sanitize a query string for safe use in FTS5 MATCH expressions.
/// Quotes each token to neutralize FTS5 operators and joins with OR.
fn escape_query(query: &str) -> String {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t))
        .collect();
    terms.join(" OR ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ns_id() -> NamespaceId {
        NamespaceId::new(1)
    }

    fn test_ns_id_2() -> NamespaceId {
        NamespaceId::new(2)
    }

    #[test]
    fn add_and_search() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();

        let id1 = MemoryId::new();
        let id2 = MemoryId::new();
        let id3 = MemoryId::new();

        index
            .add(
                test_ns_id(),
                id1,
                "Rust programming language systems",
                None,
                &[],
            )
            .unwrap();
        index
            .add(
                test_ns_id(),
                id2,
                "Python programming language scripting",
                None,
                &[],
            )
            .unwrap();
        index
            .add(test_ns_id(), id3, "Rust memory safety ownership", None, &[])
            .unwrap();

        let results = index.search(test_ns_id(), "Rust programming", 10).unwrap();
        assert!(!results.is_empty());

        // id1 mentions both "rust" and "programming", so should rank highest.
        assert_eq!(results[0].0, id1);
    }

    #[test]
    fn remove_document() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();

        let id = MemoryId::new();
        index
            .add(test_ns_id(), id, "test document content", None, &[])
            .unwrap();
        assert!(!index.is_empty().unwrap());

        let removed = index.remove(id).unwrap();
        assert!(removed);
        assert!(index.is_empty().unwrap());

        let results = index.search(test_ns_id(), "test", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn remove_nonexistent() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();
        assert!(!index.remove(MemoryId::new()).unwrap());
    }

    #[test]
    fn search_empty_index() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();
        let results = index.search(test_ns_id(), "anything", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_no_matching_terms() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();
        index
            .add(test_ns_id(), MemoryId::new(), "alpha beta gamma", None, &[])
            .unwrap();
        let results = index.search(test_ns_id(), "delta epsilon", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn update_existing_document() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();

        let id = MemoryId::new();
        index
            .add(test_ns_id(), id, "original content", None, &[])
            .unwrap();
        index
            .add(test_ns_id(), id, "updated replacement", None, &[])
            .unwrap();

        let results = index.search(test_ns_id(), "original", 10).unwrap();
        assert!(results.is_empty());

        let results = index.search(test_ns_id(), "updated", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id);
    }

    #[test]
    fn namespace_isolation() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();

        let id1 = MemoryId::new();
        let id2 = MemoryId::new();

        index
            .add(test_ns_id(), id1, "shared keyword alpha", None, &[])
            .unwrap();
        index
            .add(test_ns_id_2(), id2, "shared keyword alpha", None, &[])
            .unwrap();

        let results_ns1 = index.search(test_ns_id(), "alpha", 10).unwrap();
        assert_eq!(results_ns1.len(), 1);
        assert_eq!(results_ns1[0].0, id1);

        let results_ns2 = index.search(test_ns_id_2(), "alpha", 10).unwrap();
        assert_eq!(results_ns2.len(), 1);
        assert_eq!(results_ns2[0].0, id2);
    }

    #[test]
    fn escape_query_safety() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();

        index
            .add(
                test_ns_id(),
                MemoryId::new(),
                "some test document",
                None,
                &[],
            )
            .unwrap();

        // These should not cause FTS5 syntax errors.
        let _ = index.search(test_ns_id(), "AND OR NOT", 10).unwrap();
        let _ = index.search(test_ns_id(), "NEAR(test, doc)", 10).unwrap();
        let _ = index.search(test_ns_id(), "test*", 10).unwrap();
        let _ = index.search(test_ns_id(), "^test", 10).unwrap();
        let _ = index.search(test_ns_id(), "\"quoted phrase\"", 10).unwrap();
        let _ = index.search(test_ns_id(), "", 10).unwrap();
        let _ = index.search(test_ns_id(), "   ", 10).unwrap();
    }

    #[test]
    fn escape_query_sanitizes() {
        let result = escape_query("Sarah trip Japan");
        assert_eq!(result, "\"Sarah\" OR \"trip\" OR \"Japan\"");

        let result = escape_query("");
        assert!(result.is_empty());

        let result = escape_query("   ");
        assert!(result.is_empty());

        let result = escape_query("test\"injection");
        assert_eq!(result, "\"testinjection\"");
    }
}

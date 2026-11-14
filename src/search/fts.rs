//! SQLite FTS5 full-text search index.
//!
//! Replaces the per-namespace in-memory BM25 indexes with a single
//! on-disk SQLite database using FTS5 for tokenization, stemming,
//! and BM25 scoring. Namespaces are distinguished by a `namespace_id`
//! column in the `id_map` table rather than separate index instances.

use std::{collections::HashSet, path::Path};

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

    #[error("nonfinite FTS score")]
    NonfiniteScore,
}

pub const MAX_FTS_EXAMINED: usize = 12_800;

/// Counts describe the ordered source rows consumed, including a cap lookahead.
/// `exhausted` is true only after observing end-of-stream. At the work ceiling
/// it remains false even if the last consumed row happened to be the last match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FtsCoverage {
    pub examined: usize,
    pub eligible: usize,
    pub admitted: usize,
    pub pages: usize,
    pub exhausted: bool,
    pub candidate_limited: bool,
    pub budget_exhausted: bool,
}

#[derive(Debug, Default)]
pub struct EligibleFtsResults {
    pub matches: Vec<(MemoryId, f32)>,
    pub coverage: FtsCoverage,
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
    /// Drop all derived rows before a recovery rebuild, including deleted IDs.
    pub(crate) fn clear(&self) -> Result<(), FtsError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM fts_content", [])?;
        tx.execute("DELETE FROM id_map", [])?;
        tx.commit()?;
        Ok(())
    }

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
             ORDER BY score DESC, m.memory_id ASC
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

        let instrumented = crate::performance::enabled();
        let mut examined = 0usize;
        let mut out = Vec::with_capacity(k);
        for result in results {
            examined += usize::from(instrumented);
            let (uuid_bytes, score) = result?;
            if uuid_bytes.len() == 16 {
                let bytes: [u8; 16] = uuid_bytes.try_into().unwrap();
                let memory_id = MemoryId::from_bytes(bytes);
                out.push((memory_id, score as f32));
            }
        }

        tracing::trace!(target: "recalld::performance", kind = "fts_legacy_rows", examined, returned = out.len(), sqlite_internal_work_known = false);
        Ok(out)
    }

    /// Refill logical pages of `k` rows from one score-descending/UUID-ascending
    /// SQLite cursor. The statement pins the SQLite read view for every page;
    /// neither concurrent writes nor f32 score rounding can shift boundaries.
    /// Canonical membership must come from the caller's eligibility evaluation.
    /// This pins FTS only, not the cross-store generation owned by A1.
    pub fn search_eligible(
        &self,
        namespace_id: NamespaceId,
        query: &str,
        k: usize,
        eligible_ids: &HashSet<MemoryId>,
    ) -> Result<EligibleFtsResults, FtsError> {
        let safe_query = escape_query(query);
        let mut out = EligibleFtsResults::default();
        if safe_query.is_empty() {
            out.coverage.exhausted = true;
            return Ok(out);
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.memory_id, -bm25(fts_content, 5.0, 1.0, 2.0) AS score
             FROM fts_content JOIN id_map m ON m.rowid = fts_content.rowid
             WHERE fts_content MATCH ?1 AND m.namespace_id = ?2
             ORDER BY score DESC, m.memory_id ASC LIMIT ?3",
        )?;
        let mut rows = stmt.query(rusqlite::params![
            safe_query,
            namespace_id.get(),
            MAX_FTS_EXAMINED as i64
        ])?;
        // A live cursor is SQLite's native stable paging mechanism. Keep it
        // open across all logical pages rather than re-running OFFSET queries.
        'pages: while out.coverage.examined < MAX_FTS_EXAMINED {
            out.coverage.pages += 1;
            for _ in 0..k.max(1).min(MAX_FTS_EXAMINED - out.coverage.examined) {
                let Some(row) = rows.next()? else {
                    out.coverage.exhausted = true;
                    break 'pages;
                };
                out.coverage.examined += 1;
                let bytes: Vec<u8> = row.get(0)?;
                let id =
                    MemoryId::from_bytes(bytes.try_into().map_err(|_| FtsError::InvalidMemoryId)?);
                let raw_score: f64 = row.get(1)?;
                let score = raw_score as f32;
                if !score.is_finite() {
                    return Err(FtsError::NonfiniteScore);
                }
                let eligible = eligible_ids.contains(&id);
                out.coverage.eligible += usize::from(eligible);
                if out.matches.len() == k {
                    // One charged lookahead proves unseen source candidates.
                    // Never read beyond the examination budget to prove EOF.
                    out.coverage.candidate_limited = true;
                    break 'pages;
                }
                if eligible {
                    out.matches
                        .push((id, if score == 0.0 { 0.0 } else { score }));
                }
            }
        }
        out.coverage.admitted = out.matches.len();
        out.coverage.budget_exhausted =
            !out.coverage.exhausted && out.coverage.examined == MAX_FTS_EXAMINED;
        out.coverage.candidate_limited |= out.coverage.budget_exhausted;
        tracing::trace!(target: "recalld::performance", kind = "fts_rows",
            examined = out.coverage.examined, eligible = out.coverage.eligible,
            admitted = out.coverage.admitted, pages = out.coverage.pages,
            exhausted = out.coverage.exhausted, budget_exhausted = out.coverage.budget_exhausted,
            sqlite_internal_work_known = false);
        Ok(out)
    }

    /// Remove every document belonging to a namespace. Returns how many
    /// were removed.
    ///
    /// # Not an optimization — the only way to reach some of these rows
    ///
    /// Calling [`remove`](Self::remove) per id would only be correct for
    /// ids that meta.db still knows about. The decay sweep hard-deletes
    /// a Ghost record without touching this index, so a namespace can
    /// hold FTS rows whose meta.db record is already gone — ids no
    /// caller can enumerate. Filtering on `namespace_id` is the only
    /// handle on them.
    ///
    /// # `fts_content` rows have to go first
    ///
    /// `fts_content` is declared `contentless_delete=1`, so its rows are
    /// reachable ONLY by rowid, and the rowid lives in `id_map`. A bare
    /// `DELETE FROM id_map` therefore orphans every content row
    /// permanently: nothing can ever name them again.
    ///
    /// What that costs is not a later collision — `id_map.rowid` is
    /// `AUTOINCREMENT` and [`add`](Self::add) removes the old `id_map`
    /// row first, so re-indexing the same memory takes a fresh rowid and
    /// the `memory_id` UNIQUE constraint is never in play. It costs:
    ///
    /// - the space, for the life of the database, with no way to reclaim
    ///   it short of rebuilding the index;
    /// - **corpus-wide ranking drift.** `bm25()` scores against the
    ///   whole `fts_content` table, so orphaned rows go on contributing
    ///   to the document count and to per-term document frequencies. The
    ///   rows can never be *returned* — [`search`](Self::search) inner
    ///   joins `id_map` — but every query in every namespace is scored
    ///   against an IDF that counts them.
    ///
    /// So: collect the rowids, delete the content rows, and only then
    /// delete the id_map rows, all in one transaction.
    ///
    /// `id_map.namespace_id` is not indexed, so this is a table scan.
    /// Acceptable for a rare administrative operation.
    pub fn remove_namespace(&self, namespace_id: NamespaceId) -> Result<usize, FtsError> {
        let tx = self.conn.unchecked_transaction()?;

        let rowids: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT rowid FROM id_map WHERE namespace_id = ?1")?;
            let rows = stmt.query_map(rusqlite::params![namespace_id.get()], |row| row.get(0))?;
            rows.collect::<std::result::Result<Vec<i64>, _>>()?
        };

        if rowids.is_empty() {
            tx.commit()?;
            return Ok(0);
        }

        {
            let mut stmt = self
                .conn
                .prepare("DELETE FROM fts_content WHERE rowid = ?1")?;
            for rowid in &rowids {
                stmt.execute(rusqlite::params![rowid])?;
            }
        }

        self.conn.execute(
            "DELETE FROM id_map WHERE namespace_id = ?1",
            rusqlite::params![namespace_id.get()],
        )?;

        tx.commit()?;
        Ok(rowids.len())
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

    /// `fts_content` is contentless_delete, so its rows are reachable
    /// only by rowid — which lives in `id_map`. Deleting the id_map rows
    /// first orphans every content row permanently.
    ///
    /// This asserts on `fts_content` DIRECTLY, because nothing else can:
    /// an orphaned content row is invisible to `search` (which inner
    /// joins `id_map`), does not collide with anything on re-index
    /// (`id_map.rowid` is AUTOINCREMENT), and does not show up in
    /// `is_empty` (which counts `id_map`). Every observable this test
    /// used to check passed with the guard removed. The row count is the
    /// one that does not.
    #[test]
    fn w20_remove_namespace_deletes_id_map_and_fts_content_together() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();

        let id = MemoryId::new();
        index
            .add(test_ns_id(), id, "obsolete content here", None, &[])
            .unwrap();
        assert_eq!(fts_content_rows(&index), 1, "the setup did not index");

        assert_eq!(index.remove_namespace(test_ns_id()).unwrap(), 1);
        assert!(index.is_empty().unwrap());
        assert_eq!(
            fts_content_rows(&index),
            0,
            "an orphaned fts_content row survived: unreachable, unreclaimable, and \
             still counted by bm25's corpus statistics"
        );

        // The rest of the original guard: the memory re-indexes cleanly
        // and the stale text does not come back.
        index
            .add(test_ns_id(), id, "replacement content", None, &[])
            .unwrap();
        let results = index.search(test_ns_id(), "replacement", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id);
        assert_eq!(fts_content_rows(&index), 1);
    }

    /// The same assertion for the per-record path, which has the same
    /// ordering requirement and the same invisible failure mode.
    #[test]
    fn w61_remove_deletes_the_content_row_too() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();

        let id = MemoryId::new();
        index
            .add(test_ns_id(), id, "obsolete content here", None, &[])
            .unwrap();

        assert!(index.remove(id).unwrap());
        assert_eq!(
            fts_content_rows(&index),
            0,
            "an orphaned fts_content row survived a per-record removal"
        );
    }

    /// Rows actually present in the FTS5 virtual table, orphans included.
    fn fts_content_rows(index: &FtsIndex) -> i64 {
        index
            .conn
            .query_row("SELECT count(*) FROM fts_content", [], |row| row.get(0))
            .expect("count fts_content rows")
    }

    #[test]
    fn w21_remove_namespace_leaves_other_namespaces_searchable() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();

        let doomed = MemoryId::new();
        let kept = MemoryId::new();
        index
            .add(test_ns_id(), doomed, "shared keyword alpha", None, &[])
            .unwrap();
        index
            .add(test_ns_id_2(), kept, "shared keyword alpha", None, &[])
            .unwrap();

        index.remove_namespace(test_ns_id()).unwrap();

        assert!(index.search(test_ns_id(), "alpha", 10).unwrap().is_empty());
        let survivors = index.search(test_ns_id_2(), "alpha", 10).unwrap();
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].0, kept);
    }

    #[test]
    fn w22_remove_namespace_of_an_empty_namespace_is_zero() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();
        index
            .add(test_ns_id_2(), MemoryId::new(), "kept", None, &[])
            .unwrap();

        assert_eq!(index.remove_namespace(test_ns_id()).unwrap(), 0);
        assert!(!index.is_empty().unwrap());
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
    #[test]
    fn equal_fts_scores_use_uuid_order_before_limit_and_after_reinsertion() {
        let dir = tempfile::tempdir().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();
        let ids: Vec<_> = (1u128..=3)
            .map(|n| MemoryId::from_bytes(n.to_be_bytes()))
            .collect();
        for order in [vec![2, 1, 0], vec![0, 1, 2]] {
            for n in order {
                index
                    .add(test_ns_id(), ids[n], "identical keyword", None, &[])
                    .unwrap();
            }
            let results = index.search(test_ns_id(), "keyword", 2).unwrap();
            assert_eq!(
                results.iter().map(|r| r.0).collect::<Vec<_>>(),
                vec![ids[0], ids[1]]
            );
            assert_eq!(results[0].1, results[1].1);
        }
    }
    fn fixed_id(n: usize) -> MemoryId {
        MemoryId::from_bytes((n as u128).to_be_bytes())
    }

    #[test]
    fn eligible_refill_orders_ties_and_distinguishes_cap_from_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();
        // Deliberately insert backwards. The first 100 sorted rows are rejected.
        for n in (1..=106).rev() {
            index
                .add(test_ns_id(), fixed_id(n), "keyword", None, &[])
                .unwrap();
        }
        let eligible = (101..=106).map(fixed_id).collect();
        let result = index
            .search_eligible(test_ns_id(), "keyword", 5, &eligible)
            .unwrap();
        assert_eq!(
            result.matches.iter().map(|r| r.0).collect::<Vec<_>>(),
            (101..=105).map(fixed_id).collect::<Vec<_>>()
        );
        assert_eq!(
            result.coverage,
            FtsCoverage {
                examined: 106,
                eligible: 6,
                admitted: 5,
                pages: 22,
                exhausted: false,
                candidate_limited: true,
                budget_exhausted: false,
            }
        );
        // Upserts cannot duplicate a candidate or change an equal-score boundary.
        index
            .add(test_ns_id(), fixed_id(101), "keyword", None, &[])
            .unwrap();
        assert_eq!(
            index
                .search_eligible(test_ns_id(), "keyword", 5, &eligible)
                .unwrap()
                .matches,
            result.matches
        );
        for k in [6, 7] {
            let full = index
                .search_eligible(test_ns_id(), "keyword", k, &eligible)
                .unwrap();
            assert_eq!(full.matches.len(), 6);
            assert_eq!(full.coverage.examined, 106);
            assert!(full.coverage.exhausted);
            assert!(!full.coverage.candidate_limited);
            assert!(!full.coverage.budget_exhausted);
        }
        let empty = index
            .search_eligible(test_ns_id_2(), "keyword", 5, &eligible)
            .unwrap();
        assert_eq!(empty.coverage.examined, 0);
        assert!(empty.coverage.exhausted);
    }

    #[test]
    fn eligible_refill_budget_boundary_never_claims_exhaustive_underfill() {
        let dir = tempfile::tempdir().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();
        for n in 1..=MAX_FTS_EXAMINED + 1 {
            index
                .add(test_ns_id(), fixed_id(n), "keyword", None, &[])
                .unwrap();
        }
        for target in [MAX_FTS_EXAMINED, MAX_FTS_EXAMINED + 1] {
            let result = index
                .search_eligible(
                    test_ns_id(),
                    "keyword",
                    100,
                    &HashSet::from([fixed_id(target)]),
                )
                .unwrap();
            assert_eq!(
                result.matches.len(),
                usize::from(target == MAX_FTS_EXAMINED)
            );
            assert_eq!(result.coverage.examined, MAX_FTS_EXAMINED);
            assert_eq!(result.coverage.pages, 128);
            assert!(result.coverage.budget_exhausted);
            assert!(result.coverage.candidate_limited);
            assert!(!result.coverage.exhausted);
        }
        index.remove(fixed_id(MAX_FTS_EXAMINED + 1)).unwrap();
        let exact = index
            .search_eligible(test_ns_id(), "keyword", 100, &HashSet::new())
            .unwrap();
        assert!(exact.coverage.budget_exhausted); // No uncharged EOF probe.
        assert!(!exact.coverage.exhausted);
        index.remove(fixed_id(MAX_FTS_EXAMINED)).unwrap();
        let ended = index
            .search_eligible(test_ns_id(), "keyword", 100, &HashSet::new())
            .unwrap();
        assert_eq!(ended.coverage.examined, MAX_FTS_EXAMINED - 1);
        assert!(ended.coverage.exhausted);
        assert!(!ended.coverage.budget_exhausted);
        assert!(!ended.coverage.candidate_limited);
    }

    #[test]
    fn eligible_refill_rejects_corrupt_ids_and_sqlite_failures() {
        let dir = tempfile::tempdir().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();
        index
            .add(test_ns_id(), fixed_id(1), "keyword", None, &[])
            .unwrap();
        index
            .conn
            .execute("UPDATE id_map SET memory_id = X'01'", [])
            .unwrap();
        assert!(matches!(
            index.search_eligible(test_ns_id(), "keyword", 100, &HashSet::new()),
            Err(FtsError::InvalidMemoryId)
        ));
        index.conn.execute("DROP TABLE id_map", []).unwrap();
        assert!(matches!(
            index.search_eligible(test_ns_id(), "keyword", 100, &HashSet::new()),
            Err(FtsError::Sqlite(_))
        ));
    }
    #[test]
    fn refill_cursor_does_not_mix_namespace_views_during_concurrent_writes() {
        use std::sync::{
            Arc, Barrier,
            atomic::{AtomicBool, Ordering},
        };
        let dir = tempfile::tempdir().unwrap();
        let index = FtsIndex::new(dir.path()).unwrap();
        for n in 1..=2000 {
            let ns = if n <= 1000 {
                test_ns_id()
            } else {
                test_ns_id_2()
            };
            index.add(ns, fixed_id(n), "keyword", None, &[]).unwrap();
        }
        // Each atomic update swaps the entire namespace membership. Every valid
        // pinned result has exactly 1,000 rows and exactly one eligible terminal.
        let barrier = Arc::new(Barrier::new(2));
        let finished = Arc::new(AtomicBool::new(false));
        let writer_barrier = barrier.clone();
        let writer_finished = finished.clone();
        let path = dir.path().join("fts.db");
        let writer = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(path).unwrap();
            writer_barrier.wait();
            let mut writes = 0;
            while !writer_finished.load(Ordering::Acquire) || writes == 0 {
                conn.execute("UPDATE id_map SET namespace_id = CASE WHEN namespace_id = ?1 THEN ?2 ELSE ?1 END",
                    rusqlite::params![test_ns_id().get(), test_ns_id_2().get()]).unwrap();
                writes += 1;
            }
            writes
        });
        barrier.wait();
        let eligible = HashSet::from([fixed_id(1000), fixed_id(2000)]);
        let results: Vec<_> = (0..20)
            .map(|_| index.search_eligible(test_ns_id(), "keyword", 100, &eligible))
            .collect();
        finished.store(true, Ordering::Release);
        assert!(writer.join().unwrap() > 0);
        for result in results {
            let result = result.unwrap();
            assert_eq!(result.coverage.examined, 1000);
            assert_eq!(result.coverage.eligible, 1);
            assert_eq!(result.coverage.admitted, 1);
            assert_eq!(result.coverage.pages, 11);
            assert!(result.coverage.exhausted);
            assert!(eligible.contains(&result.matches[0].0));
        }
    }
}

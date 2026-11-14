# Storage architecture decision inputs

## Evidence and boundaries

The inspected source is commit `5aa4692`. `Cargo.lock` pins redb 2.6.3 and rusqlite 0.35. The current design coordinates metadata, edges, text offsets, mmap vectors, SQLite FTS and in-memory indexes. The compaction sequence in `src/storage/engine.rs` commits metadata offsets before publishing a separate filesystem marker; recovery in `src/storage/text.rs` relies on that marker. A database's own atomic transactions cannot make this cross-resource sequence atomic.

Primary sources checked 2026-11-14 UTC:

- [SQLite atomic commit](https://sqlite.org/atomiccommit.html): transactions provide all-or-none changes, including crash recovery under documented filesystem assumptions. This supports consolidating canonical state into one transaction, not a claim of measured recalld reliability or speed.
- [SQLite WAL](https://sqlite.org/wal.html): readers and a writer can coexist; separate attached databases do not form one atomic WAL transaction. WAL requires local shared-memory coordination and checkpoint management. Therefore a SQLite proposal must use one canonical database, not split canonical state into several WAL databases.
- [SQLite online backup](https://sqlite.org/backup.html): the backup API creates a database snapshot while permitting incremental copying. Replacing recalld backup with it still requires application validation, versioning and exclusive restore ownership.
- [SQLite FTS5](https://sqlite.org/fts5.html): FTS5 supports transactional updates, BM25 and index rebuilding; external-content tables require synchronization. Existing use of rusqlite reduces dependency novelty, but migration effort remains substantial.
- [redb design](https://github.com/cberner/redb/blob/master/docs/design.md): useful design context, but moving upstream documentation is not evidence about every behavior of the pinned 2.6.3 release. Attempts to retrieve version-specific API docs failed; no latest-version guarantee is attributed to the pinned dependency.

## Options

| Option | Benefit | Cost / unresolved risk | Recommendation |
| --- | --- | --- | --- |
| Repair current multi-file engine | Lowest compatibility cost; preserves existing layout | Must own compaction, cross-file commit, slot lifetime and recovery protocols indefinitely | Immediate containment and regression tests |
| Consolidate canonical records, text, vectors and edges in one redb database | Retains Rust-native dependency; one commit boundary | FTS remains derived and requires generation/rebuild handling; large-value performance unmeasured | Valid challenger in comparative spike |
| One SQLite canonical database plus FTS and rebuildable exact vector index | Familiar transaction, backup and constraint facilities; removes custom text-offset/slot authority | Migration, writer contention, vector blob I/O, rebuilding and binary size need measurement | Preferred design hypothesis, gated on parity and workload measurements |
| External vector database/service | Mature indexing/scaling choices | New daemon/network/operations burden; transactions still span memory metadata unless unified | Defer absent measured scale requirement |

No source establishes that SQLite will be faster for this workload. A Python transaction demonstration is an architectural proof of concept only. A backend decision requires matching durability, corpus, embedding dimensions, ingestion semantics, restart behavior, query correctness, peak RSS and latency. Preserve the current backend until that gate passes. ANN is a separate decision from canonical storage; retain an exact baseline and measure recall loss before adding ANN.

## Migration invariants

UUIDs, namespace IDs and names, structured tags, timestamps, phases, stability, supersession/parent edges, tombstones and embedding provenance must round-trip. Export is from a quiescent consistent snapshot. Import into a new destination, verify counts plus per-record content/edge hashes and representative query parity, then atomically switch ownership. Never rewrite the only copy. Once new writes begin, rollback requires exporting/replaying them or an explicit read-only rollback window; reopening a stale old database is not lossless rollback.

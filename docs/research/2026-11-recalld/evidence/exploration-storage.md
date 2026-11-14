# Storage exploration evidence

Date: 2026-11-13. Scope: storage, durability, backup/restore, concurrency, and namespace isolation.

Source inspection only. No experiments, tests, external research, production edits, or recalld MCP calls were performed. Followed the supplied AGENTS.md instructions; no additional AGENTS.md files were found under src or docs. This document is the sole authorized write.

“Source-proven” below means the implementation contains the described ordering or missing guard. It does not mean the proposed failure has been reproduced. Priorities are provisional.

## Prioritized suspected bugs

### 1. P1: Compaction commit and recovery marker are not atomic

**Source-proven behavior:** `compact_text_log` commits replacement text offsets through `batch_update_records` before creating and syncing `.compaction-meta-committed` (`src/storage/engine.rs:1414–1425`). Recovery interprets an absent committed marker as permission to delete `fulltext.dat.new` and retain the original text file (`src/storage/text.rs:581–602`). The metadata transaction itself commits in `src/storage/metadata.rs:818–829`.

**Untested failure hypothesis:** A crash after metadata commit but before marker creation leaves new offsets pointing into the old text file. Recovery discards the matching compacted data. Reads could fail integrity checks or return another valid entry, depending on offsets and lengths. Marker creation failure also leaves the process with committed offsets before finalization.

**Small reproduction idea:** In a temporary database, create dead text before a live entry so compaction moves the live entry. Interrupt immediately after the metadata commit, reopen, and read the live text. Requires controlled interruption or fault injection; not performed.

### 2. P1: Hard deletion can free a tombstone's already-reused vector slot

**Source-proven behavior:** API deletion tombstones a record and frees its original vector slot under one exclusive storage lock (`src/api/adapters.rs:438–478`). Storage `delete_memory` removes any existing record and unconditionally frees its recorded slot, with no tombstone check (`src/storage/engine.rs:1179–1196`). `VectorStore::free_slot` checks bounds but not allocation ownership or whether the slot is already free (`src/storage/vectors.rs:582–626`). Normal API deletion explicitly rejects repeat tombstone deletion.

**Untested failure hypothesis:** A storage caller hard-deleting a tombstone after slot reuse can zero another memory's embedding and return its live slot to the free list. Without reuse, a second free can corrupt the free list. An externally reachable execution path invoking hard deletion on such a tombstone was not established.

**Small reproduction idea:** Insert A, tombstone A and free its slot, insert B into that slot, then call storage `delete_memory(A)` and inspect B's vector and subsequent allocations. Not performed.

### 3. P1: Restore validates listed files but not database completeness

**Source-proven behavior:** Manifest validation checks major/minor version compatibility (`src/backup/restore.rs:155` onward). Extraction iterates only `manifest.files`, verifies their CRCs, and returns success without requiring the core database files or checking namespace/vector completeness (`src/backup/restore.rs:282–377`). Opening a missing or zero-length vector file initializes an empty file (`src/storage/vectors.rs:322–352`).

**Untested failure hypothesis:** An incomplete archive with a correspondingly incomplete manifest can restore successfully while omitting required data. Missing namespace vectors may subsequently be silently recreated empty even when metadata still references their slots. Ordinary backup collection requires core files; this concern is about restore accepting incomplete inputs.

**Small reproduction idea:** Remove one namespace vector file from both a disposable archive and its manifest, restore into a temporary directory, then inspect records and vectors. A version-compatible manifest with an empty `files` list is a smaller acceptance check. Not performed.

### 4. P2: Restore does not acquire exclusive ownership of its target

**Source-proven behavior:** Restore attempts to stop the default daemon unless `no_stop_daemon` is set, then renames the target directory and extracts into its original path without acquiring the target's `recalld.lock` (`src/backup/restore.rs:105–123`, `backup_current_data` at line 250 onward). Normal engine opening acquires an exclusive lock on that file (`src/storage/engine.rs:298–310`).

**Untested failure hypothesis:** An embedded process, or a daemon retained with `--no-stop-daemon`, can retain open files in the renamed old database while a second engine opens the restored path. Writes can continue against different database directories under the same configured path. Exact behavior depends on platform and active process lifecycle.

**Small reproduction idea:** Hold an engine open on a temporary database, restore over its directory, then inspect whether the original engine remains writable and whether another engine can open the restored path. Not performed.

### 5. P2: Storage insertion accepts two independent namespace identities

**Source-proven behavior:** `insert_memory` selects the vector store using its `namespace_id` argument and updates only the record's vector slot (`src/storage/engine.rs:1086–1100`). Metadata insertion persists and indexes `record.namespace_id` independently (`src/storage/metadata.rs:161–184`). No equality check or assignment reconciles these namespace identities in the storage insertion path.

**Untested failure hypothesis:** A caller passing namespace A with a record naming B can persist a record whose embedding lives in A while later reads and deletion select B. This is a storage boundary invariant gap; no externally reachable mismatched caller was established.

**Small reproduction idea:** Create A and B with matching dimensions, insert through A with a record naming B, then retrieve its vector and delete it. Inspect both namespaces' slots. Not performed.

### 6. P2: Phase-index updates can be reordered after metadata commits

**Source-proven behavior:** `MetadataStore::update_decay_state` first commits the record through `update_record`, then separately locks and transitions the in-memory phase bitmap (`src/storage/metadata.rs:129–150`, `221–246`). Both operations accept shared references. Record commit and bitmap transition are not one critical section.

**Untested failure hypothesis:** If callers allow concurrent transitions, their bitmap updates can occur in reverse commit order, leaving phase scans inconsistent with persisted records. A caller holding an exclusive storage lock across the entire method prevents that interleaving. End-to-end production reachability was not established.

**Small reproduction idea:** Pause one transition after its metadata commit, complete a second transition, resume the first, and compare phase scans with the persisted record. Requires controlled scheduling; not performed.

## Source-proven protections and review limits

- Engine opening holds an exclusive `recalld.lock`; vector stores also hold exclusive file locks (`src/storage/engine.rs:298–310`, `src/storage/vectors.rs:334–337`). Backup separately attempts exclusive locks on its database and vector files (`src/backup/lock.rs:67` onward); `force` permits continuing after failures. Live forced-backup consistency was not established.
- Insertion syncs vector and optional text data before committing metadata (`src/storage/engine.rs:1111–1128`). This ordering protects referenced payload durability, but is not a cross-file transaction.
- Namespace creation validates names and rejects case-insensitive duplicate names (`src/storage/engine.rs:1325` onward; `src/storage/metadata.rs:892–943`). Vector opening checks canonical containment (`src/storage/vectors.rs:734–788`). These protections do not reconcile the two namespace identities accepted by insertion.
- Normal API tombstone deletion performs its check, tombstone, and slot release under one write lock, preventing two API deletes from freeing the same slot concurrently (`src/api/adapters.rs:438–487`).
- Startup rebuilds phase indexes, cleans orphaned edges, and quarantines orphaned namespace directories rather than deleting them (`src/storage/engine.rs:348` onward). Startup rebuilding can repair transient bitmap divergence after restart; it does not establish correctness during a running process.
- Restore rejects absolute and parent-traversal entry paths and verifies listed file CRCs (`src/backup/restore.rs:309–372`). Completeness, exclusive target ownership, and crash-atomic restoration remain separate questions.

No runtime failures are claimed as reproduced. Next steps should first validate the compaction crash window and tombstone/slot-reuse sequence in isolated fixtures, if experiments are subsequently authorized.

# A1 legacy storage containment

Implemented on `impl/storage`, based on `653c8d3` (including the upstream
sweep/tombstone and daemon socket fixes). This is a bounded A1 containment
slice, not the canonical mutation foundation or complete A1 specification.
No canonical format, lineage, generation, receipt, or lifecycle policy changes.

## Behavior

- Engine inserts reject argument/record ID or namespace disagreement and
  duplicate IDs before allocating payloads. Missing namespaces and vector
  dimension mismatches also fail before allocation.
- Hard deletion of a tombstone never releases its obsolete slot. Legacy raw
  release calls scan live metadata ownership and reject owned slots. Repeated
  release of a free slot is a no-op. Startup rejects duplicate live owners,
  owned slots on the free list, invalid slot extents, free-list cycles and
  inconsistent counts. Empty existing vector files are corruption, not new files.
- Metadata's publication gate spans the transaction and phase bitmap update.
  Phase scans hold its read side, check actual record phase, and enumerate
  tombstones from canonical records. Imports publish their actual phase. Batch
  replacements preserve identity/slot ownership and update secondary indexes;
  they cannot resurrect tombstones. Legacy checkpoints always rebuild on open.
- All legacy text compaction entry points return an error. Any legacy marker
  or `.new` file blocks startup without deleting or renaming evidence. Text
  reads check referenced extents before allocating a buffer. Clean existing
  stores cannot silently initialize absent canonical files.
- Engine, backup and restore share `<target>.recalld-owner.lock` in the
  canonical parent directory. They also acquire the legacy `recalld.lock`
  inside an existing target. The sibling lock is deliberately never unlinked.
  Target symlinks are rejected. The parent directory must be writable.
- Backup requires a quiescent store even with `force`. It derives the file
  inventory from validated metadata and publishes a synced temporary archive
  by rename. New archives omit derived FTS; startup rebuilds it.
- Restore stages and validates the whole archive before replacing the target.
  It rejects incomplete inventories (including omitted namespace files),
  duplicates, undeclared paths, links, special files, size/CRC mismatches and
  invalid canonical references. redb 2.x inspection uses disposable copies
  with repair aborted, since opening redb writes bookkeeping. Source bytes
  remain unchanged. Text checksums, vector ownership/dimensions, namespace
  identity and forward/reverse edge integrity are checked.
- Restore never stops the default daemon speculatively. Stop the process
  owning the requested target before restoring; neither `force` nor the
  legacy `no_stop_daemon` option bypasses ownership. Imported FTS is discarded
  after validation so normal startup rebuilds it from canonical records.

## Interrupted restore

Before either rename, restore syncs a sibling
`<target>.recalld-restore-pending` JSON file naming `target`, `staged` and
`previous`. The staged directory and any previous store are retained across
publication errors. Startup and subsequent backup/restore refuse a target
with that marker. Successful publication validates the target, removes the
marker and syncs the parent. Previous stores remain in UUID-suffixed `.bak-`
directories; there is no automatic retention cleanup.

This marker is a fail-closed containment barrier, not automatic journal
replay. Recovery must independently validate and select a complete copy under
ownership before clearing it. Do not run older binaries during restore:
existing older writers are excluded by the legacy lock, but old binaries
starting during the rename gap do not understand the sibling lock/marker.
No filesystem power-loss guarantee is claimed by these tests.

## Next-stack requirements

1. Add the shared canonical control/owner/receipt/effects schema and mutation
   gate. Preserve the publication lock order: engine gate -> metadata
   publication -> redb transaction -> phase bitmap. Make uncertain commit and
   post-commit publication failures unavailable until reconciled. The legacy
   insert path intentionally does not reclaim payloads after commit errors.
2. Replace O(n) live-owner release checks and cached free lists with canonical
   owner IDs/allocation epochs, owner-derived allocation, and copy-on-write
   vectors. Reclaim leaked reservations only after resolving commit outcomes.
   This slice detects legacy damage; it cannot reconstruct damaged embeddings.
3. Implement metadata-selected text generations, descriptors and durable
   generation publication before re-enabling compaction. No marker ordering
   makes the old protocol safe. Add generation-bound checkpoint envelopes and
   coherent engine-wide read views; current phase scans alone are serialized.
4. Make namespace creation/purge and metadata/edge/index effects transactional
   or recoverable. Existing namespace publication and multi-file mutations
   remain outside the complete receipt/readiness contract.
5. Add backup v2 SHA-256/schema/lineage inventories, removal-ledger merge,
   historical versus retained-state restore, resumable restore journal replay,
   capacity preflight and the full crash/I/O-fault matrix. Current legacy
   restore can replace newer state and has no deletion-aware guarantee.
   Coordinate CLI help for the stricter force/quiescence behavior with A4.

## Validation

Synthetic temporary stores only, in `recalld-ci:1.94` (Rust 1.94.1), with
`--network none --cpus 2 --memory 4g`, `CARGO_BUILD_JOBS=2`, and private copied
Cargo caches/target output. No home, credentials or Docker socket mounts.

Result: 514 tests passed; four existing doctests remain ignored. Clippy and
format checks completed successfully; clippy warnings remain (including the
existing large storage-error type). No performance or power-loss gate was run.

Commands: `cargo test --offline --all-features -- --test-threads=2`,
`cargo clippy --offline --all-targets --all-features`, and
`cargo fmt --all -- --check`. Tests include a separate-process ownership
check, rejection/no-mutation cases, slot reuse and double-free, corrupt
free-list and canonical files, phase publication scheduling, checkpoint
rebuild, archive completeness, unchanged validation source bytes, and a
text/vector restore round trip. Restore-marker tests check fail-closed reopen;
they do not simulate kills at every rename/fsync boundary or power failure.

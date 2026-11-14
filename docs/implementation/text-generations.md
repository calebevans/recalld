# Generation-selected text compaction

The existing redb/file backend now selects text through metadata. This is physical text maintenance, not a complete store-wide CAS or ready-write API. `compact_text_log` is the actual engine path. No backend replacement, public receipt contract, removal ledger, batch protocol or derived-readiness guarantee is enabled here.

## Publication and recovery

Under the caller's exclusive engine write guard, compaction validates every retained text reference, including references outside Full phase. It writes a never-reused `fulltext.<uuid>.dat`, validates its payload, syncs the file and parent directory, then commits **one redb transaction** containing all relocated offsets, the descriptor, active selection, metadata generation increment and a catalog-scoped `TextGenerationSelected` receipt. Empty compaction also commits a new selection. Descriptor history and receipts remain in metadata; generated file identities are not semantic request-hash inputs.

The additive `text_generations_v1` table has one `active` key and UUID-keyed descriptors. A descriptor contains generation UUID, store ID, exact safe filename, text format version, sealed-prefix length and SHA-256. A version-2 text header is 48 bytes: MEMT, little-endian version 2, ten zero reserved bytes, store UUID and generation UUID. Entry framing remains length/CRC32/UTF-8. Appends leave the sealed prefix unchanged; all reachable tail entries are checked for bounds, length, CRC and UTF-8 on reopen. Historical descriptor files may have been reclaimed; only the active file is required.

A prepared, validated handle replaces the old handle after the metadata commit while the write guard remains held. Publication is an infallible assignment. An uncertain commit or injected publication failure leaves admission unavailable until reopen; the receipt operation ID identifies an uncertain commit. No payload is freed because a commit returned an I/O error. Metadata selects recovery: neither a filesystem marker nor a newer-looking orphan can override it.

Startup preflights the selected descriptor/header/seal, rejection gate and every referenced extent through a disposable metadata copy before opening any canonical database for writing. Failed text preflight leaves canonical files byte-for-byte unchanged. Effects replay and orphan cleanup happen later. Missing, corrupt, unsafe or unsupported selection fails closed. Backup inventory includes the selected generation and the legacy format gate; restore validates that inventory from metadata even when an archive omits the selected payload from its manifest.

Cleanup follows publication and reader drain. Owned version-2 orphan files qualify only when safe filename, header generation UUID and store UUID agree. Symlinks, foreign files, malformed lookalikes and unidentifiable legacy copies are retained. Cleanup failure is logged and retried on a later successful open/compaction; it does not undo or misreport the canonical selection. A historical selected version-1 intermediate is accepted only with a valid rejection gate and is never removed as an orphan. Unreferenced malformed append tails are reclaimable; the removed-entry statistic counts complete framed entries, not arbitrary corrupt tail bytes.

## Legacy compatibility and deployment

Retire old binaries/processes and take a complete quiescent backup before cutover. New empty stores write and sync their rejection gate before committing the initial selection. An interrupted initial creation can fail closed as incomplete; it is never guessed into a selected state.

**In-place upgrade of a legacy layout is refused.** A stopped legacy store is migrated through the real offline CLI:

```sh
recalld-text-migrate /absolute/legacy-store /absolute/fresh-destination
```

The destination must not exist and must be outside the source. Its parent must exist. The command acquires source and destination ownership plus the legacy source lock; it never contacts a daemon/provider. It validates the complete source, copies canonical files into an inaccessible sibling staging directory, writes and syncs relocated text, installs the rejection gate **before** the selection transaction, validates the complete gated result, and then renames staging to the fresh destination. Every prepublication failure leaves the source unchanged and the destination absent. Interrupted staging directories may remain for explicit operator cleanup. An interrupted postpublication response is reconciled by inspecting the destination and its durable operation receipt, not by overwriting it.

Existing lineage, namespace IDs, records, history and prior receipts survive. The text selection is a maintenance mutation with one new receipt and generation increment. A legacy source without a control lineage receives a new lineage in the destination only. No retained content is reconstructed or normalized. The command reports storeId, sourceGeneration and the committed receipt. Validate the destination, then explicitly change the service configuration; keep the old source offline because both copies retain the same lineage. The CLI does not activate either copy or replace a serving source.

Clean legacy stores remain readable/writable in their original layout; `compact_text_log` returns a recovery diagnostic without creating a selected copy or changing metadata. Ambiguous `.compacting`, `.compaction-meta-committed` or `fulltext.dat.new` artifacts still block open without deletion or guessing.

The permanent `fulltext.dat` is a 48-byte rejection gate, never a payload or selection authority. Its magic, version, reserved bytes, store identity and nonnil generation identity are validated on open and backup/restore. Invalid or legacy-readable gates are rejected, not silently repaired on a serving store. Statistics based on that filename alone no longer describe text payload size. Historical unidentified legacy copies may contain forgotten text and require explicit cleanup.

An upgraded store cannot be opened for writing by old binaries. Do not remove the gate or rename a generation to `fulltext.dat`. Downgrade requires an offline, validated export into a fresh layout supported by the old engine; that reverse-export tool is not supplied by this slice. Restoring an older backup loses newer writes and is not a lossless downgrade. Deletion-aware restore and lineage-fork semantics remain separate prerequisites.

## Validation and remaining scope

Tests use isolated synthetic fixtures in `recalld-ci:1.94`, Rust 1.94.1, network none, two CPUs, 4 GiB and two Cargo build jobs. They cover process exit at seven ordinary-compaction boundaries and six offline-migration boundaries (copy, payload sync, gate, selection, full validation and fresh-destination publication), twice-repeated reopen, dead text preceding multiple live entries, non-Full retained text, empty/repeated compaction, corrupt/missing selection, unsafe descriptors, stale generation, receipt persistence, orphan ownership and restore completeness.

Process exit tests exercise interrupted publication, not arbitrary hardware power loss. They do not establish disk-full/real fsync-failure behavior on every supported filesystem. Existing APIs still expose bare text offsets; callers must retain the shared engine read guard while resolving metadata and reading text, and must not cache offsets across a compaction write guard.

The shared sticky unavailable gate from `008ea074` is integrated. Still required: effects replay and all canonical mutation consumers and receipt/outbox participation; removal ledger and deletion-aware restore; record/schema-v2 migration and reverse export; persisted batch manifests; derived-index generation/readiness barriers. No public full-store generation/CAS claim follows from this metadata counter. The separate effects owner retains canonical-boundary integration responsibility.

Initial validation before independent review on the admission-gate parent `008ea074`: `cargo test --offline --locked --all-features` passed 542 tests, with four ignored doctests. `cargo clippy --offline --locked --all-targets --all-features` and `cargo fmt --all -- --check` passed. Clippy retains the repository's warnings, including `result_large_err` on new functions using the existing `StorageError`; this is not a warning-free or `-D warnings` result. Raw local validation output: `/tmp/recalld-lifecycle-review/compaction-validation.log`. Tests include the simulated durable-but-reported-unknown commit, receipt reconciliation after reopen, header identity independent of prefix hash, and symlink orphan preservation.

## Independent review corrections

Review of `7281918` reproduced three failures: a legacy writer could mutate the original after copy selection but before fencing; missing selected payload changed source meta.db before failing; and a self-consistent archive could publish a legacy-readable gate. The revised implementation removes that in-place upgrade protocol, preflights text before writable opens, and validates the gate during archive semantics checks. Regression tests invert each demonstrated failure, including byte-for-byte failed-open preservation and a forged archive with recomputed checksums. The offline CLI is exercised as an actual subprocess.

Review-fix validation: 547 library tests and one real CLI integration test passed; four doctests remain ignored. All-feature/all-target Clippy and formatting passed with warnings. The seven compaction and six offline-migration subprocess exits all passed. Raw output: `/tmp/recalld-lifecycle-review/compaction-review-validation.log`. Ordinary startup allows redb's normal unclean-exit recovery only on the disposable preflight copy before validating text; strict backup inspection still aborts repair.

Effects integration order is unchanged: preflight performs raw inspection only; constructor record/namespace scans must use the effects owner's recovery-only helpers, then `recover_pending_effects` precedes startup cleanup. Offline migration calls `canonical_files(source)` before writable staging work; retain the effects validator's pending-canonical-work rejection in that shared path. This branch does not bypass pending admission or apply effects during inspection.

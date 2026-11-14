# Generation-selected text compaction

The existing redb/file backend now selects text through metadata. This is physical text maintenance, not a complete store-wide CAS or ready-write API. `compact_text_log` is the actual engine path. No backend replacement, public receipt contract, removal ledger, batch protocol or derived-readiness guarantee is enabled here.

## Publication and recovery

Under the caller's exclusive engine write guard, compaction validates every retained text reference, including references outside Full phase. It writes a never-reused `fulltext.<uuid>.dat`, validates its payload, syncs the file and parent directory, then commits **one redb transaction** containing all relocated offsets, the descriptor, active selection, metadata generation increment and a catalog-scoped `TextGenerationSelected` receipt. Empty compaction also commits a new selection. Descriptor history and receipts remain in metadata; generated file identities are not semantic request-hash inputs.

The additive `text_generations_v1` table has one `active` key and UUID-keyed descriptors. A descriptor contains generation UUID, store ID, exact safe filename, text format version, sealed-prefix length and SHA-256. A version-2 text header is 48 bytes: MEMT, little-endian version 2, ten zero reserved bytes, store UUID and generation UUID. Entry framing remains length/CRC32/UTF-8. Appends leave the sealed prefix unchanged; all reachable tail entries are checked for bounds, length, CRC and UTF-8 on reopen. Historical descriptor files may have been reclaimed; only the active file is required.

A prepared, validated handle replaces the old handle after the metadata commit while the write guard remains held. Publication is an infallible assignment. An uncertain commit or injected publication failure leaves admission unavailable until reopen; the receipt operation ID identifies an uncertain commit. No payload is freed because a commit returned an I/O error. Metadata selects recovery: neither a filesystem marker nor a newer-looking orphan can override it.

Startup validates the selected descriptor/header/seal and every referenced extent before effects replay or orphan cleanup. Missing, corrupt, unsafe or unsupported selection fails closed. Backup inventory includes the selected generation and the legacy format gate; restore validates that inventory from metadata even when an archive omits the selected payload from its manifest.

Cleanup follows publication and reader drain. Owned version-2 orphan files qualify only when safe filename, header generation UUID and store UUID agree. Symlinks, foreign files, malformed lookalikes and unidentifiable legacy copies are retained. Cleanup failure is logged and retried on a later successful open/compaction; it does not undo or misreport the canonical selection. A selected version-1 intermediate is never removed as an orphan. Unreferenced malformed append tails are reclaimable; the removed-entry statistic counts complete framed entries, not arbitrary corrupt tail bytes.

## Legacy compatibility and deployment

Retire old binaries/processes and take a complete quiescent backup before activating the generation layout. New stores register their initial generation during creation. Existing clean legacy stores stay readable in their existing layout until the actual compaction path upgrades them. Ambiguous `.compacting`, `.compaction-meta-committed` or `fulltext.dat.new` artifacts still block open without deletion or guessing.

A clean legacy upgrade has two explicit metadata commits:

1. Copy and sync legacy bytes to a generation-named file, preserving offsets. Register it as the selected format-1 descriptor with a receipt. The original `fulltext.dat` remains intact. If interrupted here, metadata selects the copy.
2. Install an unsupported-version header at `fulltext.dat` by durable replacement, so old text readers reject the upgraded layout. Then compact into the format-2 generation and commit the new offsets/selection/receipt. If interrupted before this second transaction, the selected legacy copy remains usable. Reopen finishes the rejection gate without using it to infer selection.

The temporary format-1 descriptor intentionally has a legacy header, without embedded identity; its metadata filename and seal are authoritative for reads, but its bytes are insufficient for automatic orphan deletion. Such abandoned copies may retain forgotten text and require explicit operator cleanup. No `.new` payload rename or gate file is commit authority. The permanent `fulltext.dat` file is only an old-reader rejection gate; statistics based on that filename alone no longer describe text payload size.

An upgraded store cannot be opened for writing by old binaries. Do not remove the gate or rename a generation to `fulltext.dat`. Downgrade requires an offline, validated export into a fresh layout supported by the old engine; that reverse-export tool is not supplied by this slice. Restoring an older backup loses newer writes and is not a lossless downgrade. Deletion-aware restore and lineage-fork semantics remain separate prerequisites.

## Validation and remaining scope

Tests use isolated synthetic fixtures in `recalld-ci:1.94`, Rust 1.94.1, network none, two CPUs, 4 GiB and two Cargo build jobs. They cover process exit at ten boundaries (legacy copy sync/selection, gate installation, entries written, file sync, directory sync, metadata commit, handle publication, before cleanup and after unlink), twice-repeated reopen, dead text preceding multiple live entries, non-Full retained text, empty/repeated compaction, corrupt/missing selection, unsafe descriptors, stale generation, receipt persistence, orphan ownership and restore completeness.

Process exit tests exercise interrupted publication, not arbitrary hardware power loss. They do not establish disk-full/real fsync-failure behavior on every supported filesystem. Existing APIs still expose bare text offsets; callers must retain the shared engine read guard while resolving metadata and reading text, and must not cache offsets across a compaction write guard.

The shared sticky unavailable gate from `008ea074` is integrated. Still required: effects replay and all canonical mutation consumers and receipt/outbox participation; removal ledger and deletion-aware restore; record/schema-v2 migration and reverse export; persisted batch manifests; derived-index generation/readiness barriers. No public full-store generation/CAS claim follows from this metadata counter. The separate effects owner retains canonical-boundary integration responsibility.

Final validation on the admission-gate parent `008ea074`: `cargo test --offline --locked --all-features` passed 542 tests, with four ignored doctests. `cargo clippy --offline --locked --all-targets --all-features` and `cargo fmt --all -- --check` passed. Clippy retains the repository's warnings, including `result_large_err` on new functions using the existing `StorageError`; this is not a warning-free or `-D warnings` result. Raw local validation output: `/tmp/recalld-lifecycle-review/compaction-validation.log`. Tests include the simulated durable-but-reported-unknown commit, receipt reconciliation after reopen, header identity independent of prefix hash, and symlink orphan preservation.

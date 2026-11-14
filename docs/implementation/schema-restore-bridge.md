# Offline schema/effects restore bridge

Actual dependency merge `34fa377` joins accepted restore `4abc959` (PR44) with
minimal core join `1ca36c4`: ledger PR38, effects PR39, and schema PR40 including
publication proof `4391118`. Selected-text PR34's real old-reader rejection fence
remains an inherited prerequisite. No full integration branch was imported.

This bridge permits validated schema-v2 snapshots and already-applied effects
through the existing private-staging restore consumer. Ordinary schema-v2 engine
and metadata opens remain blocked. Restoring a v2 snapshot does not enable its
writers, serving endpoints, lifecycle policy or embedding provider.

## Origin and immutable migration proof

`control::permits_generation_origin(read, control, origin)` delegates to
`lineage::permits_origin`. That reader validates the independent lineage-version-1
marker without recursively invoking control validation. Generations are compared
only inside their own store ID.

The canonical schema singleton is left byte-for-byte unchanged through the fork:
source GenerationRef, publicationGeneration, migration ID, receipt operation ID
and migration timestamp all retain their original meaning. Historical receipt/key
rows retain their original bytes and UUID-scoped keys. Schema validation uses the
original source UUID's receipt key and verifies bounded provenance. Format-only
publication must equal the source generation; attested publication must equal
source + 1 and the attestation receipt generation, with positive attested count.
Deleting an attestation pointer after a fork still fails this exact proof. No
current-counter comparison across store IDs is introduced.

## V2 content masks

Reconciliation uses checked `DiskRecord::encode` before commit:

- Full-text loss clears references. Summary loss clears summary; independent
  labels remain unless the record becomes Ghost or its tags were removed.
- Tag loss clears both legacy tags and all structured entities/topics/emotions.
  Ghost/Tombstone always clear structured labels. Legacy lifecycle-v1 Ghost tags
  remain when the ledger did not remove them; schema version alone is not a
  lifecycle policy switch.
- Tombstones clear all masked content, zero strengths, and remove the structured
  vector reference. Hard deletion removes the entire row. Retained vectors keep
  their exact slots, hashes and allocation epochs. Nonterminal embedding removal
  remains unsupported and fails before publication.
- Other lifecycle fields, observation/history and timestamp provenance remain
  unchanged. A payload-free ledger contains no phase-change time; restore does
  not invent one. Nonterminal materialized strength is preserved rather than
  inferred from a phase or projected under a new clock.
- If a retained record needs destructive reconciliation but contains unknown
  optional record extensions, restoration fails closed. Untouched extensions
  round-trip exactly. A versioned extension removal consumer is required before
  interpreting such bytes; hard-deleting the whole row retains no extensions.

Ledger validation now rejects retained structured labels under a tag-removal mask
or a vector reference under an embedding-removal mask. The schema codec separately
rejects Ghost/Tombstone labels and terminal ownership contradictions.

## Applied effects consumption

Both source and retained target must pass the existing effects validator against
original receipt IDs and original control. Unapplied canonical effects are
rejected by backup/extraction and rechecked in reconciliation. Their presence is
never permission to replay writes during read-only inspection.

In inaccessible staging, the consumer reads the validated applied rows before
forking. It applies ledger removals, repairs canonical edges/vector ownership,
rebuilds tag/namespace and phase indexes, and builds/fsyncs the reconciled FTS
index. Only then does the fork transaction remove those applied outbox rows.
The new control, records, ledger and row consumption commit together; every
historical receipt/key remains. No effects validator is weakened to accept
arbitrary foreign operations, and old graph puts are not replayed against
records removed during reconciliation.

The target's newer applied operations contribute their ledger and receipt
history; restoring a snapshot still rolls back graph changes outside removal
protection. New lineage invalidates all old in-memory views, which must be built
from reconciled canonical data at ordinary startup (still disallowed for v2).
This offline consumption is not a public ready receipt or a whole-store serving
barrier. An FTS rebuild failure aborts the metadata fork and leaves the original
applied rows/receipts in private staging; the retained target is unchanged.

Engine startup can finish a pending schema-v2 restore journal and then rejects
ordinary v2 serving. Inspection itself remains nonrecovering. The schema marker
and receipt ancestry are validated during recovery before any publish/inspection.

## Evidence and remaining gates

Synthetic offline-container tests exercise actual migration, quiescent backup,
retained/fresh historical restore and `inspect_schema`/`OfflineSnapshot` reads:

- Format-only and attested snapshots retain exact schema proof and every source
  and retained-target receipt; a repeated ancestor restore also succeeds.
- Applied edge effects are present in the actual archive, newer real legacy
  writers produce Ghost/Tombstone/hard-delete ledger entries, and staging
  consumes the applied rows while removing their obsolete graph endpoints.
- Independent text/summary/tag masks preserve unremoved fields; structured
  labels, terminal vector ownership, FTS and peer counts match retained content.
- Historical pointer stripping, source gaps, zero attested count, unknown origin,
  insufficient ancestry bound and wrong receipt origin all fail inspection
  without source writes.
- Fresh historical destinations are inspectable but not ordinarily served.
  Schema journal interruption/reopen finishes safely without enabling v2.
- Real SQLite rebuild failure preserves pending effects and the retained target;
  unapplied effects fail real backup and CRC-consistent forged-archive restore.
- Opaque optional extensions are preserved unchanged or reject destructive
  reconciliation before publication.

These are returned-error/reopen tests, not power-loss or process-kill matrices.
The existing strict v1 CRC archive inventory and complete semantic validation
remain in use. Feynman owns the forthcoming ManifestV2/lossless-export registry
integration; this branch does not claim those formats complete. Ordinary v2
writer participation, serving readiness, historical promotion and unknown
extension-removal semantics remain gated.

Validation used isolated Docker `recalld-ci:1.94`, Rust 1.94.1, `--network none`,
2 CPUs / 4 GiB, two build jobs, and this worktree's private cargo/target caches.
All 28 focused restore tests passed. The full suite passed 635 library tests and
one CLI test; four doctests were ignored. Formatting passed; all-feature,
all-target Clippy completed successfully with warnings (including the shared
large-StorageError lint on new fallible helpers). No network/provider or personal
store was used.

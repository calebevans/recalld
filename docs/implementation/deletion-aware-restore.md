# Deletion-aware restore of the gated v1 layout

This implementation depends on removal-ledger `214d719` (including atomic
lifecycle and the graph revalidation fix) and selected-text/fence PR34
`838cafc`. Merge `5672ab0` records both actual parents. It does not depend on
schema-v2, effects/outbox, or lossless-export branches.

## Modes and compatibility

Default restore requires a quiescent existing target. Under its stable sibling
ownership lock and legacy directory lock, it captures validated control,
lineage, removal ledger, receipt/key rows and the namespace ID counter. The
snapshot must share the target's store ID or be within a verified ancestor
bound retained by the target. Unrelated stores, ambiguous siblings, missing or
unreadable evidence and unsupported versions fail before replacement.

Both target and snapshot require a selected text generation with the real
old-reader rejection header supplied by PR34. Additive tables/control markers
alone do not exclude pre-foundation binaries. An ungated legacy store is never
gated in place: use the explicit offline text migration into a fresh destination.
Evidence before a gated restore retains unknown prior coverage; this code cannot
infer deletions bypassed by older writers or lost from all retained copies.

`restore --historical` requires a fresh destination. It records historical mode
and unknown later deletion coverage. Ordinary engine and metadata opens reject
it before writable canonical opens; it cannot be implicitly promoted by another
restore. Read-only inspection/copying is separate from ordinary serving. There
is no promotion command in this slice.

## Reconciliation and lineage

All work occurs in inaccessible sibling staging before directory publication:

- Union compatible ledger masks; hard delete dominates tombstone. Preserve
  origin lineage, namespace ID, UUID, operation ID and per-origin generation.
- Apply terminal records and field masks, preserving v1 Ghost tags. Release
  terminal vector slots without freeing slots attributed to already-tombstoned
  records; remove incident edges and recompute surviving outgoing counts.
- Copy only retained text into a new selected generation. Rebuild tag/namespace
  indexes and FTS from retained content; discard the phase checkpoint. A failed
  rebuild prevents publication. The ordinary engine builds its in-memory views
  from reconciled canonical data; this is not a public readiness/barrier claim.
- Allocate a fresh store ID and generation 1, preserving the maximum namespace
  ID counter. Preserve historical receipt and retry-key rows byte-for-byte under
  their original IDs. Old-lineage mutation contexts are rejected. Historical
  keys are never rebound to the new lineage; a request explicitly addressing the
  new lineage is a new operation.

`restore_lineage_v1` is a strict singleton table, marked by control field
`restoreLineageVersion: 1`. It records mode, new ID, restore operation, snapshot
and captured-ledger GenerationRefs, archive SHA-256, bounded ancestors and
source ledger coverage. `lineage::read_lineage(read, control)` validates this
state without invoking control validation recursively.
`lineage::permits_origin(read, control, origin)` accepts active or verified
ancestor generations only within their own store ID. An ancestor ceiling is an
upper bound, never an exact migration publication counter. Binary backups retain
this table and all historical receipt/ledger rows inside `meta.db`.

## Publication and recovery

The versioned sibling restore journal durably retains the captured target
snapshot, new identity, archive hash, sibling paths and SHA-256 inventory of
staged canonical files plus the rebuilt, closed FTS database. The archive is
pinned to one private copy before extraction and hashing. Strict archive and
selected-generation inventory validation remain in place.

Publication fsyncs staged files/directories and journal, retains the old target
by rename, then publishes staging and removes the journal only after validation.
Engine startup takes the stable lock and directory locks before recovering.
Ordinary ownership acquisition for inspection, backup or another restore rejects
a pending journal without publishing it. Recovery
checks actual directory identities and sealed contents, including when a rename
preceded its journal-state update. It finishes the complete candidate, or, if
staging is missing after the old rename, restores the validated old target and
reports that rollback before a subsequent open. Ambiguous/corrupt evidence fails
closed while preserving copies. The old target is retained after successful
publication; old archives and retired copies are not forensically erased.

## Integration gates and remaining scope

This branch explicitly rejects control/schema-v2 and unknown canonical tables,
including effects/outbox state. It must not be advertised as production v2
restore. Main owns an integration child with Feynman's fixed schema parent:

1. Compose `control::permits_generation_origin` with
   `lineage::permits_origin`; keep original schema source, receipt IDs and scoped
   receipt keys. Align control version 2 with the independent lineage version 1.
2. Resolve the historical missing-attestation-pointer proof before accepting v2.
   A `<= ancestor bound` check alone cannot establish whether an absent receipt
   pointer is valid. Feynman's `4391118` introduces required
   `publicationGeneration` as exact schema publication evidence; preserve it
   unchanged and validate its source-origin bound and receipt relationship;
   never rewrite provenance or compare counters across store IDs.
3. Extend the explicit canonical-table/record reconciliation consumers and call
   the owner's `backup::schema_manifest::validate_manifest_snapshot` after strict
   extraction/inventory validation. Preserve canonical v2 binding/lifecycle data
   and pending effects; do not merely allow unknown tables through. In the
   effects branch, canonical inventory may contain `canonical_applied=true`
   effects still awaiting derived publication. Its current validator assumes
   active-store receipt identity/key. The fork consumer must validate their
   original receipt origin/key via verified ancestry and rebuild/acknowledge
   derived effects, or explicitly consume them in staging. Never rewrite their
   historical receipt IDs. Add actual backup → forked restore tests with these
   rows plus invalid-origin/receipt negatives in the combined integration.
4. Huygens' integrated tests must exercise actual format-only and attested
   migration → forked restore → inspect, preserving historical receipts. Reject
   stripped historical receipt pointers, source gaps, zero attested counts,
   unknown origins and out-of-bound/wrong-origin receipts. Ordinary v2 serving
   and writes remain blocked until all writers participate.
5. Register lineage in the owner's lossless logical-export inventory. Full
   restore ancestry/merge policies beyond verified ancestors, historical
   promotion, kill/power-loss matrices and filesystem portability are later work.

## Validation

Synthetic stores only, isolated Docker `recalld-ci:1.94`, Rust 1.94.1,
`--network none --cpus 2 --memory 4g`, two build jobs and private cargo/target
caches. Focused tests cover real retained-state restores, queryable reconciled
FTS, historical open rejection, terminal UUID reuse, purge, mask persistence,
old receipt identity/retry behavior, repeated ancestor restore, all four durable
journal boundaries and a second reopen, missing-stage rollback, corrupt journal
or lineage/FTS evidence, legacy rejection and explicit fresh text migration.
Boundary tests inject returned errors after durable steps and exercise actual
reopen recovery; they are not process-kill or power-loss experiments. The legacy
text-reader rejection test exercises the header parser, not a separately built
old executable. Validation: 20 focused restore tests passed; the full suite passed 590 library
tests and one CLI test, with four ignored doctests. `cargo fmt --all -- --check`
passed. All-target/all-feature Clippy completed successfully with warnings
(including the existing large `StorageError` lint on the new fallible helpers).

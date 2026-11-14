# Native lifecycle transactions

This bounded child of PR57 (`7bc4099`, guard `65edaa3`, evidence `129b701`)
enables explicit native exposure and feedback on existing canonical schema-2
stores. It does not activate ordinary schema-2 serving. No catalog, insertion,
purge, maintenance, transport, or whole integration branch is a parent.

## Entry and ownership

`storage::native_lifecycle::NativeLifecycleSession::open(path, policy, clock)`
acquires the existing store ownership and directory locks, validates the complete
canonical inventory (including the selected-text old-reader fence), and opens
the existing `MetadataStore` in native-lifecycle mode. Historical inspection
stores, legacy stores, unknown layouts, and competing owners are rejected.

The session is a thin offline ownership wrapper. Actual capture and commit
primitives are methods on `MetadataStore`, behind its existing publication lock
and redb database. An already-owned future engine must use those primitives;
it must not create a second session or store for each request. Other metadata
writers are rejected on this handle. Private offline migration/restore staging
continues to publish its complete canonical transaction through its existing
APIs. Defaults, public HTTP/MCP/daemon requests, and ordinary engine admission
are unchanged.

The generic metadata opener now checks record, namespace, and control format
consistency on a disposable copy before writable open. A valid schema-2 record
under legacy control is corruption, even if no schema marker remains. Unknown
namespace versions also fail. This check does not silently adopt mixed formats.

## Internal API and intent

- `capture_exposure(scope, &[(id, kind)]) -> ExposureIntent`
- `record_exposure(context, &intent) -> Option<WriteReceipt>`
- `capture_feedback(id, namespace_assertion, quality) -> FeedbackIntent`
- `apply_feedback(context, &intent) -> WriteReceipt`
- `lookup_operation(store_id, namespace_id, operation_id) -> Option<WriteReceipt>`

Typed native write contexts require contract version 2, independently of the
namespace's selected lifecycle policy. Contract 1 is rejected before transaction
preparation and clock sampling. Capture rechecks handle availability after
waiting for publication, so an uncertain prior commit cannot hand out an intent.

Capture reads namespace policy and eligible canonical records in one actual
read transaction. Its private preconditions bind record tokens, policy version
and revision, and resolved configuration. The optional source ID is also guarded
without becoming an exposed result. Direct exposure wins duplicate associative
exposure. Empty batches create no receipt or generation and do not sample time.

`capture_generation()` is the actual capture transaction's generation. It is
**not** a whole-query read generation or an effects readiness watermark. The
crate-private capture-at-read-transaction primitive provides the future adapter
binding point. Halley's current consumer does not yet have the coherent A1
query read view needed to use it as a query snapshot.

The request hash includes stable explicit caller IDs/kinds/scope, quality, and
caller context/CAS. Generated tokens, capture generation, resolved policy, and
server time are preconditions or outcomes, not retry identity. Lineage and
scope validation precede replay; matching replay precedes fresh token/policy/
CAS/clock checks. Changed original intent conflicts. Opaque intents are not a
public wire format.

This is **not public keyed-search semantics**. Future keyed search must hash
stable caller query intent, look up replay before fresh selection, then rehydrate
current visibility. Server-selected result IDs are outcomes. An adapter must not
reuse the explicit-ID exposure hash to claim keyed query replay.

## Atomic behavior

Each nonempty operation samples the injected clock once after final checks.
Each record clamps that sample to its lifecycle update timestamp. The final
redb transaction changes lifecycle fields, bounded access history, record token,
committed generation, and typed receipt together. A commit error poisons the
handle and returns the operation ID for reconciliation after reopen. Matching retries return the
original receipt without another sample or write. Stale policy, source/result
tokens, wrong lineage/namespace, invalid quality, missing/terminal records, and
unrepresentable time fail explicitly.

Exposure updates retrieval/update timestamps and history without changing
stability, phase, permastore, observation provenance, or retained payload.
`legacy_access_v1` also advances the anchor; `explicit_feedback_v2` preserves it.
Feedback uses the shared planner and the canonical anchor under the captured
namespace policy. It changes stability and feedback/anchor/update timestamps,
sets compatibility strength, and preserves sticky permastore and retained phase.
Summary feedback does not recreate full text. Ghost and Tombstone are ineligible.
The compatibility last-access value follows the selected policy; observation,
phase-change timestamp, and legacy attribution are not fabricated.

The publication lock covers final revalidation, transaction preparation, and
commit. These operations do not change phase membership or payload indexes.
This entry provides no live cache/search/graph readiness assertion. Future
online integration must publish/invalidate affected derived state and satisfy
its admission barrier before reporting queryable success.

## Typed proof and recovery

Only `LifecycleExposureRecorded` and `LifecycleFeedbackApplied` extend the
existing policy-migration suffix. Their proof contains numeric lifecycle before/
after states, history digests and at most one evicted numeric history event per
record. It contains no retained text, summary, labels, vectors, or feedback prose.
The shared planner verifies the transition; current-origin validation reverses
the contiguous suffix in commit order, including mixed policy migrations and
history eviction. Unknown operations, gaps, token/history mismatch, malformed
numeric transitions, and altered policy order fail closed.

The immutable schema publication marker and original migration/attestation
receipts are never rewritten. Baseline is original publication for its current
origin, or generation 1 for an active restored origin. Historical receipts keep
their original IDs and require ancestry and same-origin policy-order proof;
foreign generation counters are never compared. Their old record snapshots need
not equal current records after deletion-aware restore. A new origin cannot
replay old origin keys.

Actual backup/restore regression applies newer terminal removal evidence before
publishing the fork, preserves the old native receipt, accepts a new-origin
native write on a surviving record, and denies historical-mode writes. Mixed
native history survives policy rollback and format-only copy with immutable
publication provenance and source bytes. The lossless import implementation is
not a parent of this branch; its actual integration must retain all typed proof
and delegate canonical validation. No import integration result is claimed here.

## Validation and remaining scope

Tests use actual migrated synthetic stores and real redb transactions, including
an abandoned fully prepared transaction, an actual durable commit followed by
an injected I/O error and reopen reconciliation, same-key concurrency, stale source and
policy guards, malformed proof, bounded history eviction, rollback, reopen,
backup/fork, and historical mode. Raw edits occur only in explicitly corrupted
negative fixtures, never as valid native setup. The mixed-format regression
fails before the preflight fix and passes afterward.

Run `cargo test --offline`, `cargo fmt --check`, and
`cargo clippy --offline --all-targets` in `recalld-ci:1.94` with
`RUSTUP_TOOLCHAIN=1.94.1`, `CARGO_BUILD_JOBS=2`, `--network none --cpus 2 --memory 4g`.
Mount only this worktree and its separate scratch cargo/target. Final counts and
raw logs are recorded in the accompanying work-note evidence.

Remaining: coherent query snapshot binding, public keyed-query replay,
participating schema-2 insertion/forget/sweep/catalog/purge writers, profile
admission, derived publication/barrier integration, and actual lossless-import
join validation. Curie's final-transaction namespace-active guard belongs after
replay and before new namespace/record checks when that real parent is joined.
Unknown purge tables remain rejected by this branch's strict inventory; no purge
compatibility or broad concurrency guarantee is claimed.

# Transactional legacy exposure batches and feedback receipts

Minimal parent: `impl/atomic-lifecycle` at
`e0187fc5b24482b7bb5c6d2145ab293849e1129b` (foundation/admission and atomic lifecycle).
Branch: `impl/transactional-observation`. No integration, schema-v2, effects,
ledger or consumer branch was imported to manufacture dependencies.

## API and commit boundary

Both `MetadataStore` and concrete `RedbStorageEngine` expose:

```rust
record_exposure_batch(
    &self, ctx: &MutationContext,
    accesses: &[(MemoryId, AccessKind)], sampled_at: i64,
) -> Result<Option<WriteReceipt>, StorageError>

record_legacy_feedback(
    &self, ctx: &MutationContext, id: MemoryId, quality: u8,
    sampled_at: i64, global_multiplier: f64,
) -> Result<WriteReceipt, StorageError>
```

Exposure accepts DirectRetrieval/AssociativeRetrieval only, retains first
occurrence order, collapses identical UUID/kind duplicates, and rejects
contradictory kinds for one UUID. Canonical namespace existence/identity,
record key/UUID, record namespace, and non-tombstone phase are checked in the
same redb write transaction. A later invalid member rolls back earlier staged
members, generation, receipt and key together. Ghost exposure remains allowed,
as in the legacy single-record access path; this does not make Ghost content
eligible for retrieval.

A nonempty batch appends one event per distinct UUID and advances generation
exactly once, with one receipt for the namespace. It retains v1 anchor/history
semantics, not v2 exposure-only semantics. Each applied timestamp is
`max(sampled_at, created_at, last_accessed_at)`, preventing backward anchor
movement; the receipt records each actual timestamp. No new clock is sampled
inside the transaction. Existing single-record access behavior is unchanged.

Empty input validates the context, lineage, and namespace, then returns `None`:
no write transaction, generation, receipt, key reservation, or CAS check. It is
not a mutation/replay request. Consumers should skip exposure for zero results.

Feedback rereads the record and namespace policy inside its write transaction,
then calls the existing `resolve_policy` and `plan_feedback` with the frozen
server multiplier, default legacy decay configuration, and supplied clock sample.
The original atomic lifecycle helper and this new path share the same feedback
patch function: phase eligibility, monotonic timestamp, stability/strength,
permastore preservation, and bounded ManualReinforcement history semantics are
unchanged. Unrelated fields are retained from the current record. Phase does not
change, so no new phase-bitmap publication path is needed. The existing guarded
sweep/atomic helper remains intact.

These methods acknowledge canonical metadata commit only. They do not await
cache/graph/index publication, emit a public ready result, or implement the
later shared read view. Existing legacy adapters are not silently switched to
the new APIs: the actual awaited consumer join is a separate next slice.

## Retry identity and receipt encoding

Both use the existing control tables, scoped key namespace, `replay_or_check`,
`request_hash`, and `record_receipt`; there is no second retry store. Keys remain
`(storeId, namespaceId, key)` across operation kinds. The operation domains are
`record_exposure_batch` and `record_legacy_feedback`; reuse across these and the
existing `record_access` operation conflicts. Exposure hashes normalized ordered
UUID/kind pairs; feedback hashes UUID and quality. Context version, scope, and
expected generation retain the foundation hash rules.

Sampled server time, calculated patch values, and frozen server configuration are
excluded from request identity. A client-supplied time/configuration operation
would require a distinct normalized request contract. Replay checks lineage,
then request identity, before generation and current record/policy lookup; it
works after deletion/purge and preserves the original operation/generation/outcome.
An unkeyed call is a new operation, not retry-safe. Successful lookup/replay never
reconstitutes content from a receipt.

Receipt/hash versions remain 1, with two explicitly validated tagged outcomes:
`exposuresRecorded` carries distinct UUID/kind/applied-time entries;
`legacyFeedbackRecorded` carries UUID/quality/applied time. Neither stores
summary, tags, text, vectors, or a record snapshot. Invalid/duplicate/empty
exposure outcomes, invalid feedback quality, unexpected outcome fields and
catalog-scoped new outcomes fail validation. Older readers that do not recognize
these outcome tags reject them; do not silently downgrade/drop receipts for
rollback/export. This is additive control-schema-1 metadata, not record/schema-2
migration. New APIs explicitly reject contractVersion 2 and catalog scope.

A shared receipt commit helper also serves the existing single-access operation.
Commit uncertainty poisons admission and retains `CommitOutcomeUnknown` with the
operation ID; no assumed rollback or automatic new operation is returned.

## Actual next consumer join

- Await exposure after the final distinct returned UUID set is known, including
  MCP extra-neighbor hydration. Do not emit from both query and outer transport.
  Group multi-namespace fetches into ordered per-namespace mutations/results.
- Carry stable request identity/context and surface receipts/errors through
  HTTP/MCP/daemon adapters. Remove fire-and-forget exposure and swallowed errors.
  `recordAccess=false` must bypass these APIs and any attributable deferred work.
- Feedback consumers must call this fresh-state operation and publish/invalidate
  their derived views. Its receipt is canonical acknowledgement, not a cached
  feedback response or a coherent later content read.
- On the real ledger/effects/schema join, preserve the ledger hook and disposable
  startup validation. Extend the existing exhaustive outcome validator without
  dropping graph/text/catalog variants. Existing lifecycle ledger logic stays
  around its transaction; the shared feedback patch extraction must not remove it.
  New v2 record fields/policy behavior need their actual schema dependency.
- Whole-store CAS/readiness remain deferred until create/tombstone/batch/sweep/
  catalog/maintenance and other canonical writers participate. The foundation's
  `expected_generation` is still metadata-scoped, not public whole-store CAS.

## Validation

Runtime checks use temporary metadata stores only in `recalld-ci:1.94`, installed
toolchain 1.94.1, `--network none --cpus 2 --memory 4g`, two build jobs, and this
worktree's isolated `.validation/cargo` and `.validation/target`. No home,
credentials, Docker socket, personal memory, data or daemon mounts.

`cargo test --offline --all-features`: **568 passed**, no failures; four existing
doctests ignored. `cargo fmt --all -- --check`: passed.
`cargo clippy --offline --all-features --all-targets`: passed with warnings
(292 for lib-test, including inherited large-error/style warnings).

Ten new tests cover duplicate/order/time behavior, all-or-nothing late rejection,
fresh legacy feedback equivalence, replay before CAS, lineage and cross-operation
key conflicts, independent namespace keys, concurrent identical requests,
real prepared-transaction abandonment, generation exhaustion after staging,
invalid kinds/quality/v2 contexts, canonical phase eligibility, malformed receipt
startup rejection, and process exit immediately after committed exposure or
feedback followed by reopen and replay after purge. The process-exit tests are
not power-loss evidence. No fault-injected uncertain-I/O commit claim is made.
Logs remain untracked in `.validation/tests.log` and `.validation/clippy.log`.

Validated source commit: `5a72eb3a5b06f8a8cc9d1ca1f0641ab909a11328`.
The final validation annotation is documentation-only; no Rust source changed
between the successful checks and this commit.

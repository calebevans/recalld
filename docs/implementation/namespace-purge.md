# Durable namespace purge: bounded legacy writer slice

Runtime `f6e4d9263fe2d96fca16282eed47b1251ad5398b` on `impl/namespace-purge`.
Actual parents are published tombstone `dad8465` (runtime `1ae3d5a`) and catalog
`d4f756c`, including its retention endpoints and error mapping. Join correction
`97f0fd9` preserves both receipt validators and moves the schema-bearing
insertion fixture behind the now-stricter catalog boundary. No unrelated
maintenance/native/export/restore branches were imported.

The actual MCP/daemon namespace-delete caller now starts a durable operation,
then processes it synchronously before awaiting derived completion. It still
uses the legacy unkeyed transport contract. The engine also exposes:

```rust
begin_namespace_purge(ctx, force) -> PurgeProgress
resume_namespace_purge(store_id, namespace_id, parent_operation_id, chunk_limit)
    -> PurgeProgress
lookup_namespace_purge(store_id, namespace_id, parent_operation_id)
    -> Option<PurgeProgress>
```

Start checks lineage and matching replay before CAS, then checks canonical
namespace/default/schema/force predicates in the final transaction. Force is a
boolean contract default, included in request identity. Generated operation IDs
are excluded. The immutable start receipt authenticates the saved name hash;
progress cannot redirect cleanup by changing only its name. Namespace IDs are
never reused. Explicit native v2 remains rejected.

The lazy `namespace_purge_schema` and `namespace_purge_progress` tables use
version 1. Ordinary legacy opens do not create them. Progress retains IDs,
counts, cursor, receipts, reserved next child and name lease; no removed record
text, tags or embeddings. Start reserves the first child. Each child consumes
that reservation and reserves its successor in the same transaction that
removes records/tag/namespace rows, repairs peer counts, writes the removal
ledger, stores its immutable receipt and writes deterministic edge effects.
Canonical chunks contain 1..1000 UUID-ordered rows, including tombstones.
Incoming foreign edges are integrity cleanup, never cross-namespace retrieval.

The states are `Deleting`, `Cleanup`, `Complete`. After a final canonical
emptiness check, a separate receipt removes the namespace configuration and
enters Cleanup. The name remains leased while owned directory cleanup and root
fsync run. A separate final receipt marks Complete and leaves a derived outbox
entry even for an empty namespace. A matching completed replay returns the
original start receipt/current progress without new writes. Historical resume
never unlinks a recreated namespace using the old display name.

A store-wide atomic admission flag closes existing engine/metadata entry gates
while purge is active. It is restored from validated progress at startup. The
private purge guard bypasses only that flag, not uncertain-commit/canonical
failure admission. `namespace_state::require_namespace_active(tx, namespace)`
is the shared final-transaction join point; insertion consumes it. Ordinary
mutation publication is serialized with begin/resume, and direct existing
metadata writers cannot modify the deleting namespace through those gates.
This is entry admission, **not** a leased whole-query read view or proof that
independent raw EdgeStore/VectorStore handles participate.

Startup resumes saved work before orphan cleanup or ordinary admission.
`DerivedCompletion::complete` also resumes under its existing publication,
graph, index and storage lock order, then rebuilds/acknowledges every view.
Normal namespace deletion completes online. Errors retain durable work and
fail closed; storage I/O uncertainty requires reopen/reconciliation. Failed
online derived publication can retry completion without another mutation.
An already-typed current-child `CommittedMutation` or `CommitOutcomeUnknown`
passes through unchanged; only an otherwise unclassified failure is wrapped
with the last known progress receipt. The actual transport regression asserts
the current child ID, not merely an unavailable status.

## Validation and limits

Exact runtime: **698 library + 1 CLI passed**, zero failed; **4 doctests ignored**.
`cargo fmt --all -- --check` passed. `cargo clippy --offline --all-features
--all-targets` completed with 411 library / 431 test warnings including duplicates.
All used isolated `recalld-ci:1.94`, Rust1.94.1, network none, two CPUs, 4GiB,
two build jobs, separate Cargo/target copies; no home/credential/socket mounts.
The minimal parent has no `offline_eval` target, so none is claimed.

[Evidence and exact-source manifest](../evidence/namespace-purge/README.md)
cover multi-chunk reopen/replay, reserved-child uncertainty, name reuse, current
child error identity, six process-exit boundaries, actual online completion,
publication failure/retry, malformed-table/progress source preservation, and
whole-metadata backup/snapshot/schema migration preservation. These are
synthetic process tests, not power-loss tests.

[Compatibility audit](namespace-purge-compatibility.md): unfinished purge blocks
offline inventory, snapshot, backup and migration. Completed progress is
preserved in whole metadata. Published logical export `faf0f81` and the newer
lineage-changing restore explicitly reject the new tables, including completed
history, until Feynman's actual ancestry-aware capture/merge/export child.

Remaining work: Feynman's maintenance/raw-writer guard join and separate offline
compatibility child; Pascal's native writer participation; Halley's accepted
work tracker, transitive completion barrier and coherent read views; and the
separate [catalog batch-manifest/actual-consumer child](catalog-batches-next.md) using reserved identities.
No public whole-store CAS, complete A1, native serving or A6 barrier claim.

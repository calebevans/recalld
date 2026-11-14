# Canonical writer reconciliation audit

Audited the validated insertion/tombstone parent `1ae3d5a` and read-only integration `75882c3`; owner plans below were confirmed directly. Integration had not yet consumed insertion/tombstone at audit time. This is a staged participation inventory, not an ordinary-v2 admission claim.

| Actual path | Current boundary | Required reconciliation |
| --- | --- | --- |
| HTTP/MCP/daemon store, explicit parent/supersedes | Insertion receipt, record/effect tail, awaited completion; vector validation `7fa19f8` | A4 adds provider/profile evidence through the same final transaction. Its Bound check still rejects ordinary-v2 serving; it is not a native insertion writer. |
| HTTP/MCP/daemon forget | Tombstone receipt/ledger/outbox, retained structural edges, awaited completion | Native tombstone requires versioned-record proof/token participation; keyed batch manifests remain separate. |
| Sweep strength, Full→Summary, Summary→Ghost | This maintenance slice routes the real engine/sweep into the shared receipt/outbox tail | Merge active-namespace admission when Curie's durable purge state exists. Preserve final current-record/policy/graph guards and original clock. |
| Ghost hard delete, explicit graph writes, autolinks | Existing deterministic graph effects, receipt and removal ledger for deletion | Include accepted autolink/deletion children in transitive completion tracking; never silently include separate commits in an insertion receipt. |
| Legacy exposure/feedback | Integrated receipt-bearing observation transactions and awaited consumers; direct legacy helpers still exist | Native exposure/feedback is Pascal's restricted session over existing MetadataStore. It covers those two operations only, not native insert/forget/sweep. |
| Namespace create/policy | Integrated catalog receipt and staged filesystem preparation | Participate in all-writer publication accounting; policy changes affect projected eligibility. Native policy migration remains explicit and separately proven. |
| Namespace delete/purge | Legacy chunked deletion and best-effort cleanup | Curie owns durable begin/resume/progress, deletion admission, stable child IDs/receipts, same-tx chunk ledger/effects, final catalog deletion and awaited cleanup. No atomic namespace-batch claim. |
| Selected text compaction | Synced generation payload, one selection/reference/receipt transaction, engine-exclusive publication | Count its generation in a coherent view/barrier even when no entries are removed. Native participation remains gated. |
| Raw metadata insert/update_record/batch_update_records/decay_sweep/strip/edge-count and low-level vector/edge access | Some advance generation/ledger; not all register derived work or receipts. Legacy benchmark seeding still calls raw insertion. | Restrict runtime access or delegate actual callers to typed participating writers. Keep test/recovery loaders explicitly separate; do not advertise a global CAS while bypasses remain. |
| Migration/import/restore/export/backup | Exclusive offline ownership, validation and staged publication; restore forks lineage | Keep offline tools outside ordinary admission, preserve every known canonical table/receipt and fail closed on unknown required tables. Purge progress must join each registry/validator before activation. |

## Owner interfaces

Curie proposes `namespace_state::require_namespace_active(&WriteTransaction, NamespaceId) -> Result<NamespaceConfig, StorageError>`, resolving current config and rejecting durable Deleting state under the writer's final transaction. It is not implemented in this parent; do not add a fake always-active helper. Purge resume owns the narrowly permitted Deleting-state writes.

A4's generated input is `GeneratedEmbedding { values, profile: Option<EmbeddingProfile> }`, propagated through the actual insertion completion/final transaction. Exact Bound profile ID/dimension is checked before provider/index/staging; unknown legacy provenance remains unknown. The planned profile plumbing does not authorize native record creation.

Pascal's native exposure/feedback captures opaque intents from one canonical read transaction and checks record/policy tokens after replay under a narrow native guard. It reuses MetadataStore and owner locks. Generic native mutation remains rejected. Future native insertion, tombstone, phase loss and purge must participate in those schema/provenance/token/lineage proofs; no parallel storage engine is needed.

## Coherent views and barrier sequence

The current `effects_epoch` checks cover effects work, not every metadata/catalog/observation mutation. `derived_dirty == false` is not a full-store applied-generation watermark. Neither value can be relabeled `readGeneration` or a complete barrier.

1. Route remaining runtime mutators through typed final transactions or reject them. Each canonical commit must either register durable derived work or prove the affected views unchanged; generation is not advanced by derived acknowledgements.
2. Join one admission/publication boundary across insertion, observation, maintenance, graph/autolink, catalog, purge and compaction. Preserve graph→indexes→storage ordering and include direct metadata capabilities, not only transport wrappers.
3. Halley's tracking must register accepted work **before enqueue**, register child work before parent completion, close admission, then drain a transitive fixed point including `DerivedCompletion`. Only after that may a freeze lease expose a verified `GenerationRef`, zero pending work and per-view applied generations. This API is planned, not implemented here.
4. Bind canonical records, policy/profile, selected payloads, graph and requested derived views to one verified read view. An effects-only before/after stamp is insufficient. Release the read view before separately committing observation through its token-checked intent; do not invent an identical replayed query payload or use a second owned database session per query.
5. Admission tests must force writes from every family while a view/barrier is paused, include transitive autolink and sweep children, and inject canonical/derived failures. A returned ready generation must exclude neither accepted work nor maintenance commits. Native serving stays closed until all participating writers and consumers satisfy this join.

The next bounded A1 work is this legacy maintenance patch alongside Curie's independent purge slice. A4 profile plumbing and Pascal native observations are prerequisites for later native writers, not substitutes for them. There is no general content-update endpoint to implement.

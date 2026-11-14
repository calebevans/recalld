# Namespace entity postings and idempotent replacement

Independent branch from main `653c8d3`, retaining the tombstone-sweep and daemon fixes. This slice changes the derived entity index and its callers; it does not depend on the retrieval/semantics/FTS branches or introduce a canonical schema.

`EntityIndex` stores namespace-partitioned normalized-key postings and reverse normalized keys per `(NamespaceId, MemoryId)`. `add(ns, id, entities)` replaces the complete entry, repeated equivalent updates are no-ops, and empty/unindexable input removes old postings. `remove(ns, id)` uses reverse keys rather than caller-supplied labels; repeated or wrong-scope removal is harmless. `len()` counts distinct scoped entries with at least one normalized key. `remove_namespace(ns)` clears orphan postings even if canonical records have already disappeared. Storage remains responsible for immutable namespace/UUID assignment; the index never moves an entry to another scope implicitly.

Normalization and canonical-form overlap counts are unchanged. Duplicate labels and aliases contribute each canonical key once; shared-count ties use UUID bytes. Recall and autolink select namespace postings before their candidate caps. Canonical hard filtering from PR #17 and the entity-link branch must still be preserved during integration: partitioning does not certify that a posting is live or otherwise eligible.

Updated callers: HTTP single/batch insertion and deletion, MCP storage and shared single/batch/namespace cleanup, startup rebuild, benchmark ingestion, retrieval adapter, and autolink. HTTP's internal `SearchPipeline` trait now takes namespace on entity addition/removal, and removal no longer accepts labels. MCP publishes accepted `input.entities` independently of `autolink_enabled`; disabling graph linking no longer disables entity recall. No query/ranking formulas or transport fields change.

## Accepted labels and integration

Consulted `d8d0406` and its multiword/Unicode/possessive-name regressions. **Legacy tags are not the complete accepted entity list.** Live indexing uses request entities. Removal does not parse tags, so names missing from legacy tags still disappear from the live index. Startup can recover only labels actually retained on disk until structured-label persistence is integrated; this limitation is unchanged and explicitly documented at the rebuild call. No tag-based eligibility shortcut was added.

Coordinated with Feynman (canonical schema) and Huygens (structured-label consumers). Their planned `StructuredLabels { entities, topics, emotions, provenance }` retains exact UTF-8, order and duplicates, with `NativeV2` versus `LegacyRetainedOnly` provenance. After that schema is available, consumers should feed its complete entity list into `add`; this index separately normalizes derived keys. Legacy migration cannot recover names discarded before persistence. This branch does not claim durable structured-label fidelity or v2 readiness.

No entity-link base dependency is needed: the autolink diff only names the existing namespace parameter and passes it to `find_by_entities`. Preserve `d8d0406`'s canonical live/scope checks, accepted-name behavior, and bounded alias-weight calculation when combining branches. Its direct EntityIndex test setup must pass a namespace on every `add`; use the record's namespace for ordinary fixtures, or deliberately wrong scope where testing stale/corrupt postings. It must not recover candidate label sets from canonical tags.

PR #17 and its descendants also have test fixture additions to `EntityIndex::add`; pass each fixture record's namespace there. Their retrieval adapter keeps the eligible-ID filter before its cap and changes the underlying lookup to `find_by_entities(namespace_id, entities, exclude_id)`. No timing/diagnostic fields are added. Schema/publication integration retains ownership of coherent snapshots, rebuild/readiness barriers and generation watermarks.

## Validation

Tests run offline in `recalld-ci:1.94` with `RUSTUP_TOOLCHAIN=1.94.1`, `--network none --cpus 2 --memory 4g`, `CARGO_BUILD_JOBS=2`, and dedicated cargo/target scratch. No home, credentials, daemon sockets or personal data mounts. `.validation` stays untracked.

- `cargo test --locked --offline --all-targets --features bench --quiet`: **502 passed**, zero failed (6 added tests); both binary targets compiled with zero tests.
- Rustfmt checks passed for all changed Rust files. Clippy completed with repository warnings, without a warnings-as-errors clean claim.
- A disposable read-only Docker file overlay used `d8d0406`'s autolink implementation/tests with namespace arguments added at its index calls: **3 entity-scope tests passed**, including accepted multiword/Unicode/possessive names. Neither branch was merged or modified for this check. The foreign fixture was deliberately indexed under the requested namespace to keep testing canonical wrong-scope rejection.

Regression coverage includes full replacement and repeated updates, stale alias removal, empty replacement, wrong-scope removal and same-UUID isolation, deterministic ties, higher-overlap foreign candidates before the adapter cap, real MCP ingestion with autolink disabled and accepted UTF-8 labels, cleanup without reconstructing labels, namespace deletion of orphan postings while preserving another namespace, and actual startup reconstruction with per-record namespaces for retained legacy labels.

Remaining scope: structured-label persistence/consumer integration and restart fidelity for legacy-lost names, A1 coherent publication/readiness, complete retrieval coverage/explanations, transport-v2 prerequisites and A5/A6 performance/quality gates. This is a correctness slice with no performance promotion or full A2 completion claim.

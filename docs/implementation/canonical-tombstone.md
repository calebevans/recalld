# Canonical legacy forget participation

This slice wires existing HTTP, MCP and daemon forget paths to a canonical receipt/effects transaction, including the ordered legacy batch wrapper. It builds on the frozen insertion writer and PR45 ownership foundation. There is no general content-update transport in the existing API; store/supersedes belongs to insertion. Native schema-v2 serving and public whole-store CAS capability remain disabled.

## Commit and replay

`RedbStorageEngine::tombstone_memory_with_receipt(&mut self, &MutationContext, MemoryId) -> Result<WriteReceipt, StorageError>` uses the insertion writer's shared transaction tail, extended with private `RecordMutation` variants. Matching replay precedes CAS and current-record lookup; explicit lineage is checked first. Fresh missing IDs are not found. A retained tombstone yields `MemoryTombstoned { memoryId, status: already_tombstoned }`; a fresh transition yields `status: tombstoned`. The typed outcome is payload-free. Request identity includes scope, operation/contract, expected generation and UUID, never removed text, generated time or mutable policy.

The `meta.db` transaction strips summary/tags/text references, zeroes strength, updates tag/namespace indexes, writes its receipt, records terminal removal with `ALL` field-loss bits and the same receipt operation/generation, and stores vector-release/derived work. Already-tombstoned outcomes retain previous removal evidence and release no slot. A matching replay never commits again, including after hard deletion/restart. Canonical structural edges remain unchanged; the terminal activation prerequisite excludes Ghost/Tombstone from seeds, receiving activation and relaying it.

Separate vector/edge files are **not** atomically committed with metadata. Pending canonical work blocks ordinary access and replays before startup cleanup. An unapplied release must match the canonical tombstone identity/namespace/old slot and must never target a slot owned by another live record. Applied work is not released twice after slot reuse. Payload bytes can remain physically present in freed slots/unreachable text; no forensic-erasure claim is made.

## Actual transport completion

HTTP/MCP forget await the existing `DerivedCompletion` worker. Postcommit failures retain the receipt and return committed/not-ready through the shared `CommittedMutation` error (renamed from insertion-specific error without changing its wire shape). HTTP returns 503 with `INDEX_NOT_READY`, `commitState: committed`, `ready: false` and the receipt. MCP preserves this evidence as a partially-applied error; the daemon preserves that error category.

Legacy success booleans/counts remain compatible. Missing IDs and clean already-tombstoned legacy calls return false without a new operation. If a previous forget has pending derived work, retry completes it before returning false. The explicit receipt API still records an already-tombstoned outcome when requested.

Legacy batch children commit independently in input order, preserving duplicate/missing-ID flags. A failure stops further children and reports the completed flags/index plus committed/unknown evidence where available; the MCP tool returns `isError=true` for partially-applied outcomes instead of inventing a zero-deleted success aggregate. This is not an atomic batch, a durable keyed batch manifest, or the complete contract-v2 indexed outcome protocol. Legacy unkeyed retry does not guarantee replay of original flags.

## Validation and remaining scope

Offline Docker tests use `recalld-ci:1.94`, toolchain 1.94.1, two build jobs, two CPUs and 4 GiB RAM, with no network/home/credentials/socket mounts. The original four actual transport probes reproduced missing same-transaction receipts and false ready success; the implementation makes those probes pass. Focused tests also cover replay-before-CAS, lineage/key conflicts, missing/already outcomes, restart after hard deletion, ambiguous commit, unapplied canonical failure, five process interruption boundaries, applied-release slot reuse, rejecting a live release owner, retained structural edges, HTTP failure/retry and actual batch tool error admission. Validation: 654 library tests and 1 CLI integration test passed; 4 doc examples are ignored. Clippy completed successfully with existing warnings, and formatting passed. After the final conservative unavailable-error mapping, all 13 focused tombstone tests and formatting passed again. Logs: `/tmp/recalld-lifecycle-review/tombstone-final.log` and `/tmp/recalld-lifecycle-review/tombstone-focused-final.log`.

Low-level legacy metadata tombstone/batch helpers, namespace purge, other maintenance/content-strip writers, durable keyed batch manifests, public discovery/CAS/readiness/barrier admission and native schema-v2 consumers remain separate required slices. No complete-store generation or writer participation claim follows from this patch. Namespace purge retains its explicitly best-effort legacy index cleanup; forget no longer uses that path.

## Validated insertion correction join

Merged insertion vector validation `7fa19f8` without rewriting history. Its new caller uses the shared `map_mutation_error`; profile/provider validation and original request-bit hashing remain insertion-owned. Joined validation: 659 library tests and 1 CLI integration test passed, formatting passed, and Clippy completed with warnings. Log: `/tmp/recalld-lifecycle-review/tombstone-vector-join.log`.

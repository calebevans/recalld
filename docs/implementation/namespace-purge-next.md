# Next A1 writer slices — proposal, not implementation

Inspected corrected insertion source `7fa19f8`, catalog receipt/scope branch
`82e840b`, and Feynman's reported tombstone handoff `2cfe80d`. Pascal owns native
exposure/feedback; Halley owns the Observed consumer join (`3b590fb`) and A6
freeze planning. Their work is not declared complete by this inventory.

## Actual gap

`RedbStorageEngine::purge_namespace` currently commits chunked GraphChanged
receipts with same-transaction removal ledger and effects. It carries removed
records in memory for cleanup, then separately deletes namespace configuration
with a generation increment but no receipt. It has no durable parent operation,
Deleting state or cumulative progress. After interruption, a retry rescans
survivors; its reported counts do not reconstruct prior committed work. The
actual namespace-delete consumer is MCP/daemon `delete_namespace`; there is no
REST namespace-delete or general content-update route to invent.

`DerivedCompletion::complete` repairs durable effects under publication/graph/
index/storage locks. It is not a generation-pinned read view or a transitive
all-writer barrier. `effects_epoch` is admission evidence, not a GenerationRef.
`OfflineSnapshot` owns an offline store and must not be opened over a live owned
root or treated as certification for an unchecked directory copy.

## First implementation: resumable namespace purge

Minimal parent: corrected insertion plus Feynman's shared RecordMutation and
CommittedMutation child, then the catalog receipt/scope fix (`82e840b` or its
published descendant). These are real dependencies: extend the shared removal
transaction and ensure actual namespace creation/policy paths respect deletion
state and pending directory ownership. No broad integration parent. Pascal's
native stack need not be imported merely to avoid conflicts, but its eventual
join must consume the same namespace-state checks before activation.

Proposed internal API (names provisional):

```rust
begin_namespace_purge(ctx, force) -> PurgeProgress
resume_namespace_purge(store_id, namespace_id, operation_id, chunk_limit) -> PurgeProgress
lookup_namespace_purge(store_id, namespace_id, operation_id) -> PurgeProgress
```

`PurgeProgress` carries immutable start/child/final receipt references, counts,
state and pending work; never removed record payloads. The start receipt remains
historical and immutable. Current progress lives in a separate versioned row;
completion commits a distinct final receipt, not a rewritten start receipt.

* Begin: check lineage/replay before CAS; under the final transaction resolve the
  namespace, reject default namespace/schema-ineligible operations, and enforce
  force/emptiness using canonical rows including retained tombstone identities.
  Commit Deleting, immutable namespace ID, parent operation, initial cursor,
  reserved next-child ID and start receipt together. No destructive work before
  this commit. Force does not bypass ownership.
* Resume: walk bounded UUID-ordered chunks, including tombstones. Commit removed
  rows/indexes, ledger, edge effects, child receipt and cumulative progress in
  one transaction. The reserved child ID survives an uncertain acknowledgement;
  advance progress and reserve the next child ID only in that same commit.
  A private receipt helper must validate the reservation rather than accept an
  arbitrary caller-selected operation ID. Reconcile an unknown child before
  retrying. Original start CAS is not reapplied after the operation's own writes.
* Finalize: prove no canonical records remain, remove configuration and commit
  the final receipt/progress atomically. Retain receipts, hashes, terminal state
  and removal evidence after namespace deletion. Directory cleanup must never
  target a newly created namespace reusing the display name; keep the old name
  reserved until owned cleanup finishes. Startup validates and resumes durable
  purge/effect state before ordinary admission, without creating missing files.
* Actual consumer: await canonical and derived completion; return cumulative
  outcomes or receipt-carrying committed/not-ready errors. Reads/writes must not
  treat a Deleting namespace as ordinary live state. First implementation may
  use conservative store-wide admission closure during the bounded runner;
  selective availability is not an acceptance requirement. Recovery must drive
  the saved operation, not leave a permanent outage requiring an undocumented
  manual step. Keep graph -> indexes -> storage order; no storage-held graph wait.

Required proofs: multi-chunk interruption and replay, uncertain child/final
commits, concurrent insertion/policy/forget against Deleting, force-check race,
namespace-name reuse, tombstone/foreign incident-edge cleanup, strict malformed
progress preflight without source mutation, and actual consumer publication
failure/retry. All use synthetic temporary stores/offline Docker.

## Second implementation: actual batch consumers and catalog manifests

Separate child after the reserved-child receipt primitive is proven. Reserve
catalog-scoped manifests with ordered normalized request hashes, immutable
resolved scopes and stable child IDs. Actual insertion/forget batch consumers
commit each item and its child outcome together; invalid items become indexed
final failures, pending/unknown items require reconciliation, and replay never
reapplies committed items. Persist hashes/IDs/numeric defaults/outcomes, never
request text/vector copies. Keep original intent and expanded-default identity;
changed policy must not silently change a resumed item's request hash.

Preserve every input index and committed receipt through transport errors. No
single aggregate generation or atomic-batch claim. Exact CONTRACTS aggregate
statuses and readiness are part of this consumer slice, not assumed from the
legacy batch envelope. An unfinished manifest that needs client payload cannot
be executed by inventing it during startup or freeze.

## Coherent read views and barrier prerequisite

Before public whole-store CAS/v2 or an A6 frozen-state claim, reconcile every
writer and consumer: canonical insertion/tombstone, exposure/feedback, lifecycle
loss, explicit/automatic edges, catalog policy/create/delete, compaction,
maintenance/import paths and batch bookkeeping. Remaining direct paths include
MetadataStore insert/generic patches/batch updates/decay/strip/raw edge-count
methods, old metadata purge/config deletion, and legacy engine insertion used
by benchmarks/import-oriented code. Route, explicitly isolate, or gate them;
receipt variants alone do not establish participation.

Halley's agreed boundary is `barrier(GenerationRef, monotonic_deadline)` plus an
owned freeze lease: register accepted work before enqueue, register descendants
before parents complete, close action/writer admission for freeze, drain the
transitive closure to a fixed point, and verify real committed/derived generation
watermarks with zero pending work. Keep the lease through coherent capture of
records/payload selection, graph/indexes, receipts/manifests, ledger and policy.
A query read view likewise pins canonical state and compatible derived views for
its entire pipeline. No storage lock may be held while awaiting graph ownership.

A6 uses a 30-second monotonic deadline; unresolved work fails/invalidates the
episode. Pending manifests requiring resubmission must remain reported pending,
not be hidden to certify a snapshot. Reset stops workers, verifies a fresh
synthetic clone, restores clock/config/RNG/cache mode, rebuilds, barriers and
checks logical hashes before actions. This is a dependent all-writer join,
not a new public endpoint that can be advertised from the purge slice alone.

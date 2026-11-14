# Canonical legacy insertion participation

This slice starts at core PR45 `1ca36c4c1bf363f98e0003d1bb283b1ad08d4a9e`.
It implements a typed insertion writer and its actual legacy HTTP single/batch,
MCP store and daemon store consumers. It does not enable public keyed writes,
whole-store CAS, native schema-v2 insertion, or complete A1.

## Transaction and publication

`InsertMemory` carries the optional caller UUID, summary/text/tags, optional
initial stability and creation timestamp, optional supplied embedding, parent
and superseded identity. `insert_memory_with_receipt(ctx, input,
generated_embedding, sampled_at)` uses exactly the namespace in `ctx`.
`lookup_insert_replay(ctx, input)` can resolve an existing key before provider
work; absence never reserves a key and final insertion repeats verification.
Existing public legacy transports have no keyed parameter and use unkeyed
receipts. Their provider work is outside the storage guard.

The metadata publication guard and one redb write transaction cover replay,
current namespace decoding and policy checks, terminal-ID/duplicate checks,
canonical endpoint checks, new row/indexes, explicit edges, absolute peer counts,
receipt and durable effects. The engine's exclusive mutable ownership protects
the edge snapshot through this transaction and its materialization. No cached
graph phase grants write eligibility. Both endpoints must exist in the selected
namespace. The common effects tail rejects Supersedes cycles and branches;
new insertion cannot normally form a cycle because its UUID must be new.
Legacy retained parent/old-version identities remain valid structural targets,
including retained Ghost/Tombstone targets. The new child/replacement is Full.

Finite vectors of the namespace dimension (1–8192), finite positive stability,
representable timestamps and valid content are required before generating a
UUID or allocating payloads. Text/vector bytes are staged and synced before
metadata commits references. Final configuration is read inside the transaction,
so provider work cannot authorize a stale namespace, resolved default or edge
plan. All schema-bearing namespaces and control schema 2 remain rejected;
this slice makes no claim that same-dimensional vectors prove model identity.
Native profile validation and native writer participation remain dependencies.

A committed insertion includes its deterministic explicit edge intents in the
outbox. Canonical edge materialization precedes `canonical_applied`; derived
publication is acknowledged separately after graph/vector/FTS/entity/cache
completion. The startup pure pending-plan preflight validates new insertion
outcomes against canonical rows before writable recovery. Historical applied
plans are not reinterpreted as current records after subsequent deletion.
Removal-ledger hooks and selected-text disposable validation are preserved.

The online coordinator releases its shared publication lease before taking the
exclusive completion gate, then locks graph, indexes and storage in that order.
It never awaits the graph while holding storage. All affected cached records
are invalidated; unrelated cached temporal-link candidates remain intact.
Legacy automatic links remain subsequent operations and completion runs again
before success. This is not an atomic transaction for all automatic links.

## Request identity and expanded defaults

This implements the CONTRACTS receipt rule to hash normalized fields and
expanded defaults, rather than silently excluding mutable defaults:

* The original `Option` values remain in the hash: absent UUID means generate,
  absent creation time means sample the server clock, absent stability means
  resolve the namespace default, and absent embedding means provider generation.
  Explicit UUIDs, timestamp zero and float bits (including signed zero) remain
  distinct caller intent. An explicit value equal to an expanded default does
  not alias an absent value.
* Expanded stability and embedding dimension are also hashed. Fixed v1
  difficulty 5, phase Full and strength 1 are expanded explicitly. Ordered
  normalized tags and supplied embedding float bits participate. The shared
  hash includes operation kind, contract version, immutable scope and expected
  generation using the existing length-delimited SHA-256 encoding.
* Generated UUID/time and external provider output are excluded. On first
  commit, the outcome persists UUID, timestamp, initial-stability bits,
  dimension and explicit-edge count. These are numeric/ID outcomes, not copied
  summary/text/tags/embedding/provider output or secrets.
* Replay first reads the scoped key and historical numeric outcome, reconstructs
  the expanded hash with those original defaults, then verifies the complete
  hash. A changed current namespace policy therefore cannot alter the identity
  of a retry. Lineage precedes replay; matching replay precedes current CAS,
  namespace/policy lookup, current-clock checks and payload allocation. Keys
  remain shared across operation kinds. Historical replay after deletion returns
  the original ID/receipt and never reconstructs content.
* Receipt decoding rejects nil UUIDs, unrepresentable timestamps, non-finite or
  non-positive initial stability, dimensions outside 1–8192 and more than two
  explicit edges. Catalog-scoped insertion receipts and inconsistent outbox
  outcome/action counts are rejected. Historical numeric defaults must pass
  these checks before they are used to reconstruct identity.

## Failure boundaries

Prevalidation errors leave payloads, generation and receipts untouched. After
staging begins, errors close admission and preserve bytes until reopening has
validated every canonical vector owner. Unowned allocated slots can then be
reclaimed; an owned free slot, duplicate owner or missing payload remains
corruption. Text staging may leave unreferenced append bytes for later safe
compaction. No I/O return is permission to free a possibly committed slot.

Unknown metadata commit returns `CommitOutcomeUnknown` with operation ID and
requires reopen/receipt lookup. Known committed materialization/publication
failure returns `CommittedInsertion` with the complete payload-free receipt.
HTTP reports `INDEX_NOT_READY`, `commitState=committed`, `ready=false`, receipt
and 503; MCP/daemon preserve the receipt in a partially-applied error. No ordinary
successful memory response is returned on those failures. Retrying online
completion repairs a derived-only failure without another insertion/generation;
canonical/uncertain failures require reopen first.

## Remaining actual writer sequence

1. Dependent canonical tombstone/content participation: receipt and same-tx
   removal evidence, delayed release with terminal ownership checks, actual
   forget/batch callers, retained structural edges. Feynman owns this child.
2. Native profile validation plus schema-aware record writer, coordinated with
   A4/A3. Legacy schemaNone remains explicitly unverified, never auto-attested.
   No native-v2 writes are enabled by this insertion change.
3. Join exposure/feedback receipt consumers and original-intent/query admission
   work owned by Halley/Hume; this parent intentionally does not import those
   independent stacks. Join catalog receipts separately.
4. Audit and route every remaining writer: `MetadataStore::insert`, generic
   record patches/batch updates, lifecycle strip/decay paths, raw edge/count
   setters, single/batch tombstone, administrative purge and namespace mutation.
   The old low-level engine insertion remains used by synthetic fixtures,
   offline/import-oriented code and `bench/locomo.rs`; it is not certified as a
   participating public writer by this slice. Hard-delete/graph effects already
   have outboxes, but complete caller-keyed mutation semantics remain separate.
5. Durable keyed batch manifests, full per-item receipt/unknown/not-attempted
   outcomes, all-writer generation authority and pinned read views/barriers.
   Existing legacy batch success shape is retained; this slice does not claim
   the CONTRACTS v2 aggregate batch protocol or exactly-once automatic linking.

Validation records follow after the exact source is frozen. All tests use
synthetic temporary stores in offline Docker. Process exits and synthetic I/O
faults establish these boundaries only; they are not power-loss proof or a
performance promotion result.

## Validation and handoff — 2026-11-14

Implementation commit: `5eb0e23`. Validated source/merge:
`75fe92d670148ed21de16f3b4a61e2cb89dde70d`. Its second parent is updated core45
`fb7ae7e`, which includes ownership fix `2788645` and schema/snapshot ownership
`6c0aa74`. The ownership merge was clean; it does not add unrelated integration
ancestry. This evidence-only documentation update does not change tested source.

Validation used `recalld-ci:1.94`, `RUSTUP_TOOLCHAIN=1.94.1`,
`CARGO_BUILD_JOBS=2`, `--network none --cpus 2 --memory 4g`. Mounts were this
worktree and its separate scratch cargo/target directories only.

* `cargo fmt --all -- --check`: passed.
* `cargo test --offline --all-features`: **640 library tests + 1 CLI integration
  test passed**, zero failures; four pre-existing documentation examples ignored.
* `cargo clippy --offline --all-features --all-targets`: finished successfully
  with warnings (370 library warnings, 381 in library-test mode including
  duplicates). This is not a warning-clean claim.
* Focused `cargo test --offline --all-features insertion` before the ownership
  parent update: **14 passed**. Those tests also passed in the final full run.
* An additional `--test offline_eval` invocation reported that no such target
  exists on this minimal parent; its only integration target is
  `text_migration_cli`. No offline-evaluation result is claimed here.

The 14 insertion proofs cover replay before mutable policy/current-clock/CAS and
allocation, caller option/float identity, cross-operation key conflict,
concurrent identical keys, no-mutation validation, canonical explicit links,
stale missing targets and retained structural tombstones, schema gating,
receipt numeric validation, effect failure/reopen ownership, pure malformed
pending-plan preflight without changing the original DB, synthetic uncertain
successful commit and receipt reconciliation, and real process exits at payload
sync, before/after metadata commit, edge commit and canonical acknowledgement.
Actual HTTP storage-adapter and daemon dispatch tests induce a real derived
vector rebuild failure after canonical commit and verify committed/not-ready
receipt errors. The online coordinator test repairs all views with the same
receipt/generation and keeps unrelated cached candidates.

Logs remain uncommitted under `.validation/`: `final-tests.log`,
`final-clippy.log`, `final-fmt.log`, `insertion-final.log`, `offline-eval.log`.
Earlier exploratory logs include the corrected keyed-test fixture omission;
only the final source and results above are the handoff evidence. No personal
store/daemon, provider credentials, networked runtime tests, push or deployment
was used. Dependent tombstone work may generalize the internal
`CommittedInsertion` variant to `CommittedMutation` without changing this wire
shape or dropping receipts.

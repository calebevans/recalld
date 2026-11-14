# Mutation foundation (A1, CONTRACTS revision 1)

Implementation base: `653c8d32f526e78d3f51a39cf6dea2d50c07bcf2`.
Branch: `impl/mutation-foundation`; rebased onto `impl/storage`.
See the integration validation and bounded next slice below.

## Implemented boundary

This is an additive **metadata transaction foundation**, not the complete A1
canonical store. No MCP, REST, daemon, discovery, cursor or barrier interface
advertises the counter or keyed mutations. Do not expose them as whole-store
generation/CAS until the remaining writers and effects below participate.

`meta.db` gains three tables: `canonical_control`, `mutation_receipts`, and
`mutation_keys`. Control schema 1 persists a random stable `storeId` and a
decimal-string u63 `committedGeneration`. It is deliberately distinct from
canonical/record schema 2: this change preserves DiskRecord v1 and the existing
vector/text/edge layout. Clean legacy metadata is adopted at generation zero;
existing records are not reencoded. Unknown/malformed control, missing state,
or a partially missing control/receipt table set fails closed. Generation and
namespace-ID exhaustion fail rather than wrap.

All existing metadata mutation transactions advance the counter in the same
redb transaction. This includes insert, record patches, decay sweep, text-pointer
updates, tombstone, hard delete, batch record updates/tombstones, namespace
creation/deletion, and purge chunks. Reads, initialization and derived bitmap
checkpoints do not advance it. A multi-transaction operation advances it per
transaction, not once per logical operation; it has no aggregate atomicity.

Access recording is the first receipt consumer. Existing
`StorageEngine::update_access` delegates to the receipt implementation, retaining
its legacy return type. `RedbStorageEngine::update_access_with_receipt` exposes
the internal integration entry point. Record/history changes, generation,
receipt and key binding commit in **one metadata transaction**. A tombstone or
missing record cannot receive a new access event. This preserves legacy access
timestamp semantics, not the proposed lifecycle v2 planner.

The receipt/context foundation enforces:

- Explicit lineage for keyed or expected-generation requests; foreign lineage
  fails before replay. Unkeyed requests remain distinct events.
- Immutable namespace or distinguished catalog scope; keys have no operation
  kind or transport in their scope. Operation kind is part of the request hash.
- Matching replay before the metadata-wide expected-generation check. Another
  namespace's metadata mutation also invalidates that check.
- SHA-256 requestHashVersion 1 over ordered, length-delimited fields: each has
  an unsigned 64-bit big-endian byte length. The common prefix is operation kind
  (UTF-8), contract version (u32 BE), scope (catalog byte 0, or byte 1 + u32 BE
  namespace ID), expected generation (canonical UTF-8 decimal or empty).
  Access appends target UUID bytes and the serialized AccessKind string. A4
  must normalize defaults and object fields before adding further consumers;
  supplied arrays/embedding bytes must retain their semantic order/identity.
- Access's timestamp is a **server clock sample**, excluded from request
  identity. Replay returns the originally committed timestamp/ID/generation;
  it never applies a newer timestamp. Client-supplied time needs a distinct hash
  contract. Generated IDs and external provider output likewise stay outside
  request identity.
- Lifetime receipt retention after record and namespace purge. Lookup uses
  store ID, immutable scope and operation ID. Recreating a namespace name never
  rebinds old receipts. Stored outcomes contain only IDs/timestamps, not content.
- A receipt proves canonical metadata commit only. No `ready=true` is inferred;
  there are no derived watermarks or pending-work counts yet. A redb commit error
  returns `CommitOutcomeUnknown` with operation ID for reconciliation after
  reopen, not an assertion that the access event rolled back.

## Required dependent slices

1. **Complete the serialized canonical boundary and durable effects/outbox.**
   Commit deterministic edge puts/deletes, absolute peer counts and index
   invalidations with metadata/generation/receipt. Route all edge writers through
   it; replay pending effects before reads and startup orphan cleanup. Keep the
   storage parent's payload durability/ownership/compaction gates. Neither the
   current edge DB nor vector/text files share the metadata transaction. Merely
   incrementing the counter before/after these writes is insufficient.
2. **A3 lifecycle consumers and schema migration.** Persist the planner's
   record/policy checks, one clock sample, feedback/exposure/phase changes and
   typed numerical outcomes in that boundary. Implement canonical/record schema
   v2 migration with provenance and explicit policy choice. Current reinforcement
   still consists of multiple legacy calls; an access receipt is not its receipt.
3. **Remaining receipts, terminal identity and removal ledger.** Add inserts,
   feedback, deletions, graph and catalog operations, payload-free terminal IDs
   and removed-field masks. Implement restore ancestry/ledger reconciliation and
   new restore lineage; ordinary open preserves lineage, but existing restore
   does not yet implement the required fork. Historical receipt lookup across
   restored lineages is not implemented. No deletion-safe restore claim.
4. **Batches and readiness.** Durable catalog manifests and stable child IDs,
   per-item commits and finalized failures, unknown-outcome reconciliation;
   synchronous derived publication, generation-consistent reads, watermarks and
   barrier/transitive-work accounting. Return committed/unavailable plus receipt
   when publication fails. A successful metadata receipt alone is insufficient.
5. **A4 transport consumers.** Thread normalized immutable scope, context and
   receipts through MCP/HTTP/daemon, propagate commit state and actual readiness,
   and implement default-true synchronous exposure and explicit false-mode
   no-write behavior. Only then enable public whole-store CAS/cursors/discovery.

Old binaries may ignore additive tables and mutate metadata without advancing
this counter. Downgrade/mixed-writer use is therefore not certified. Do not
delete these tables or reset the generation to make an incompatible store open.
The storage parent's fail-closed file inventory and migration work remain
prerequisites; this change does not replace them or change the backend.

## Validation

Tests use synthetic temporary stores inside `recalld-ci:1.94`, toolchain
`1.94.1`, `--network none --cpus 2 --memory 4g`, and `CARGO_BUILD_JOBS=2`.
Only the worktree, a read-only dependency registry, and an isolated target
directory are mounted; no home, credentials, personal database or host sockets.
Cargo uses `--offline --locked`.

Ten new tests cover canonical decimal encoding, hash boundaries/order, invalid
contexts, transaction abandonment before commit and generation exhaustion,
replay-before-CAS/lineage/payload conflicts, concurrent retries and namespace
key scopes, legacy generation advancement, legacy adoption/version rejection,
missing control tables/state, and engine restart/purge/recreated-name replay.
Abandonment and lost-response tests exercise actual redb transactions and engine
reopens; they are not power-loss or arbitrary I/O-fault injection tests.

Executed on 2026-11-14 against this worktree before stacking/rebase:

| Check | Result |
| --- | --- |
| `cargo test --offline --locked --lib` | 506 passed |
| `cargo test --offline --locked --all-features` | 506 passed; 4 doc tests ignored |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --offline --locked --all-targets --all-features` | Passed with warnings |

An additional `-D warnings` clippy attempt failed on the repository's lint
backlog. New control methods also inherit `result_large_err` warnings from the
existing StorageError representation; this is not a warning-free lint result.
Re-run validation after stacking on the storage parent, especially where its
metadata publication gate intersects the new access transaction and generation
increments. Build outputs and logs are outside the worktree and are not committed.

## Storage integration

The storage parent's mutation/publication gate remains authoritative. The
fallible single-record helper advances generation and publishes phase under
that gate. Sweep/batch replacement uses the same guarded helper and advances
exactly once per nonempty transaction; checkpoint/rebuild writes still do not
advance generation. Access receipt preparation now requires a held publication
write guard, retained through commit or replay. This prevents a concurrent
sweep snapshot from overwriting an acknowledged access event.

Metadata startup and staged backup/restore share read-only validation of the
additive control schema: partial table sets, multimap/type conflicts, extra or
missing control state, unsupported versions, malformed receipt identity or
future generations, dangling retry keys and cross-scope receipt pointers fail
closed. An adopted store also cannot recreate missing canonical record,
namespace or namespace-counter tables. Clean legacy stores remain adoptable;
validation itself does not initialize control or mint lineage. Removing *all*
control tables is indistinguishable from legacy state in this additive format;
a future format gate must make deliberate downgrade detectable.

The integration regressions cover the paused-sweep/access schedule, all six
partial table-presence combinations, missing canonical tables, table/type and
receipt-integrity conflicts, and checksum-valid bad-control archives preserving
the original restore target. Source bytes remain unchanged during strict
backup validation. General whole-store CAS/readiness is still unsupported.

## Next dependent slice: canonical graph effects

Deliver a metadata-plus-edge effects boundary without exposing new public
mutation, generation, CAS or readiness APIs. It depends on this foundation and
must preserve the lifecycle owner's final-write expected-state checks.
Generation-selected text compaction is a separate parallel slice: its owner
controls text descriptors, atomic offset/selection, `text.rs`, engine text-open
and compaction wiring, and selected-file backup inventory. This effects slice
must not modify those methods or overload the text descriptor transaction.

Concrete scope and order:

1. Add a versioned durable effects table in metadata and one mutation/read
   admission rule. Under the existing publication lock order, commit the record
   change (when any), absolute surviving peer counts, deterministic edge
   puts/deletes, operation receipt and derived invalidations together. Allocate
   operation identity before commit; on uncertain commit, make admission
   unavailable until reopen/receipt reconciliation. Never infer rollback from
   a returned I/O error or reclaim referenced payloads on that basis.
2. Implement idempotent effect application to `edges.db`, sync it, and record
   completion for the same operation/generation. Replay pending work before
   startup orphan cleanup or any ordinary reads/writes. Replay failure must
   keep the store unavailable with its committed operation identity. Expand
   strict startup/backup validation to require and validate the effects schema;
   backup refuses snapshots with pending canonical work.
3. Route every edge writer through that path: engine add/batch/remove,
   `remove_edges_and_repair_peers`, explicit links, autolinks, hard-delete and
   namespace-purge chunks, and lifecycle bridge application. Existing graph-
   first paths in `graph/explicit_links.rs`, `graph/autolink.rs` and
   `decay/sweep.rs` must become commit-first. Bridge planning/lifecycle policy
   remains A3-owned; storage applies a validated plan and expected state under
   the final guard. Preserve same-namespace edge validation and cross-scope
   incident-edge cleanup; prevent low-level mutable-store access from bypassing
   the admission rule.
4. Publish affected graph/cache/FTS/entity changes or mark their channels
   unavailable before reopening admission. Coordinate query admission with A2
   so graph readers cannot pass a readiness check and then consume a partially
   published view. Do not call a metadata-only receipt `ready`, and do not use
   stale-index empty results as success. Keep transport response shapes intact
   until all participating writers and query views support the public contract.

Acceptance uses isolated real redb stores: interrupt before metadata commit,
after metadata commit/before edge application, midway through edge application,
after edge sync/before completion, and before/after derived publication. Reopen
and replay twice; prove no lost/duplicated edges, absolute peer counts, receipt
replay without extra generation, no orphan cleanup ahead of replay, and either
complete visible state or explicit unavailability. Concurrent explicit/autolink
writes, hard deletion and purge chunks must use the same gate. Inject commit
and publication errors; do not substitute process exit tests for fsync/power-
loss evidence. Retained-ledger restore and lineage forking remain a separate
required boundary; this slice must not claim deletion-safe restore.

The deliverable is an internal recovery/read-admission guarantee for these
operations. Complete record/payload transactions, canonical vector ownership,
removal ledger, full lifecycle receipts, durable batch manifests and public
whole-store CAS remain explicitly gated follow-ups.

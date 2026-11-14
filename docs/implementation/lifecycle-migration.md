# Offline namespace lifecycle migration

Parent: `ad6408929b98831e40fc39ac7d13ff288b47d4cf` (schema/restore bridge,
PR48; includes the schema, selected-text fence, ledger and canonical-effects
prerequisites). The branch also merges the minimal schema/ownership prerequisite
`6c0aa74b9164bcb624b26f9c4b42af874ecf8347` (ownership repair `2788645` plus
the `OfflineSnapshot` guard type), without importing integration history. This implements the explicit policy switch in A3 §8. It does
not enable ordinary schema-v2 serving, exposure, feedback, or sweep writers.
Defaults remain `legacy_access_v1`.

## Operation and cutover

`storage::lifecycle_migration::{preview_lifecycle,migrate_lifecycle}` operate on
the existing canonical layout. The source must already have schema 2 and the
selected-text old-reader rejection fence. Legacy stores first require explicit
offline text/schema migration into a fresh destination. No in-place activation.

The source and destination are exclusively owned during staging/publication.
The source is never modified. All canonical files, receipts, ledger entries,
and immutable schema publication evidence are copied. One metadata transaction
changes the namespace policy version/revision, every namespace record's policy
version/revision and record token (including terminal records), and the receipt.
It does not change anchors, stability, any timestamps, retained phases, payload
references, structured labels, history, materialized strengths or indexes.
Rollback to v1 uses precisely the same operation and preservation rules.

Publication renames the validated, synced private staging directory into a fresh
destination. Cutover is an operator responsibility: keep the old source offline
and designate only one active copy. The copies intentionally retain the same
store ID. This is not a live migration, distributed fencing protocol, or a claim
that independently copied stores share a global idempotency-key registry.

A lost response is reconciled against the **exact destination** and original
request. Its receipt is returned with `replayed: true`, before the expected-
generation check. The request hash binds the normalized destination path.
A different request under that key conflicts; unrelated existing destinations
are rejected. Fresh independent copies can each commit independently. Do not
retry into another destination as a substitute for inspecting the published one.
Prepublication failures abandon the private copy; postpublication failures need
exact-destination reconciliation. These receipts do not certify derived serving readiness.

## CLI

Write a JSON request (all counters are canonical decimal strings):

```json
{
  "expectedSource": {"storeId": "<source UUID>", "generation": "42"},
  "namespaceId": 1,
  "expectedPolicyRevision": "0",
  "target": "explicit_feedback_v2",
  "idempotencyKey": "namespace-lifecycle-cutover-1"
}
```

Use `legacy_access_v1` for explicit rollback. Source ID/generation and namespace
revision come from canonical inspection, not guessed counters.

```sh
recalld --config offline.toml lifecycle --source /offline/source \
  --request request.json --preview-at 1794614400000
recalld --config offline.toml lifecycle --source /offline/source \
  --request request.json --destination /offline/fresh-destination
```

Preview and destination are mutually exclusive. JSON results go to stdout;
errors go to stderr. These commands never start a daemon, provider, or serving
engine. Preview samples its injected clock once, clamps each record against its
persisted lifecycle-update time, resolves the namespace policy, and uses the
shared pure projection. Graph donors are canonical live records in that
namespace, projected at the same clock sample, without query tag filters.
The graph resolver is the minimal shared helper from Hume's `b31dab8`; the
retrieval branch is not a parent. Preview reports immediate eligible transitions
and missing-summary skips without stripping content or materializing strengths.
Multiplier zero, disabled sweep config, permastore and terminal identities skip
transitions. Preview is diagnostic, not a promise that the future store remains
at those strengths or that derived serving state is ready.

## Evidence and compatibility

`LifecyclePolicyMigrated.transition` binds namespace ID, old/new versions,
old/new policy revisions, previous active-origin generation, affected count,
and a deterministic SHA-256 over sorted UUIDs and postcommit record tokens.
Normal request-hash, receipt/key and scope invariants apply.

Canonical inspection retains the original schema `source`,
`publicationGeneration`, migration ID and optional attestation receipt unchanged.
Existing schema-v2 format-only copies also retain the original marker and all
canonical bytes unchanged; a supplied new migration timestamp does not replace
original evidence. Schema-v2 re-attestation is rejected until a dedicated typed
operation/proof exists. Initial v1-to-v2 explicit attestation remains supported.

The active-origin suffix starts at the original publication for an unforked
store, or at generation 1 for a restore fork. Every subsequent generation needs
exactly one recognized lifecycle-migration receipt. Gaps, duplicates and unknown
operations fail closed. Every active receipt must explain a final namespace
transition; policy revisions must follow receipt order within each origin.
Historical receipts are checked against verified ancestry
bounds, never compared numerically with another store ID's generation.

Namespace policy history starts at legacy revision zero. The final namespace
version/revision and every record's policy fields must match that history. For
active suffix operations, count and UUID/token digest are checked against records,
rewinding only the known subsequent policy-migration token increments. Historical
record-set digests are not compared to the fork's surviving record set: restore
can legitimately apply deletion masks or remove records. Historical transitions
still need valid scope, keys, ancestry and an unbroken policy-revision history.
Ambiguous histories for the same policy revision are rejected, not guessed.

Revision-zero legacy records still require the original access/anchor equality.
A rolled-back record may retain distinct native retrieval and anchor times;
revision > 0 alone is insufficient without canonical transition evidence.
The shared feedback planner now reads an explicit canonical decay anchor when
present, clamping against `lifecycleUpdatedAt`, rather than treating retrieval
time as feedback. This is pure planning preparation, not an enabled native writer.

## Next writer boundary

Native exposure must atomically persist retrieval history/time, lifecycle-update
time and record token with its receipt; v2 exposure must not change the anchor or
stability. Native feedback must use the explicit anchor and effective clock,
preserve observed time/retained phase, and commit feedback fields/history/token
and receipt together. Both require expected namespace policy and record tokens,
terminal/removal guards and participation in canonical effects/publication.
Their recognized receipt variants and proof validation must extend this bounded
suffix validator before enabling them. Its token-rewind rule intentionally
admits only policy migrations today. No sweep/transport/default-v2 rollout is
included. Manifest/import fixes being integrated by Main/Feynman remain separate
from this branch's single actual parent.

## Changed files and ownership handoff

- `src/storage/lifecycle_migration.rs`: offline preview, staging transaction,
  typed transition evidence and strict namespace/receipt proof validation.
- `src/storage/lifecycle_migration/tests.rs`: real synthetic metadata, backup,
  restore, interruption and malformed-proof regressions.
- `src/storage/control.rs`: the typed policy-migration receipt outcome.
- `src/storage/canonical_schema.rs`: preserve original publication checks and
  delegate subsequent-operation proof validation.
- `src/storage/schema_migration.rs`: preserve immutable evidence on v2 copies;
  reject unsupported schema-v2 re-attestation.
- `src/storage/mod.rs`: expose the explicit offline migration API.
- `src/model/schema.rs`: revision-aware anchor compatibility; canonical history
  validation is still mandatory for migrated policies.
- `src/decay/lifecycle.rs`: explicit-anchor feedback planning, preserving the
  schema-absent v1 path.
- `src/graph/activation.rs`: Hume's shared canonical-strength donor resolver.
- `src/main.rs`: the explicit offline lifecycle CLI.
- `tests/lifecycle_cli.rs`: real CLI preview, cutover, retry and rollback.
- `docs/implementation/lifecycle-migration.md`: contract, boundaries and handoff.

Feynman agreed the codec, immutable publication and copy guard. Hume was notified
that canonical v2 fixtures now need actual migration receipts rather than a raw
policy-field rewrite. Huygens confirmed export/import CLI work uses
`src/bin/recalld_schema.rs`, separate from this CLI dispatch. No MCP/HTTP/search
request plumbing or ordinary-serving activation is changed here.

## Validation

On the initial `ad64089` parent: 11 focused lifecycle tests, then the complete
646-library-test suite and both real CLI integration tests passed; four existing
doctests were ignored. `cargo fmt --check` passed. `cargo clippy --all-targets`
exited successfully with warnings, including the shared large `StorageError`
variants (also reported at new call sites); this is not a warning-free build.
All execution used `recalld-ci:1.94`, `RUSTUP_TOOLCHAIN=1.94.1`, offline Cargo,
Docker `--network none --cpus 2 --memory 4g`, and a separate cargo/target cache.
Tests used synthetic temporary data without home, credential or socket mounts.

After merging `6c0aa74`: the complete **653 library tests + 2 real CLI integration
tests passed**, including all 11 lifecycle tests and the ownership/fork
regressions. Four existing doctests remained ignored. Formatting passed and
Clippy exited 0 with the same warning categories. Validation logs are retained
locally in `.test-scratch/`; none of its caches, outputs or logs are committed.

## Next native-writer slice (plan only)

Use an explicit ownership-checked entry through the existing metadata/publication
path. Permit the two validated canonical lifecycle operations while retaining
rejection of unparticipating writers and the ordinary-serving gate. Do not create
a parallel storage subsystem or implicitly upgrade namespace policy.

Halley's guarded caller should retain the original per-namespace exposure intent
(UUIDs, access kinds/roles, expected policy revision and record tokens), rather
than hashing a newly filtered retry list. Validate scope, visibility and terminal
state in the final transaction. V2 exposure changes retrieval history/time but
not anchor or stability; v1 also changes the anchor. Feedback uses the explicit
anchor and matching namespace policy, preserving observed time and retained
phase. One sampled clock, clamped against persisted lifecycle-update time, drives
each operation; commit fields, history, record tokens and receipt together.

Retry identity excludes generated timestamps. Lineage validation precedes replay,
which precedes fresh CAS checks. Return original numeric outcomes and receipts;
re-read content under current visibility, never retain deleted payloads in replay
state. Keep pre-exposure read generation distinct from per-namespace committed
receipts and skipped/conflicted outcomes. A postcommit publication failure must
carry its committed receipt, not become a generic retryable failure.

Add only typed outcomes for actual exposure/feedback writers. Extend the
contiguous proof validator with their explicit policy/token transitions, replacing
the current policy-only token-rewind assumption. Unknown operations, gaps,
duplicates, incorrect ordering and unsupported historical origins continue to
fail. Tests must cover commit/reopen/replay, original intent under visibility
changes, backward clocks, retrieval-before-feedback anchor invariance, stale
policy/tokens, terminal races and abandoned real transactions. Exact consumer API
names require coordination with Halley's current guarded-caller implementation;
this handoff introduces no native writer or transport activation.

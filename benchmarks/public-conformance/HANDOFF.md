# Public harness handoff and next controls

The executed baseline is commit `c9b4d42`, runtime `03d9b4d`, with immutable
[report](REPORT.md), [final evidence](evidence.tgz) and
[failed attempts](development-attempts.tgz). Later read-only documentation/test
alignment in this branch does not retroactively change that measured runtime.
Current source-selection support is a follow-up; it has separate boundary-test
evidence and has not yet run the combined production candidate.

## Exact baseline limits

- Nine episodes: three fresh stores per stdio MCP, HTTP MCP and REST, each with
  one ordinary process restart. Four synthetic sources, d4, k20, seed7 label,
  36 accepted source records total. No statistical performance study.
- MCP `2025-06-18` initialization/discovery, real legacy HTTP session, real
  production handlers/provider request path. Stdio uses existing direct fallback;
  remote daemon lifecycle/client interoperability is not covered.
- Seven malformed requests plus one deliberate offline embedding miss per
  episode; separately probe string quality. This is not the entire validation,
  namespace/profile, supersession, batch or retry matrix.
- Explicit false reads only. Omitted namespace tests the literal `default`, not
  custom connection/server-default migration. No claim about true-mode exposure
  parity. Public fields and durable files are compared, with `fts.db-shm` raw
  hashes retained but excluded from durable equality. No internal cache/history,
  receipt/ledger, transitive pending-work or concurrent-writer proof.
- Actual wall clock. Multiplier0; sweeps/autolinking/RIF/warm-file persistence off.
  Grade4 leaves stability unchanged; grade1 weakens without deleting. Tombstone
  forget removes summary/full text. No hard-delete/effects-completion, compaction,
  restore, crash, power-loss or chronological phase-transition test.
- Scoped assertions pass but full conformance is false: wrong-type quality
  accepted with durable changes in6/6 MCP cases, rejected in3/3 REST cases.
  Synthetic evidence retention and source Recall@20 are separate, both1.0 on
  one annotated evidence unit; no extraction/semantic answer quality claim.
- 518 all-feature library tests, fmt, Clippy and debug build passed in network-none
  Rust1.94.1 Docker,2CPUs/4GiB/two build jobs. Prior rustdoc exit101 is retained;
  doctests are not claimed passing. No host home, credentials or sockets mounted.

## Next bounded combined-runtime replay

1. Main supplies the stable combined commit after effects/compaction integration
   and confirms included validation/vector changes. Its reported 646 library
   passes are upstream context, not a result produced by this harness.
2. Freeze `--source-root` plus `--source-commit`. Review Cargo target inventory and
   list required tracked non-Rust assets with repeatable `--source-input`. The
   separate harness overlay remains pinned; reject dirty/moved runtime inputs,
   missing targets, symlinks and unsupported workspace/path dependencies.
3. Run the same nine-episode public fixture first in fresh isolated scratch.
   Preserve all old archives. Expect the PR16 quality fix to change all nine
   wrong-type-quality probes to rejection/no durable change; inspect the summary
   explicitly, since process success means characterization completed, not that
   `conformancePassed` is true. Retain failures and do not retry unkeyed writes.
4. On that pinned combined parent, extend the fixture to successful HTTP
   vector-only/similarity false-mode calls, including unchanged-state checks.
   Keep this as a declared fixture revision; do not relabel baseline EmptyQuery
   limitations as successful coverage. No expensive performance matrix is needed.
5. Draft33 online effects completion remains a separate gate. Public tombstone
   forget is not evidence for hard-delete crash recovery or complete effects
   publication. Likewise CLI34 fresh-destination rules are not exercised by the
   present process restart. Restore tests must use a newly allocated parent and
   a never-created destination, with no overlap or symlink to any source store.

No personal store or existing destination is a test target. Publication belongs
to main/user. Future FTS measurements must explicitly select the real v2 internal
entry point; this public-v1 suite does not measure that algorithm.

## Follow-on manifest and trace contract

Keep the diagnostic track/schema distinguishable from the full A6 contract.
Before adding controls, validate and freeze these fields, with unavailable
capabilities explicit rather than invented version numbers:

| Artifact | Frozen additions / checks |
| --- | --- |
| Run manifest | Runtime and harness commits, scoped dirty/source hashes and per-file origin, Cargo target/lock/build hashes, container ID, architecture/resource limits, complete independent CONTRACTS version vector and actual discovery capabilities |
| Fixture/environment | Dataset revision/license and content hashes; embedding full-input/profile/normalization hashes; source ordinals/mappings, schedule and scorer hashes; namespace policies, clock seed/initial time, graph/RIF/candidate settings and budgets |
| Control state | Snapshot manifest/logical hashes, lineage, generation refs, readiness/derived watermarks and pending counts; declare actual-clock diagnostics separately from manual-clock replay |
| Append-only trace | Run/episode/group/step and environment hash; request attempt before send; exact response/error/partial/unknown outcome; monotonic latency and actual/simulated times; UUID/source mappings, ranks/truncation and completion denominator |
| Mutation/read linkage | Actual receiptVersion/storeId/operation ID, decimal-string generations, separate read and committed generations, replayed flag, barrier target/result and snapshot before/after hashes; absent APIs stay unsupported |
| Evaluator sidecars | Labels/rewards and canonical inspection are inaccessible to actor requests; unique-source retention/retrieval scores stay separate, unknown evidence unscored; no silent ID/time normalization |

Validate counters as canonical decimal strings within CONTRACTS bounds, never
compare generations across store IDs, and preserve an explicit UUID bijection
including references/receipts. A source-input seed does not make clocks or UUIDs
deterministic. Retain failed attempts and count unresolved outcomes; no valid
reward/completion sample follows an ambiguous write or timed-out barrier.

## Frozen-state controls: consume owner interfaces

These are future requirements, not implemented endpoints. A1 owns canonical
state, generations, receipts, publication and snapshots; A3 owns the shared
clock and lifecycle planner. A6 may add an evaluator-only control channel inside
the isolated process, but must not implement alternate ingestion/mutation,
clock or receipt semantics. Actor actions still use serialized public tools.

1. **Barrier:** consume A1 `barrier(GenerationRef, deadline)` with a30-second
   monotonic deadline. Reject foreign lineage; wait for accepted and transitive
   work. Capture committed/applied generations, ready flags and pending count.
   Unavailable/timeout invalidates the episode, not a quiet-window substitute.
2. **Snapshot/inspect:** stop admission, drain, freeze shared clock; require zero
   pending work and generation-consistent views. Hash canonical records/text/
   vectors/edges, namespaces/policies, receipts/batch manifests/removal ledger,
   clock/RNG and behavior-affecting caches in stable logical order. Exclude only
   nonbehavioral telemetry/physical layout. Do not serialize executor queues.
3. **Reset:** stop prior workers and clone the synthetic snapshot into a fresh,
   inaccessible root. Restore exact clock/config/RNG and canonical identity;
   rebuild pinned indexes, barrier, then verify logical hash and deterministic
   ranks before actor admission. Fixture lineage may be preserved only inside
   this isolated replay domain. Production restore requires its separate lineage
   and deletion-coverage rules; never overlay a running store or copy an old
   archive over newer deletion state.
4. **Clock/sweep:** inject A3's one shared manual clock at construction, initially
   `2026-01-01T00:00:00Z`. Reject negative/overflow advance; advancing time does
   not implicitly sweep. Explicit sweep invokes the production planner/commit
   path then barrier; pin fixed-state and chronological policy arms separately.
5. **Acceptance:** three fresh reset/replays including restart yield matching
   declared logical hashes/ranks. False queries leave canonical/history/receipt/
   graph/lifecycle/ledger state and pending work unchanged after drain. Inject
   delayed transitive work and foreign lineage to prove barrier rejection/waiting.
   Only after A1/A4 receipts exist test same-key retry across restart; never
   automatically retry current unkeyed reinforcement. Deletion must stay absent
   through graph retrieval, rebuild and restored fixture boundaries as specified.

Until those interfaces and acceptance checks exist, keep publishing limited v1
public evidence. No full A6 completion, v2 readiness, CI/release performance gate,
paid-model evaluation or RL readiness follows from this foundation.

## Source-selection validation (follow-up only)

Seven Python boundary tests passed in isolated network-none Docker using image
`sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`,
2 CPUs/4 GiB, only copied scripts/logs mounted. These cover explicit/automatic
Cargo targets, sibling modules/assets and executable modes, separate harness overlay, unrelated
tracked/untracked exclusion, required includes, missing inputs, symlinks, frozen
commit drift and unsupported workspace/path dependencies.

A read-only frozen snapshot of aligned branch
`669a28b8d3b3ad4461c9c8cd32eb2f1d639ad360` selected126 files and explicitly
excluded five legacy data files. No new combined runtime has been executed.
[Validation log, command, source hashes and snapshot inventory](source-selection-validation.tgz),
SHA-256 `bff04242200361b91a9da45309d64c82ec18e73cc953b1dd28fd2a66d421060c`.
The old final/failed-conformance archives remain byte-for-byte unchanged.

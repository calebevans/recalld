# Final integration review

2026-11-13 · Integration revision 1 · Research-only specification review

**Design reconciliation is complete.** [CONTRACTS.md](CONTRACTS.md) and all six sibling specs now share the decisions below. This review changes documentation only; it does not certify implementation or measured release readiness. No experiments, personal recalld MCP access or delegation occurred in this review.

## Closed issues

| Overlap | Settled decision |
| --- | --- |
| Generation encoding | Lineage is mandatory for comparison: `{storeId, generation: "42"}`. Receipt `committedGeneration` and request `expectedGeneration` are decimal strings, range 0–9223372036854775807; expectedGeneration is store-wide CAS. No numeric JSON generation examples remain. |
| Retry scope and retention | `(storeId, namespaceId, key)` across operation kinds/transports; kind is in the request hash. Creation/batch manifests use catalog scope. Lifetime receipts survive namespace purge; no seven-day expiry or silent reuse. Keyed requests carry storeId; stale restore lineage conflicts. |
| Batch completion | Ordered per-item commits plus durable keyed manifest/child IDs; no batch atomicity. Preserve committed and finalized failed outcomes; pending/notAttempted/unknown are unresolved until reconciled. Counts include unresolved; committed-but-not-ready is never reported rolled back. |
| Restore/deletion lineage | Retained-state restore merges available compatible removal ledger before serving. Old-backup-only historical restore is isolated/read-only with unknown later deletion coverage; separate promotion acknowledges data loss/reappearance risk. No promise to infer deletions after the only surviving snapshot. Production restore changes lineage; lossless migration preserves it. |
| Observation and replay | Search/direct/batch fetch default true in both API versions; false means no canonical/deferred lifecycle writes. True commits one exposure per returned UUID before success. Read and exposure generations are separate. Replayed exposure cannot return archived deleted text or apply another event. |
| API, lifecycle and RIF versions | Omitted API version remains 1; API 2 is opt-in and selects retrieval semantics 2. Both use baseline-v1 score formulas. New/migrated namespaces remain legacy_access_v1; explicit_feedback_v2 is an explicit namespace migration. Legacy API keeps configured query-local RIF; API 2 defaults off, with operator-only shadow/experimental modes. No persistent RIF. |
| Current/history and tombstones | V2 current returns proven eligible terminals or omits unresolved/stale predecessors. History returns retained eligible versions without inheritance, not as-of reconstruction. Neither mode relays associative activation through tombstones/ghosts. Content-free tombstones may only help inspect a Supersedes chain. |
| Strength source | Lifecycle v1 uses canonical materialized strength for eligibility/ranking; current projection is diagnostic. Lifecycle v2 uses a single-clock pure projection consistently. API version does not change lifecycle policy. Projection cache identity includes clock/policy/generation. |
| Filters, budgets and coverage | A2's profile is authoritative: B=min(1600,max(100,8L)), entity 2L, FTS 12,800 examined, graph 400 expanded/12,800 edges/depth 0–3, supersession 64 per origin/12,800 per query. No competing A5 hidden union/node cap. Hard filters precede allowances; all limiting/degradation is reported. |
| Namespace and embedding compatibility | By-ID operations resolve the record namespace; explicit scope is an assertion. Memory namespace is immutable; new edges stay within a namespace. Explicit allCompatible similarity is the only cross-scope exception. Dimensions 1–8192 still require a matching single active profile; equal dimension alone is insufficient. |
| Readiness and fallback | Ready success requires publication. Post-commit failure carries receipt, committed state and readiness. Canonical point reads work after pending effects replay; unavailable search views fail explicitly, or independent ready channels return labeled degraded coverage. No invented canonical FTS scorer. |
| Performance thresholds | Instrumentation: <=5% overhead. Optimization: >=10% targeted improvement, <=5% collateral regressions. Correctness/API/lifecycle rollout: no speedup required, <=10% p95 overhead. Backend replacement has A1's separate migration gates. Mixed changes establish a corrected baseline first; allowances do not stack. |
| Versions and provenance | A1 owns disk/receipt/export versions and persisted lineage/profile/lifecycle provenance; A4 owns wire names/profile definition; A2 owns rank provenance; A6 owns source mappings/manifests/traces; A5 contributes measured counters. Unknown provenance remains unknown. |
| Frozen harness state | A6 consumes A1 receipts/GenerationRefs/readiness and A3 clock. Snapshot drains transitive work and freezes policy/RNG/behavioral caches; reset clones a fresh isolated root and verifies logical state/ranks. Fixture-only lineage preservation is explicitly distinct from production restore. |

## Dependency-ordered implementation slices

| Slice | Implementable boundary | Owner / prerequisites | Exit evidence before dependent release |
| --- | --- | --- | --- |
| 1. Containment and observability | Strict quality/dimension validation, namespace/slot guards, disable unsafe compaction, truthful annotations; add low-overhead spans | A1/A4; A5 instrumentation can proceed alongside | Rejection/no-mutation regressions; slot ownership and initial recovery tests; overhead measurement before enabling instrumentation broadly |
| 2. Canonical mutation foundation | Generation-selected compaction, ownership/phase serialization, strict backup/restore, removal ledger, receipts/batch manifests/effects and readiness | A1 after slice 1; consume fixed A3 logical schema and A4 wire definitions | Crash/reopen, ledger restore coverage, namespace purge/retry, pending-effect and publication tests |
| 3. Shared lifecycle and public contract | A3 clock/policy/planner, atomic feedback/exposure, false-mode reads; A4 versioned envelopes, discovery/profile validation and by-ID scope | A3/A4 after slice 2; pure planner/parser work can start earlier | MCP/HTTP/daemon parity, historical receipt replay, unchanged false-mode state, migration fidelity |
| 4. Corrected retrieval | Prefiltering, namespace postings, equal inheritance, current/history, real hops, coverage/explanations and RIF-off v2 | A2 after coherent slice 2 views and slice 3 clock/observation | Exhaustive eligible oracle, existing/injected replacement equality, graph/version/filter boundaries, unavailable/degraded behavior |
| 5. Integrated harness and opt-in rollout | A6 isolated public actions, controls, frozen manifests/source mappings and scorers; A4 exact pagination/client compatibility | A6/A4 after slices 2–4; scaffolding/fixtures may start earlier | Three deterministic reset/replays, deletion/lineage/batch fault matrix, exact pair pagination, supported clients, <=10% rollout cost gate |
| 6. Measured optimization and backend comparison | A5 complete pipeline measurements; reversible cache/lock/batching changes; A1 redb/SQLite comparison behind the same interfaces | A5/A1 with slice 5 workloads | Five-run paired distributions, class-specific latency/resource gates, exact parity, crash/restore/reverse-export fidelity |
| 7. Optional promotion experiments | A2 ranking/RIF, A3 lifecycle default, ANN only at measured crossover, eventual A6 learning readiness | Owning action after corrected baseline and its separate empirical gate | Frozen grouped utility/quality/cost results; no automatic policy/backend/index change and no training authorized here |

Early inheritance regression/fix and parser/pure-function work need not wait for a backend replacement. Do not advertise complete v2 mutation semantics until all participating write paths satisfy slices 2–3. Keep v1 available for at least two releases; passing correctness tests does not automatically change the default contract or lifecycle policy.

## Ownership matrix for shared files and handoffs

| Surface | Accountable owner | Required handoff |
| --- | --- | --- |
| Storage engine/metadata/backup, schema/export and removal ledger | A1 | A3 supplies logical lifecycle fields; A4 supplies wire requirements; A6 tests restore/replay |
| System readiness and mutation barriers | A1 | A3 registers lifecycle work; A2 pins views; A4 serializes readiness; A6 waits/drains |
| Clock, decay planner and exposure/feedback meaning | A3 | A1 atomically persists; A2 consumes projections; A4 propagates flags; A6 controls isolated time |
| Query/pipeline/graph/entity/FTS and ranking diagnostics | A2 | A1 generation authority; A3 strength source; A4 serialization; A5 spans; A6 oracle fixtures |
| MCP/API/daemon schemas, namespace/profile identity and exact duplicate pagination | A4 | A1 receipts and canonical profile storage; A2 eligibility/exact ordering; A3 observation; A6 client parity |
| Timings/cache/lock/batching optimization | A5 | A1 durability, A2 ranking budgets, A4 profile identity; measurements emitted into A6 traces |
| Bench harness/controls/manifest/trace/source sidecars | A6 | Consume A1–A5 contracts; no private ingestion semantics, alternate clock or competing canonical receipt system |

The accountable owner edits the shared interface; consuming workstreams update callers against that interface. No implementation agent is assigned a remaining integrator decision.

## Evidence boundary and remaining empirical release gates

The completed [baseline REPORT](../experiments/baseline/REPORT.md) establishes **487 passing baseline tests** and **seven added tests: three pass, four fail**. Its actual Rust supersession stage reproduces **0.9/rank 1 versus 0.1/rank 11**. The d384/10k exact-index p95 is **232.835 µs**, one process run excluding the full pipeline. [Python ablations](../experiments/ablations/REPORT.md) and the SQLite toy transaction probe remain models; they do not establish real-engine quality, recovery or throughput. Artifact checks were already complete and were not rerun for this review.

Only execution evidence remains open:

- **Recovery/fidelity:** real implementation crash, I/O/ownership, slot-reuse, migration, retained-ledger/historical-restore and reverse-export tests on declared supported local filesystems. Process exits alone do not prove power-loss durability.
- **Public conformance and replay:** actual transports/clients, all seven regression expectations, batch uncertainty/retry, lineage, schemas, no-write queries, strict-current/hops/filters, profile compatibility, exact pagination and frozen reset/barrier checks.
- **Measured cost and operational readiness:** A5 baseline hardware/workload characterization, absolute p95/RSS/error budgets frozen before candidate measurement, five-run comparisons and bounded canary evidence. Budget values require measurements; their owner and selection protocol are settled. A failed performance gate pauses promotion without undoing correctness.
- **Backend/index choice:** A1 fidelity/parity and ratio gates select SQLite only if passed; redb remains its measured comparator. Exact stays default; ANN requires the measured crossover and recall/utility gates.
- **Policy utility and evaluation readiness:** frozen rights-cleared artifacts/model outputs, verified provenance/scorers, A6 grouped sample plan, A2 RIF/ranking and A3 lifecycle quality gates. Insufficient strata or inconclusive intervals retain existing defaults. Learning/live spend remain outside this review's authorization.

## Review verification and changed files

Bounded documentation verification passed: 105 relative local links across ten documents resolve with exact filename case; both JSON examples parse and use decimal-string generations. Cross-spec defaults/thresholds and leftover integrator-choice wording were checked. No experiments were rerun; this does not certify future acceptance tests.

Changed: [CONTRACTS.md](CONTRACTS.md), [A1](A1-storage-recovery.md), [A2](A2-retrieval.md), [A3](A3-lifecycle.md), [A4](A4-tools-namespaces.md), [A5](A5-performance.md), [A6](A6-evaluation-harness.md), this review, and two narrow consistency edits in [REPORT.md](../REPORT.md). README already links this review and is unchanged. Production source and experiment artifacts are unchanged by this review.

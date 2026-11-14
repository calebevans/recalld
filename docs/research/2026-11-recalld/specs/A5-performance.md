# A5 — Actual-engine performance instrumentation and optimization gates

Status: proposed implementation specification; no production changes or new experiments in this deliverable.
Owner: A5 performance. Research baseline: `5aa4692c7d326d2ef10712939eb01484a520e891`.

## Purpose and boundaries

Measure the real request path before selecting optimizations. Preserve durability, namespace eligibility, lifecycle semantics, and retrieval quality while bounding work and output. Keep exact vector search as the oracle and default until an actual workload demonstrates a reason to change it.

This specification follows [CONTRACTS.md](CONTRACTS.md), the [research report](../REPORT.md), and the [ablation report](../experiments/ablations/REPORT.md). A5 owns measurement and optimization gates, not storage transaction semantics (A1), ranking/eligibility (A2), lifecycle semantics (A3), public contracts (A4), or benchmark datasets and trace schema (A6). All numerical gates below are proposed acceptance thresholds, not achieved results. Implementation must record its own source revision and configuration.

## Evidence available now

- `src/search/pipeline.rs` and `src/search/response.rs` already expose `StageTimings`: parsing, embedding, vector/FTS/entity retrieval, fusion, metadata loading, graph expansion, filtering, strength/RIF, ranking, response construction, and pipeline total. Extend this instrumentation instead of replacing it.
- Supersession resolution and its subsequent sort lack their own timing. Pipeline total is captured before asynchronous access recording and does not measure transport serialization or complete background work.
- `src/search/adapters.rs` wraps storage in `std::sync::RwLock` with `spawn_blocking`; vector and FTS synchronous interfaces bridge to Tokio locks with `block_in_place`. Metadata batches hold one read guard while loading records; access batches loop over individual updates. These are contention hypotheses, not measured bottlenecks.
- `src/search/index.rs` uses an exact flat SIMD search with a bounded heap. It tests metadata predicates before dot products but iterates index entries. The shared vector adapter currently passes namespace rather than all query predicates. Entity recall is globally collected before later eligibility checks.
- `src/storage/engine.rs::insert_memory` writes vector/text data, syncs it, then commits metadata. Text/vector and metadata persistence are separate resources. Removing those flushes without a replacement commit protocol is not an optimization permitted by A5.
- `src/embedding/cache.rs` uses a bounded-entry Moka cache keyed by a 64-bit text hash. Query/document distinction can depend on prefix wrapping. Batch misses are gathered before insertion, so repeated uncached texts in one batch are not necessarily deduplicated despite the comment. Profile identity, collision-safe equality, and in-flight deduplication require explicit treatment.
- The Python ablations demonstrate candidate starvation and model operation counts. Their dot-product/load counts are not measured Rust latency. The SQLite probe establishes synthetic transaction atomicity, not recalld durability or throughput. Saved LoCoMo retrieval means may include LLM query construction and cannot supply engine p50/p95.

### Actual-index microbenchmark supplied during specification

The completed [baseline REPORT](../experiments/baseline/REPORT.md) records a release `FlatVectorIndex` microbenchmark executed in isolated Docker, Rust 1.100, ARM64, dimension 384, `k=10`, 20 warmup queries and 100 measured queries per cell, in one process run:

| Records | Eligibility | p50 (µs) | p95 (µs) |
| --- | --- | ---: | ---: |
| 1,000 | Unfiltered | 22.458 | 23.042 |
| 1,000 | 10% eligible | 3.542 | 4.000 |
| 10,000 | Unfiltered | 223.752 | 232.835 |
| 10,000 | 10% eligible | 33.042 | 34.584 |

These are actual Rust index timings using precomputed vectors, not a full-engine benchmark. They exclude embedding, storage, API transport, and eligibility-construction costs. RSS, throughput, lock contention, repeat-run variance, and full-pipeline percentiles were not supplied. The [raw samples](../experiments/baseline/vector-benchmark-results.json), [harness](../experiments/baseline/research_flat_vector.rs), [environment log](../experiments/baseline/logs/environment.log) and [source revision](../experiments/baseline/source-commit.txt) are packaged. The report binds them to baseline 5aa4692c7d326d2ef10712939eb01484a520e891; no rerun was performed for this integration review. No speedup ratio or production capacity claim follows from this single run. In particular, these measurements do not justify ANN.

## Instrumentation contract

Reuse existing `tracing`, `metrics`, and `StageTimings`. Add internal spans/counters at the real adapters and engine boundaries; do not create a second query implementation. A6 owns trace serialization/versioning; A4 owns any public diagnostic fields. Keep optional diagnostics additive and preserve existing timing fields and legacy response decoding.

| Boundary | Required duration and counts |
| --- | --- |
| Request | Client enqueue-to-response latency; server admission/queue, decode/validation, namespace resolution, serialization/send; request/response bytes, status, timeout/rejection count |
| Embedding | Cache lookup, provider queue, provider call, validation; hits/misses/evictions, unique misses, coalesced callers, input bytes, requested/returned vectors, retries and errors |
| Candidate sources | Vector/FTS/entity times separately; entries inspected, eligible entries, dot products actually executed, per-source candidates, deduplicated union, cap hits |
| Metadata/graph | Cache hits/misses, storage records/bytes read, nodes/edges visited, graph frontier peak, unique expansions, filtered/rejected records, replacements loaded |
| Ranking/output | Strength and RIF, ranking, supersession, final sort, content fetch, packing; pre/post-filter counts, returned records, omitted content, serialized bytes/context tokens |
| Writes | Admission, embedding, storage-lock acquisition, append/write, each sync, metadata transaction/commit, projection publication, acknowledgment; records/bytes, commits and flushes |
| Background | Access recording, decay, compaction, index rebuild; queue wait/service, completions/failures, backlog, generation lag, readiness-barrier time |
| Process | RSS baseline/peak/steady state, CPU time, completed requests/s, durable records/s, active workers and queue high-water marks |

Use monotonic elapsed time for performance and A3's injected clock for lifecycle; never derive latency from the lifecycle clock. Record request ID, operation kind, resolved namespace scope, generation, budget profile, embedding profile fingerprint, and variant in isolated traces. Production metric labels must be bounded categories, not UUIDs, arbitrary namespace names, raw text, or credentials.

Measure blocking-worker queue delay from submission to closure start; measure lock wait immediately before acquisition to acquisition, and hold from acquisition to guard release. Separate storage read/write, vector read/write, FTS mutex, entity, graph, and phase-index locks. Trace nested locks and long holders; redact payloads. A stage's wall time already contains its lock wait: report nesting rather than summing it twice.

Record errors and cancellations as outcomes, not missing samples. Track exporter drops. Legacy asynchronous-access timing remains a labeled baseline measurement. Under the integrated contract, synchronous access commits/publication are included in true-mode response latency; remaining transitive background work has separate spans and the shared A1/A6 barrier. `recordAccess=false` must produce zero memory/lifecycle writes; diagnostic collection cannot change retrieval state.

For each request, account for non-overlapping top-level spans and an explicit residual. Do not add concurrent child times to claim wall time. Instrumentation-on/off comparison must show at most 5% p95 latency and throughput regression at the declared load; otherwise reduce diagnostic sampling and rerun. Benchmark counters remain complete or state their sampling rate explicitly.

## Bounded work, response, and context

A2 defines eligibility and candidate-budget semantics; A4 validates inputs and exposes truncation. A5 measures enforcement at allocation/iteration boundaries, not just after building an oversized response. No bounded search promises exhaustive matches. Existing `total_matches` is a candidate count, not corpus cardinality.

Use named experimental budget profiles, initially preserving existing public defaults. Proposed benchmark settings are:

| Resource | Benchmark profile and required enforcement |
| --- | --- |
| Result count | `k=10,20,100`, within A4-approved limits; validate before multiplication/allocation |
| Candidate sources | Compare existing `min(max(8*k,100),1600)` vector/FTS fetch behavior with A2-approved prefiltering; sweep 160/400/1600 per source at `k=20` |
| Combined candidates | A2 per-source bounds and graph edge work bound retained IDs; no hidden 3,200 union cap. Deduplicate before admission and report source overlap. An alternative union cap is an explicitly named experimental arm only. |
| Graph/replacements | A2 default: 400 expanded states, 12,800 examined associative edges, max depth 0–3; Supersedes 64 edges/origin and 12,800 examinations/query. No extra 1,600-node cap. Report cycle/branch/work exhaustion. |
| Metadata/content | Stream canonical eligibility and metadata scoring without a hidden scan cap; retain A2-bounded candidates and fetch full content for selected results. Measure transient bytes separately. |
| Response | 64 KiB/256 KiB are experimental output budgets, not new public truncation defaults. Default preserves validated results; an existing transport overflow returns RESPONSE_TOO_LARGE. |
| Answer context | A6 packs at 2,048 and 8,192 tokens using a pinned tokenizer, counting graph text, provenance and separators; same cap across variants |
| In-flight work | Initial load-test ceiling: 32 admitted requests and 64 queued; explicit unavailable/backpressure when full; separately bound embedding and background queues |

CONTRACTS fixes public defaults; alternative candidate/response/load settings here are named benchmark arms, never silent API changes. Include a legacy-budget control. A candidate cap does not bound an exact scan's CPU: count inspected entries and enforce admission/deadline checks between chunks. An interrupted scan must return an explicit incomplete result or error under A4's contract, never silently advertise exact completeness.

Return which resource exhausted, applied limits, consumed counts, omitted content, and whether completeness is unknown. Keep eligible result IDs/provenance when shortening optional content; never emit invalid JSON or split UTF-8. A result that cannot fit the existing transport limit errors with RESPONSE_TOO_LARGE; it is not silently omitted. Experimental packing arms label every omitted byte/result. Use source IDs to allow follow-up reads. Pagination does not imply a complete scan or stable snapshot unless A1/A4 provide it.

Response bytes and model tokens are different budgets. Engine response packing and A6 answer-context packing must be separately timed. Prevent an answer-quality comparison from buying better results through larger graph context or longer summaries. Exercise control characters, multibyte text, maximum-length content, dense graphs, and adversarial filters.

## Optimization sequence and gates

### 1. Correctness baseline, prefiltering, and namespace indexes

First establish a corrected exact baseline with A1 containment, A2 eligibility, and A3 observational queries. Keep a historical baseline separately; a correctness repair is not a like-for-like speed comparison.

Measure eligibility construction plus scan plus metadata loads. A2 must prove equivalent hard-filter semantics for vector, FTS, entity, graph and supersession candidates before pushdown. Unknown/dynamic predicates remain checked against canonical state; stale phase/strength projections cannot silently exclude valid records.

Compare the shared flat index with namespace-to-slot postings or per-namespace exact partitions, choosing the smallest change supported by profiling. Namespace-local entity postings must apply eligibility before the cap. Reuse existing namespace-aware storage phase indexes where semantically valid; do not assume they already index every search predicate.

Derived indexes carry the A1 generation and can be rebuilt. Test namespace deletion/recreation, slot reuse, tombstones, phase transitions and concurrent writes. Acknowledged writes must be immediately readable through consistent publication or A1's shared readiness barrier: canonical point reads after effects replay, explicit unavailable for affected search channels, and labeled degraded coverage only from independent ready channels. Missing/stale projections cannot become empty-success responses. Measure index memory, rebuild time and mutation overhead alongside query gains.

### 2. Lock scope and blocking work

Use wait/hold and queue distributions to choose changes. First batch metadata reads, reduce redundant acquisitions/copies, and move provider/network work outside storage locks. Preserve bounded blocking workers; spawning unlimited blocking tasks only moves the queue.

Consider shorter guards or immutable generation snapshots only when contention dominates. Snapshot publication must preserve A1 read-after-write guarantees. Document lock order, prohibit awaiting network work while holding synchronous storage guards, and exercise concurrent search/store/delete/decay/compaction. Do not split locks or parallelize retrieval stages merely because they can be split; measure the resulting CPU, memory, fairness and writer tail latency.

### 3. Fsync and write batching without reduced durability

A1 owns the transaction/receipt protocol and recovery proof; A5 owns batch-size/dwell experiments and timing. Begin with explicit caller batches, then consider group commit only after those measurements. Sweep batch sizes 1/8/32/128 subject to A4's public cap, and maximum dwell 0/2/5 ms for queued group commit. Bound both records and buffered bytes (initial test cap 8 MiB); oversized allowed items use a separately bounded path.

For the current multi-file layout, all referenced vector/text bytes must be durably flushed before metadata commits that reference them. A group may share required flushes, but no member receives success before its durable commit and required projection readiness. Directory/rename durability and compaction ordering remain A1 responsibilities. For a consolidated backend, use its A1-approved durability configuration; disabling sync is never a valid comparator.

Report batch wait, write/sync/commit time, flushes per durable record, committed records/s, and per-request acknowledgment p50/p95. Public per-item outcomes remain per-item unless A1/A4 explicitly introduce an atomic batch. Cancellation after commit cannot undo a durable operation; retries use A1/A4 operation receipts and idempotency rules. A failure must not acknowledge uncommitted members or conceal a partial batch.

A1 crash tests must interrupt before/after data flush, metadata commit, projection publication and acknowledgment, including sync failure/disk-full and restart. Every acknowledged operation survives with the same UUID/content/vector/lifecycle/provenance and receipt. An unacknowledged committed operation is discoverable by retry. Process-kill tests do not establish power-loss durability; document platform/filesystem guarantees and any remaining fault-model gap. No batching rollout before this gate passes.

### 4. Embedding cache and provenance

Retain the single provider initially. A4 defines profile identity: provider/model revision where available, dimensions, normalization and document/query preprocessing/prefix versions. Cache identity additionally selects the query/document task and exact transformed input; it does not invent a competing namespace profile fingerprint. A1 persists profile provenance with stored vectors; A6 persists it with fixtures. Unpinned provider revisions are explicitly recorded as such.

Cache identity is profile plus task plus exact effective input, with equality verification rather than trusting a 64-bit hash collision. Different query/document semantics must remain distinct even without prefixes. Reconfiguration invalidates the relevant cache; no reuse across incompatible dimensions/models. Validate vector count, dimension and finite values before caching or publication.

Deduplicate identical misses within a batch and, if concurrent misses justify it, coalesce in-flight calls with bounded waiters and cancellation/error handling. Preserve input order and per-item semantics; do not cache provider failures as valid vectors. Bound retained bytes as well as entries, accounting for vectors, keys and cloned results. Measure hit rate, provider calls, bytes, duplicate-miss reduction and RSS at 0/50/90% repeated inputs. Cache-off remains a supported rollback path. No multi-provider routing or semantic query-result cache is required by A5.

### 5. Exact oracle versus ANN

Do not implement ANN based on corpus-size folklore or the supplied kernel timings. First compare exact search with eligibility pushdown, namespace partitioning, batching/cache and lock improvements on the full pipeline. The exact oracle uses identical vectors, metric, normalization, eligible generation and deterministic tie rules. Compare candidate Recall@k separately from final A2 ranking and A6 answer utility.

ANN investigation opens only when repeated measurements at a demonstrated target scale miss a predeclared full-pipeline latency/throughput budget, and exact vector work consumes at least 30% of engine service time after simpler changes. Report the actual resident count, dimension, eligibility and hardware at this crossover; no assumed million-record production workload.

A candidate ANN must achieve mean eligible-oracle Recall@10 and Recall@20 >=0.99, and >=0.95 in every declared filter/namespace cohort, with zero hard-filter violations. Require at least 20% full-pipeline p95 or throughput improvement at equal load/budgets, with no >5% regression in the other metric and within the declared RSS cap. A6 must show no >1 percentage-point retrieval/answer-quality loss with uncertainty reported; insufficient evidence blocks rollout. A2 approves any approximation semantics.

Include build/rebuild time, persistent and resident size, update/delete cost, readiness lag, sparse-filter fallback and restart behavior. Shadow comparison is observational and bounded; never double-record access. Retain exact fallback and an index-off switch. ANN indexes remain derived, not canonical storage.

## Concrete benchmark matrix and reporting

A6 supplies deterministic isolated fixtures and runners. A5 adds actual-engine instrumentation. No personal corpus, daemon socket, credentials, paid inference or training is needed. Fixture vectors may be precomputed, but must enter the real engine; label such runs as embedding-excluded. Optional provider-inclusive work is a separate approved track.

| Track | Required cells |
| --- | --- |
| Exact kernel control | Reproduce supplied 1k/10k, d384, k10, 100%/10% eligible cells; extend to 100k only when resources permit |
| Engine retrieval | 1k/10k/100k records; d384/1536; k10/20/100; vector-only, hybrid, metadata-only; fixed clock and `recordAccess=false` |
| Eligibility/namespaces | 100%/10%/1% eligible; random and low-score eligible layouts; 1/10/100 namespaces, uniform and 90% hot-namespace skew |
| Contention | Clients 1/8/32; 100% reads, 90:10 reads:writes, 50:50; record-access off/on; decay/compaction off/on |
| Durable ingestion | Single and 8/32/128-item batches; summary-only and 4 KiB/64 KiB text; cache-off and 50%/90% repeated embedding input |
| Output/context | k20/100; 64/256 KiB response and 2,048/8,192 context-token caps; dense graph, long text, escaping and sparse-result fixtures |
| Recovery/scale | Restart and index rebuild at each completed scale; compare A1-approved redb/SQLite variants only with equal semantics/durability |

Avoid a blind Cartesian product. Anchor: 10k records, d384, k20, 10 namespaces, 10% eligibility, hybrid retrieval, one client, cache-off, observational query. Sweep each factor against the anchor; additionally run 1% eligibility with 100 namespaces and 32 clients, mixed writes with compaction, and maximum output with dense graph. Repeat every affected cell for a proposed optimization. One million records is an optional later scale gate, not a deliverable or evidence already obtained.

For each steady-state cell: five independent process runs, seeds 7/19/41/73/101; restore the same initial snapshot; at least 200 warmups then 1,000 completed measured operations per run. Sustain contention/throughput runs for at least 60 seconds and 1,000 completions. Report actual sample counts; if a run cannot complete, retain its failures and mark the cell incomplete. For cold-process latency, use 100 independent opens/first requests; do not label OS page-cache-warm runs cold-disk.

Use release builds and pin source/diff, lockfile, Rust version, target/SIMD, CPU, RAM/cgroup limit, filesystem/device, OS/container image, worker count, fixture hashes, embedding provenance and durability flags. Fsync comparisons require a real persistent filesystem; tmpfs is only a separately labeled CPU/logic control. Randomize paired variant order; drain background work and reset state between runs.

Report both closed-loop concurrency and open-loop offered-load curves at 25/50/75/100/125% of measured baseline sustainable throughput. Timestamp at scheduled arrival to expose overload queues; count admission failures/timeouts. Never compare throughput at different latency/error budgets or infer sustainable QPS as the inverse of a single kernel latency.

Each result row must include: variant/cell/run, attempted/completed/failed/timed-out/rejected counts, elapsed seconds, client and server p50/p95, stage p50/p95, lock wait/hold p50/p95, baseline/peak/steady RSS MiB, successful requests/s, durable records/s, provider calls, cache hits/misses, actual scan/dot-product/load/graph counts, commits/fsyncs, response bytes/tokens, budget exhaustion, and oracle quality. Use `not measured` for missing values. Include raw samples or mergeable histograms, per-run results and paired 95% confidence intervals; never average percentiles or mix unlike cells into one p95.

## Acceptance, rollout, rollback, and dependencies

Acceptance requires all of the following:

1. Instrumentation covers the public MCP/daemon path and actual `QueryEngine`/storage path, distinguishes provider and background time, reconciles counters on small fixtures, and meets the <=5% overhead gate. API coverage is included where the changed adapters are shared.
2. A1/A2/A3/A4 correctness suites pass: no lost acknowledged writes, namespace leakage, stale-generation silent omission, altered feedback semantics, or incompatible embeddings. Exact optimizations preserve eligible results and scores within documented f32 tolerance/tie rules.
3. Budget tests enforce bounded allocation/queues and valid truncation/error metadata at boundary and over-limit inputs. Observational evaluation has zero lifecycle writes and bounded RSS under overload.
4. Before comparing variants, record target hardware, workload, absolute p95 SLA, RSS cap and maximum error rate in the A6 manifest. These absolute budgets remain unresolved until baseline measurements; missing budgets block performance acceptance.
5. A performance-only change delivers >=10% improvement in its declared full-pipeline p95, sustainable throughput, or peak RSS target, with a paired 95% interval excluding zero improvement; other p95/throughput/RSS metrics regress by no more than 5% and stay within absolute budgets. Correctness/API/lifecycle rollout uses the separate <=10% p95 gate in CONTRACTS and needs no speedup; disclose costs and pause promotion if that gate fails. Instrumentation keeps its <=5% overhead gate. Backend replacement uses A1's migration ratios. Mixed work first establishes the corrected baseline, then tests the optimization separately; allowances are not stacked. ANN uses its stricter gate above.
6. Durability batching passes A1 interruption/retry tests; cache changes pass profile/task/collision/batch-order/error tests; index changes pass generation/delete/rebuild tests. Raw benchmark artifacts make every claim reproducible, with no synthetic-count-to-runtime inference.

Roll out in independently reversible stages: instrumentation only; A2-approved prefiltering/namespace projections; measured lock changes; cache changes; A1-approved batching; ANN only if its gate opens. Start each optimization disabled, then run isolated replay and a bounded canary with representative approved workloads. Require five successful benchmark runs plus at least 24 hours and 10,000 completed canary operations before widening; low traffic extends the canary. This spec authorizes no production rollout itself.

For performance-only optimizations, rollback immediately on a correctness/durability/profile violation, or after two consecutive five-minute windows exceeding the declared p95/RSS/error limits or >5% throughput regression at comparable offered load. Correctness/API/lifecycle promotion uses its 10% p95 class gate; rollback must retain validation/deletion fixes, and can disable promotion or use a corrected slower path. Disable the affected optimization, drain in-flight writes through their existing durable barrier, retain receipts, and return to corrected exact search. Discard/rebuild derived indexes or clear caches as needed. Do not abort committed batches or reopen a pre-migration snapshot after new writes as if that were lossless. Canonical backend rollback belongs to A1 and requires replay/export of post-cutover writes.

| Action | Dependency and handoff |
| --- | --- |
| A1 storage/recovery | Supplies durable generation/receipt/readiness contract and crash-safe batching/backend implementation. A5 supplies write-stage counters and equal-durability workload comparison. Immediate repairs precede batching/migration. |
| A2 retrieval | Owns eligibility, source/graph/replacement budgets and ranking. Supplies exact semantic fixtures before prefilter/index/ANN changes; A5 supplies cost and oracle comparisons. |
| A3 lifecycle | Supplies injected clock, observational mode and drain semantics. Required before comparable replay and side-effect-free shadow queries; A5 measures feedback/background costs separately. |
| A4 tools/namespaces | Owns strict bounds, namespace resolution, embedding profile validation, pagination, truncation/errors and compatibility. Approves public budget/diagnostic fields before rollout. |
| A5 performance | Can begin baseline spans immediately. Integrates measurement only after shared semantics stabilize; optimizations remain separately gated and reversible. |
| A6 evaluation | Owns fixtures, splits, trace format, provenance, reset/barrier and quality evaluation. Supplies comparable workloads; consumes A5 raw timing/counter/RSS outputs. No RL prerequisite. |

Implementation touchpoints are existing `src/search/{pipeline,response,adapters,index,entity_index}.rs`, `src/api/{handlers,adapters}.rs`, `src/daemon/server.rs`, `src/mcp/{bridge,bridge_adapters}.rs`, `src/embedding/{cache,provider,prefix}.rs`, and A1-owned `src/storage/{engine,metadata,text,vectors,fsync}.rs`. A6 extends `src/bench/` and isolated harness artifacts. Coordinate changes at these boundaries rather than creating competing interfaces.

Design handoffs are closed by CONTRACTS: A2 budgets, A1 receipts/readiness, A4 profile identity and change-class thresholds apply unchanged. Kernel evidence is attached above. Remaining empirical gates are baseline workload/hardware characterization and preregistered absolute p95/RSS/error budgets, measured optimization/canary results and fixture profile/revision verification. An unpinned provider revision is explicitly unknown, never guessed. These block dependent performance claims/promotion, not containment.

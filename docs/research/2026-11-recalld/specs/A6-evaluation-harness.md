# A6 — Evaluation and learning harness

Status: **proposal; production behavior is not implemented by this document**. Written 2026-11-13 against research baseline `5aa4692c7d326d2ef10712939eb01484a520e891` and available research artifacts.
This deliverable is specification only: no training, paid evaluation, experiments, or personal MCP access. Integrated against CONTRACTS revision 1; A6 owns the harness boundary, not competing storage or public-action semantics.

## 1. Evidence, ownership, and decisions

Read alongside [CONTRACTS.md](CONTRACTS.md), [REPORT.md](../REPORT.md), [external memory research](../evidence/external-memory-research.md), and the available sibling [A3 lifecycle spec](A3-lifecycle.md).
The completed [baseline REPORT](../experiments/baseline/REPORT.md) records 487 baseline passes and seven added tests (three passes, four failures). Its supersession reproduction exercises an actual Rust stage, not a full request pipeline; the d384/10k exact-index p95 of 232.835 µs excludes that pipeline. Synthetic ablations are complete, but do not establish public-API correctness, conversational utility, or production performance. None of the gates below is satisfied merely by those results.
The current [benchmark methodology](../../../benchmark.md) describes extraction, query planning, retrieval, graph context, answering, and judging together, with decay disabled and an ingestion path that bypasses public validation. Preserve those results as a named legacy regression track.

| Owner | Required boundary |
|---|---|
| A1 | Canonical schema, transactions, receipts, generations, snapshots/recovery and terminal deletion identities. A6 consumes these guarantees. |
| A2 | Eligibility, ranking, provenance-bearing results, candidate diagnostics and historical-query semantics. A6 measures rather than reimplements retrieval. |
| A3 | Clock, lifecycle versions, feedback and sweep semantics. A6 supplies isolated clock control and episode schedules. |
| A4 | Public schemas, namespace resolution, strict validation and retry contracts. A6 sends serialized requests through these contracts. |
| A5 | Instrumentation and absolute latency SLO. A6 supplies frozen workloads, paired comparisons and resource accounting. |
| A6 | Isolation, replay, manifests, splits, provenance mappings, scoring, trace format and learning gates. |

Defaults: contractVersion 2 / retrievalSemanticsVersion 2 (with separate named legacy contract-v1 parity/comparator tracks); offline replay; no credentials or network; one isolated store per episode; serialized actions; exact vector search; explicit namespace; `recordAccess=false` on every evaluation search/fetch; `legacy_access_v1` lifecycle. `explicit_feedback_v2` is an opt-in comparison only.
Fixed-state diagnostics disable decay with multiplier 0 and disable autonomous sweeps. Chronological episodes use multiplier 1 unless an arm explicitly specifies otherwise, with scheduled explicit sweeps. Keep A3's other constants pinned. Contract v2 uses rifMode=off independent of lifecycleVersion; legacy contract-v1 comparators pin existing configured RIF. Shadow/experimental arms explicitly use A2's capped algorithm.
Use seed 7 for contract fixtures and `[7,19,41,73,101]` for future paired stochastic comparisons. A seed alone does not make an external model deterministic; recorded outputs are required for exact replay.

## 2. Isolated public-API environment

The actor talks to an isolated recalld child process over public MCP stdio, using discovery/initialization appropriate to the pinned protocol version and normal serialized tool calls. HTTP is a contract-parity test target, not an alternative private ingestion shortcut.
Every store, supersession, query, reinforcement and forget must traverse production parsing, validation and handlers. Never silently drop an invalid supersedes target as the current benchmark path can. Record the actual error and resulting state.
The supervisor creates an empty temporary data/config root, allowlists environment variables, supplies a temporary home to the child, and mounts only rights-cleared fixtures and its output directory. It must reject personal data paths, daemon sockets and inherited provider credentials; namespace separation alone is insufficient isolation.
The default embedding provider is a pinned offline replay fixture keyed by complete embedding input and profile hash. A missing response fails closed; it must not fall back to a live provider. Artificial fixture vectors establish contracts only, never semantic retrieval quality.
Actor tools expose no evaluator labels, full-state dump, snapshot handle, clock mutation or reward internals. An evaluator-only control channel supplies the following **proposed interfaces**, confined to the isolated process:

| Control | Contract |
|---|---|
| `barrier(GenerationRef, deadline)` | Use A1's CONTRACTS readiness shape and reject a foreign storeId. Wait for accepted work through that decimal-string generation, including transitive graph/index/lifecycle tasks; report committedGeneration, ready, pendingWork and each derived appliedGeneration/ready. Default deadline 30 seconds of monotonic wall time. Timeout invalidates the episode. |
| `snapshot()` | Stop action admission, drain work, freeze clock, capture canonical records/text/vectors/edges, namespaces, policies, receipts/batch manifests, removal ledger, RNG state and behavior-affecting caches. Return immutable manifest, hashes and generation. |
| `reset(snapshot)` | Terminate prior workers, clone the synthetic snapshot into a fresh isolated root, restore clock/config/RNG, receipts/ledger and indexes at the captured generation. Barrier and verify logical hash before actions. Fixture lineage may be preserved only in this isolated replay domain per CONTRACTS; production restore creates a new lineage. Never overlay a live instance. |
| `advance(delta_ms)` | Reject negative/overflow values; advance the shared A3 manual clock without running background work implicitly. Default initial time is `2026-01-01T00:00:00Z`. |
| `sweep()` | Invoke the same A3 transition planner/commit path, then barrier. One explicit sweep advances at most one destructive phase per record. |
| `inspect()` | Evaluator-only canonical logical-state hash, generations, pending count and diagnostics; inaccessible to actor prompts. |

These controls are not new public production administration endpoints. Production actions still use the public API; control-plane convenience must not introduce alternate action semantics.
The barrier must include recursively scheduled work, not merely an empty queue sampled once. A1 provides canonical point reads after effects replay and explicit search-channel unavailability/degraded coverage; snapshot creation additionally requires derived views to be quiescent and generation-consistent.
Snapshot manifests list index versions and rebuild inputs. Rebuilt indexes must yield identical eligibility/ranks under a declared deterministic tie rule; use stable fixture identity for equal scores. Preserve real UUID mappings in traces rather than erasing identity differences blindly.
Hash logical canonical values in stable key order, including lifecycle fields, receipts/batch manifests, removal ledger and policies. Exclude physical layout and nonbehavioral telemetry. Normalize generated IDs only through an explicit bijection that preserves references and receipt identity.
Pending work must be zero at snapshot boundaries; do not serialize an opaque executor queue. Fault-injection snapshots with pending work belong to A1 recovery tests, not ordinary evaluation clones.

## 3. Transition and lifecycle contract

Define an episode transition as `(state, action, elapsed_ms) -> (next_state, observation, outcome, reward, terminated)`.
Advance time, validate/execute one action, wait for its completion barrier, inspect state, then compute evaluator reward. A timed-out or ambiguous write is not a NOOP and cannot produce a valid reward sample.
Initial actor actions are `STORE`, `STORE_WITH_SUPERSEDES`, `QUERY` and harness-local `NOOP`. Reinforcement and forget are enabled only after their independent contract fixtures pass; the first learning pilot excludes them.
Validate payload, target existence, resolved namespace, phase and generation through A4/A1. Invalid input must cause no mutation. Batch calls retain per-item outcomes; never assume batch atomicity.
Use one explicit idempotency key and discovered storeId per intended mutation, plus explicit namespace/ID. Retain lifetime receipts; no expiry-based resubmission. Batch unresolved/unknown outcomes and historical receipt replay follow CONTRACTS. Retry a transient transport failure at most twice with the same key and payload only once A1/A4 receipt semantics are available. Never automatically retry unkeyed reinforcement. Unresolved outcomes invalidate the episode.
Evaluation queries explicitly set `recordAccess=false` on direct search, fetch, batch fetch and graph-context reads. After a barrier, records, receipts, access history, graph weights, lifecycle state and pending work must be unchanged. Behavioral caches must be frozen or bypassed.
Dedicated exposure fixtures use `recordAccess=true`: lifecycle legacy_access_v1 retrieval resets the decay anchor without stability growth; explicit_feedback_v2 only records exposure. Explicit feedback updates `reinforced_at` and anchor in both versions, using A3's shared planner. Retrieval never supplies implicit usefulness feedback.
Quality omission remains 3; present values must be integers 1–4. Quality 1 is not delete. Repeated keyed feedback applies once; unkeyed feedback is a new event. Ghost/terminal feedback cannot restore content; Summary feedback cannot recreate full text.
Separate source observation time, event time described in text, ingestion time, query cutoff and simulated lifecycle clock. Historical source dates do not automatically age newly ingested records.
Chronological ingestion advances the manual clock by declared source-time gaps; fixed-state ingestion uses a frozen clock and retains original dates as provenance. Equal timestamps use stable source order, then explicit scheduled action order.
Historical questions see only evidence available at their cutoff: construct the appropriate chronological snapshot, then use A2 versionMode=current or history as declared by the task. History means retained versions, not as-of reconstruction; do not emulate historical search by rewriting timestamps or accepting future corrections.
Forget removes content from fresh reads, graph context and caches after acknowledgement; contract-v2 tombstones/ghosts never relay associative activation; only content-free Supersedes-chain inspection is permitted. Resetting a synthetic counterfactual clone may restore an earlier fixture state solely under the isolated reset contract. Production retained-state restore must merge available newer removal state; old-backup-only historical restore reports unknown later deletion coverage and remains isolated unless explicitly promoted as data-loss recovery. Test both modes without pretending to infer lost future deletions.

## 4. Source provenance and traces

Every source unit has `dataset_revision`, `group_id`, `conversation_id`, `session_id`, `turn_id`, role, source timestamp, content hash and stable ordinal. BEIR uses document IDs in place of conversation turns.
A6 evaluator-side source mappings (sourceMappingVersion 1) map every derived memory UUID to all supporting source IDs plus optional character spans, extractor/prompt/output hashes and transformation type: raw turn, round, chunk, summary or extracted fact. Preserve mappings through supersession and retained summaries. These sidecars do not add evaluator labels to canonical memory. A1 preserves stable IDs and any existing stored references; A2 owns rank/channel provenance, A4 wire representation, and A5 measured counters.
Record every input turn, including turns that yield no memory, rejected stores and truncation. Emit a turn ledger with `seen`, `attempted`, `accepted`, `no_op`, `rejected`, or `truncated`, and reason codes. No silent extraction loss.
Keep answers, evidence annotations, `has_answer`, `answer_session_ids`, category labels, annotated event summaries and future queries in evaluator-only sidecars. Strip label-encoding question-ID suffixes from actor-visible IDs. Generated summaries/observations are not ordinary dialogue input by default.
Actor-visible inputs contain allowed dialogue/roles/timestamps, current question/query date and public tool outcomes only. Oracle runs deliberately use labels in a separate context assembler and never populate ordinary ingestion caches.
Source overlap measures evidence location, not proposition preservation. On development data, manually audit 100 derived-memory/evidence pairs, stratified across extraction type and abilities (all if fewer); record support, omission, contradiction and unknown separately.
Report turn coverage at both ingestion and retrieval: fraction of eligible turns processed, fraction represented by accepted memories, and fraction of annotated evidence turns represented/retrieved. Keep unknown/missing annotations outside scored denominators and publish their counts.
Map retrieved memories to unique source IDs before source recall; multiple memories citing the same turn earn one coverage credit. A session citation cannot claim every turn in that session was preserved.
Write append-only `trace.jsonl` with traceSchemaVersion 1, run/episode/group/step IDs, environment hash, action/request, outcome/error, receiptVersion/storeId/operation ID, GenerationRefs and decimal-string counters, readiness, simulated time, snapshot/hash references, source mappings, candidates/ranks, truncation, model/cache IDs, tokens, bytes, latency and reward components.
Store actor observations and evaluator-only labels/rewards separately. Published traces contain only permitted public/synthetic material; do not include credentials. Record partial outputs and failed attempts, not just successful examples.

## 5. Frozen dataset and run manifests

Each run requires a validated `manifest.json` with manifestVersion 1, sourceMappingVersion 1 and every independent version named in CONTRACTS; unresolved revision hashes or missing licenses block execution, not this design document. Do not download datasets or invent commit hashes while implementing the spec.
Dataset fields: upstream URL, exact revision, artifact SHA-256, downloaded filename, license/attribution text hash, intended-use decision, adapter version, record counts, units, excluded IDs/reasons, source-ID mapping and group/split map hashes.
Run fields: source commit plus dirty-patch hash, build/lockfile and container digest, CPU/RAM/thread limits, API/schema versions, lifecycle/policy revisions, seeds, clock/sweep schedule, snapshot hashes, embedding profile/dimensions and tokenizer revision.
Also pin extraction/query/answer/judge model revisions, prompts and cached outputs; chunking, candidate depths, graph budgets, fusion weights, context order, scorer revision, parser/retry rules, resource caps and contamination disclosures.
Cache keys include complete inputs, role, model/profile, prompt, tokenizer and environment configuration. Label-bearing oracle/judge caches are separate from actor caches. Output cache availability is not permission to reuse examples across splits.
Default reported context cap is 4,096 answer-evidence tokens, including fetched text, neighbors, provenance and graph annotations; report fixed prompt/question overhead separately and hold it constant. Truncate deterministically in ranked order and report lost evidence.
Default retrieval requests use `k=20`, evaluated at 5/10/20; graph expansion is off for the initial diagnostic, then a separately named arm with at most 10 extra neighbors under the same token cap. Pin A2 retrieval-v2-default budgets for corrected runs and separately pin historical budgets for legacy comparators before collection; do not invent comparable pools after seeing results.

| Dataset track | Frozen input and scoring contract |
|---|---|
| LoCoMo | Pin the ten-conversation release and verify actual counts. Caption-only if captions are used; no image URL fetching. Upstream scorer: category 1 sub-answer F1, 2/3/4 token F1, 5 phrase-based abstention. Publish categories 1–4, 5, all categories, and per-conversation counts. |
| LoCoMo custom | Preserve existing binary-judge accuracy as a separate named track with its judge/prompt pinned. Never compare it numerically as equivalent to upstream F1. Upstream fallback retrieval recall of 1 is compatibility output only; A6 missing-evidence coverage is unscored. |
| LongMemEval | Cleaned V1 S plus matched oracle, pinned separately; raw-round and extracted-memory indexes are separate arms. Use the upstream task-dependent QA evaluator, including temporal tolerance and update rubric. Report stricter stale-answer diagnostics separately. M and V2 are deferred. |
| BEIR | Full SciFact and NFCorpus corpora, pinned corpus/query/qrels files and official metric implementation. Primary nDCG@10; Recall@5/10/20, MAP@100 and MRR@10 secondary. Retrieve 100 for MAP in this track; use graded qrels as specified by the scorer and disclose unjudged treatment. No answer model. |

License decisions follow the collected external evidence: LoCoMo CC BY-NC 4.0 is not an unrestricted product-training grant; cleaned LongMemEval is identified as MIT with upstream filler provenance retained; SciFact/NFCorpus cards identify CC BY-SA 4.0. Verify exact artifact notices before reuse. Toolkit licenses do not replace dataset licenses.

## 6. Grouped splits and uncertainty

Build groups before extraction, chunking, embedding caches, summaries or reward examples. Connected components of shared users/conversations, evidence histories, aliases and detected duplicate/near-duplicate histories stay together. Freeze dedup method and group map before inspecting scores.
Default allocation for new rights-cleared chronological data: 60% train, 20% development, 20% test by sorted SHA-256 of `a6-split-v1 || group_id`, using whole-group counts and publishing exact assignments. Balance ability coverage only by a preregistered grouping procedure.
LoCoMo is an exposed regression set, not a pristine training holdout: no training by default, two hash-ordered conversations for development and eight locked regression conversations. Also retain a separately labeled historical all-ten track. Ten conversations cannot support claims about thousands of independent environments.
LongMemEval S/M/oracle variants and questions sharing evidence/history stay in the same group. Default is no training, 20% development groups and 80% locked evaluation groups. Select a 50-question ability-stratified adapter subset from development only; if fewer exist, use all and report counts.
BEIR uses native train/development/test query splits where available, with full corpus preserved. If no development split exists, partition native training query groups 80/20; if no training labels exist, use frozen untuned settings rather than tune on test. Shared-query/duplicate conflicts across native splits are quarantined and disclosed as a custom track.
No holdout labels enter ingestion, actor observations, reward collection, prompt/fusion selection or checkpoint choice. Public benchmark pretraining exposure is unknown; disclose it rather than claiming contamination-free evaluation.
For paired comparisons, resample whole conversation/history groups 10,000 times with seed 7; report paired effect and percentile 95% interval, micro score, group macro score and ability/category breakdowns. Resample queries for BEIR only after grouping duplicates.
Default lifecycle promotion sample plan is at least 100 independent rights-cleared history groups, at least 20 query events per group, and at least 20 groups covering each of updates, abstention, explicit forget and multi-hop evidence (overlap allowed). Freeze IDs and schedule before collection; insufficient groups or inconclusive intervals keep the old default.
These are minimum design sizes, not a power guarantee. Report effective group count; the LoCoMo regression alone cannot clear lifecycle or learning promotion gates. Test is opened once per frozen candidate; further tuning requires a new candidate version and fresh holdout protocol.

## 7. Separate metrics and attribution

| Layer | Required measurements |
|---|---|
| Extraction | Turn-ledger coverage, accepted/rejected memories, unique evidence-turn retention, audited proposition preservation, contradictions, duplicate ratio and extraction tokens/cost. Raw-turn/round indexing is the preservation baseline. |
| Retrieval | Candidate evidence recall versus final Recall@5/10/20; all-required-evidence coverage; nDCG at declared evidence unit; MRR only where first relevant hit is meaningful; eligibility violations, stale replacements, truncation and annotated-denominator coverage. |
| Answering | Official dataset metric, separately named custom metric, answerable accuracy, abstention precision/recall, unsupported claims, stale-answer rate and all-required-evidence use. Oracle evidence is a reader diagnostic, not a guaranteed upper bound. |
| Lifecycle | Useful evidence retained, update correctness, deleted-content returns, forbidden namespace reads/writes, phase loss, feedback counts, memory/byte budget adherence and chronological QA. |
| Reliability/cost | Invalid actions, transport/parse/model-refusal failures, retries, timeouts, total calls/tokens/bytes, peak memory, build time, and cold/warm p50/p95 latency with sample count. |

Define diagnostic Recall@k as unique retrieved relevant source units divided by annotated relevant units; all-required coverage is 1 only when every annotated required unit is present. Report session-level and turn-level metrics separately when annotations differ in granularity.
For diagnostic nDCG with binary evidence labels, use gain 1 for relevant units and log2 rank discount; deduplicate units at their first rank. BEIR retains its official qrel/scorer semantics. Zero-label cases are unscored in evidence diagnostics, with counts.
Attribute failures in order: relevant proposition absent after extraction; retained but absent from candidates; candidate omitted/ranked out/context-truncated; evidence present but answer wrong. Missing annotations yield unknown attribution, not extraction failure.
Always-abstain must be visible through answerable accuracy. Wrong answer, refusal, parse failure and transport failure remain distinct. Report both successful-output quality and attempted-example completion rate; failures cannot silently disappear from denominators.
For generative judging, calibrate on 100 development cases stratified by ability and including stale answers, partial multi-hop answers, paraphrases and prompt injection. Require at least 90% agreement with blinded human adjudication before using a binary judge as a promotion/reward gate; otherwise publish diagnostics only and revise on development.

## 8. Costs and chronological ablations

Default allowed external calls and spend are both **zero**. Replay misses stop the run. Future live evaluation requires a separately authorized manifest with provider prices/date, per-stage token/call caps and a dollar ceiling; reserve worst-case retry cost before submitting each call and stop before exceeding the cap.
Report paid cost and estimated replacement cost of cached outputs separately. Cached replay latency is not live-model latency. Include extraction, embeddings, query planning, retrieval, graph fetches, answer generation, judging, storage/index build and retries in totals.
Default chronological budget is 1,000 live memories and 8 MiB retained UTF-8 memory payload per episode, plus the 4,096-token answer-evidence cap. Report indexes/receipts/total disk separately. Budget overflow terminates a constrained arm rather than silently evicting data; unconstrained store-all is a separately labeled ceiling diagnostic.
Run future ablations in this order, freezing all unaffected components and using independent clones of identical start states:

1. Public contract fixtures and deterministic replay; no quality claims.
2. Fixed-state raw-round versus extracted-memory diagnostics; lexical-only, exact vector-only, current fusion, then RRF with c=60. Hold candidate pools constant for fusion comparisons; candidate-generation changes get separate arms.
3. Frozen-answerer QA under matched context budgets, then oracle-evidence diagnosis. Keep legacy and new public-API tracks distinct.
4. Chronological no decay; deterministic oldest-first eviction at budget pressure; A3 v1; opt-in v2. All arms honor explicit forget. Script feedback independently of generated test answers and hold feedback schedules constant.
5. Actual-engine RIF off/current/deduplicated-and-capped comparisons, including direct/entity/graph origins and cumulative suppression; synthetic probes only motivate these arms.
6. Operation policies: store-all, exact normalized-content deduplication, then frozen prompted policy. Compare future query utility and costs under the same limits before considering learning.

Chronological fixtures include redundant facts, corrections, temporal queries, distractors, feedback qualities, forget, restart, and time advances across each phase threshold. Explicit sweep order is part of the trace; no autonomous scheduler races.
Use paired future query streams without feeding their gold answers back into memory. Retrieval observations remain false-mode by default; exposure effects are a separately declared true-mode arm. Never treat successive actions in one evolving store as counterfactual comparisons.

## 9. Constrained learning readiness and promotion

No training is authorized by this spec. A future pilot requires all transition/acceptance gates, rights-cleared grouped data, audited rewards, a frozen environment, a cost-approved protocol and evidence of a remaining controllable policy failure after scripted/supervised alternatives.
First pilot learns only STORE/STORE_WITH_SUPERSEDES/NOOP decisions. Freeze extraction representation, embedding model, retriever, answerer and judge. REINFORCE/FORGET remain scripted until independently justified; no simultaneous manager/answerer training.
Candidate reward version `utility_v1` is future-query mean correctness minus `0.05 * retained_payload_fraction` minus `0.05 * actor_token_budget_fraction`, each cost fraction clipped to [0,1]. Fix the future window to the next 20 scheduled queries, or the remaining episode if shorter; log the denominator and terminal window. Train labels only; validate weights on development before freezing.
Reward correctness uses a calibrated frozen rubric; exact match is allowed only for validated structured-answer tasks. Log each utility/cost component and counterfactual branch, not only the scalar. External judge refusal/parse failure makes reward unavailable rather than zero utility.
Namespace violations, deleted-content return/resurrection, future-label leakage and budget violations are **hard rejection gates**, not offsettable reward penalties. Terminate/quarantine the trajectory and block candidate promotion. Reward shaping cannot relax public API validation.
Training readiness is not deployment readiness. A future pilot uses all five declared seeds and the untouched grouped test set; report seed dispersion and group-paired intervals. Environment/schema/reward changes invalidate prior readiness and require replay and reevaluation.
Policy promotion requires zero hard violations, no completion-rate regression, lower 95% bound of paired QA gain above zero at matched budgets, and p95 latency within A5's absolute SLO and <=10% regression. Inconclusive results retain the scripted policy.
A3 v2 default promotion follows its separate gate: zero hard failures; lower 95% bound of v2-minus-v1 accuracy >= -1 percentage point; plus >=10% fewer retained bytes at matched accuracy or >=2 percentage points less stale answering at matched bytes; A5 p95 regression <=10% and absolute SLO satisfied. No automatic migration of existing namespaces.

## 10. Acceptance tests and implementation touchpoints

The following are future acceptance tests, not results of this documentation task. All must emit replayable traces and declared expected outcomes.

| Test | Pass condition |
|---|---|
| Isolation/offline | Personal paths/sockets/credentials and outbound calls are unavailable; missing cached embedding/model output fails closed. |
| Public validation parity | MCP and HTTP agree on valid requests, malformed quality/dimensions, missing/foreign supersedes and batch outcomes; invalid operations leave canonical state unchanged. |
| Replay/reset/barrier | Replay each fixture three times including process restart; normalized logical hashes and ranks match. Delayed transitive indexing blocks barrier; timeout never scores an episode. |
| Observational queries | Repeated false-mode search/fetch/batch/graph reads leave state/events/receipts/pending work unchanged after drain, including warmed-cache runs. |
| Lifecycle alignment | Day-0 store/day-7 exposure/day-8 quality-3 feedback matches A3 v1/v2 anchor rules; phase thresholds, multiplier 0/1/2, missing summary and sticky permastore follow A3. |
| Retry/recovery | Same keyed action across retry/restart yields one receipt/mutation; changed payload conflicts. Interrupted writes satisfy A1 atomic outcomes; unknown status is never silently replayed as a new action. |
| Deletion/correction | Forget blocks text through search, graph, restart and index rebuild; old receipts cannot resurrect it. Supersession rank is invariant to candidate discovery path subject to identical eligibility; historical cutoff excludes future successors. |
| Provenance/leakage | Every input turn has a ledger result; source mappings survive summaries/corrections. Label canaries never appear in actor inputs/caches, and duplicate histories cannot straddle splits. |
| Scorer adapters | Hand-calculated perfect/empty/partial/duplicate/unknown cases match diagnostics; pinned upstream golden outputs match LoCoMo, LongMemEval and BEIR adapters. Missing evidence never becomes diagnostic recall 1. |
| Costs/reward | Retry tokens and graph context count toward caps; cache miss cannot spend. Adversarial high-utility trajectories violating a hard constraint cannot pass promotion. |

| File/interface touchpoint | Proposed responsibility |
|---|---|
| `src/bench/mod.rs`, new `src/bench/harness/` | Environment supervisor, public client, controls, manifest/trace schemas, replay, split validation and metric orchestration. |
| `src/bench/locomo.rs`, `src/bench/claude.rs` | Preserve named legacy path; add public-API adapter and recorded-model interface without silently changing historical results. |
| New `src/bench/harness/datasets/` and `tests/evaluation_harness.rs` | LoCoMo/LongMemEval/BEIR adapters, label separation, provenance mapping and end-to-end contract fixtures. |
| `src/mcp/{tools,args,bridge_adapters,transport}.rs`, `src/api/{models,adapters,handlers}.rs` | A4/A3-owned strict schemas, recordAccess propagation, truthful outcomes and transport parity. |
| `src/system.rs`, proposed `src/time/mod.rs`, `src/decay/{mod,sweep}.rs` | A3-owned clock injection and shared lifecycle operations; isolated supervisor schedules them. |
| `src/storage/engine.rs`, `src/backup/{manifest,restore}.rs` | A1-owned generation barriers, snapshot integrity, receipts and recovery; A6 adds no competing storage format. |
| `src/search/{pipeline,response}.rs` | A2/A5-owned candidate/rank/provenance diagnostics and timing; observation must not alter ranking. |
| `docs/benchmark.md`, future harness fixtures/manifests | Versioned instructions, metric meanings, permitted data notices and immutable run references after implementation. |

## 11. Rollout, rollback, and integration gates

Stage 0: land this specification and reconcile contracts. Stage 1: implement isolated offline fixtures and manifest/scorer validation; block on public transition failures. Stage 2: migrate benchmark actions behind an explicit new track flag while retaining legacy outputs. Stage 3: separately authorize frozen public-data evaluation and chronological policy comparisons. Stage 4: consider a separately approved learning pilot only after §9.
No production lifecycle/ranking change ships with harness scaffolding. Keep production API contract v1 and namespace lifecycle legacy_access_v1 as independent defaults; opt-in API v2, lifecycle explicit_feedback_v2 and any learned policy have separate flags and evidence gates. New metrics cannot relabel historical benchmark numbers.
Rollback the harness by disabling its new track and retaining manifests/traces for diagnosis; discard only isolated temporary stores. Incompatible snapshots fail closed and are regenerated from permitted source inputs under the original environment, not silently upgraded.
For a later policy rollout, disable the policy and return to the scripted baseline. A3 governs lifecycle rollback: stop sweeps/disable new v2 enrollment and use schema-aware policy rollback; never restore deleted payloads or assume an old backup preserves intervening writes.
CONTRACTS settles lifetime receipts, shared readiness/GenerationRefs, clock/reset construction, independent version defaults, provenance ownership and retained-history semantics. A6 uses those interfaces without a second receipt store, clock or production restore policy. Remaining empirical gates are implemented replay/transport/deletion conformance, frozen artifact/license/model verification, A5 baseline hardware/workload characterization and preregistered absolute budgets, and measured grouped utility/cost results. These are evidence requirements, not unresolved design choices.
Dataset hashes, rights decisions, model revisions and approved live budgets remain execution-manifest requirements. Until populated and all relevant gates pass, the deliverable is an implementation plan with no claim of benchmark improvement or RL readiness.

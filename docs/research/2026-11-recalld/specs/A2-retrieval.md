# A2 — Retrieval correctness and ranking

Status: proposed implementation specification; no production changes or experiments performed by this author.
Owner: A2. Baseline: `5aa4692c7d326d2ef10712939eb01484a520e891` plus inspected working tree.
Contracts: [CONTRACTS.md](CONTRACTS.md); research scope: [PLAN.md](../PLAN.md).

## 1. Problem and evidence

Retrieval must not let ineligible records consume eligible candidate allowances, lose a correction's inherited rank, or describe an aggressiveness setting as a hop ceiling. These are correctness requirements independent of whether new ranking weights improve answer quality.

| Finding | Evidence and limits |
| --- | --- |
| Vector/FTS caps precede metadata eligibility; entity caps precede namespace eligibility; metadata scanning caps before ghost filtering and ranking. | [Retrieval exploration](../evidence/exploration-retrieval.md), source-supported; synthetic filter models demonstrate the mechanism, not end-to-end Rust quality or latency. |
| An independently discovered replacement does not inherit its predecessor's score. | Actual Rust pipeline-method fixture `research_supersedes_inherits_score_when_replacement_already_present` in [validation patch](../experiments/baseline/validation-probes.patch). The completed baseline report records injected replacement score 0.9/rank 1 versus already-present score 0.1/rank 11, missing top 10; regression fails while baseline 487 tests pass. This isolates `resolve_supersedes` with fixture candidates/adapters, not the complete embedding/index/transport path. |
| Graph depth changes budgets/thresholds without tracking hops. | `SharedGraphReader::spreading_activation`, `graph::activation::spreading_activation`; source inspection, no runtime hop result claimed. |
| RIF duplicates shared neighbors, uses placeholder stability, omits query-level cumulative cap, and affects channels differently. | [Retrieval exploration](../evidence/exploration-retrieval.md); [synthetic models](../experiments/ablations/probes.py) and [results](../experiments/ablations/results.json) are source-informed Python models, not Rust pipeline measurements. |
| FTS errors become empty results; score descriptions and channel thresholds disagree. | `run_fts_search`, `fuse_scores`, `passes_filters`, `compute_composite_score`; source inspection. |
| Retrieval records asynchronous access; historical questions can require old versions. | [Tool exploration](../evidence/exploration-tools.md), [external evaluation research](../evidence/external-memory-research.md). A3 owns lifecycle semantics; historical retention is not established by retrieval alone. |

Do not infer production incidence, answer-quality improvement, or speedup from these findings. Later packaged experiment results may revise priorities or budgets; they must not silently redefine correctness.

## 2. Decisions and scope

- Ship eligibility, namespace indexing, supersession inheritance, actual hop bounds, deterministic ordering, and coverage reporting as correctness work.
- Keep existing fusion weights and temporal boost formula initially; identify them as ranking profile `baseline-v1`.
- Introduce retrievalSemanticsVersion `2` selected by contractVersion 2; contractVersion 1 selects semantics 1. Ranking profile and namespace lifecycleVersion remain independent response fields.
- Default v2 RIF to `off`; enable only explicit isolated `shadow` or `experimental` configuration pending the gate below.
- Preserve UUIDs and canonical records. A2 adds derived indexes and response/query fields, not a second canonical storage model.
- Do not add ANN, learned rerankers, embedding providers, persistent RIF mutation, or event-time reconstruction in A2.

## 3. Query and response contract

A4 implements strict parsing and transport schemas; these are the A2 semantic requirements. Wire names below use camelCase.

| Field | Proposed semantics |
| --- | --- |
| `versionMode` | `current` (v2 default) or `history`. Independent of existing `queryMode`, which selects retrieval channels. |
| `recordAccess` | `false` forbids all memory/lifecycle writes, including queued work. Default true for search/direct/batch fetch under both contracts; tests and evaluation always pass false. |
| `explain` | Boolean, default false. Adds per-result rank explanation and bounded diagnostics; does not alter ranking. |
| `graphDepth` | Integer 0–3; exact maximum number of associative edges traversed, with independent work budgets. |
| `minScore` | Preserve existing meaning: threshold on fused direct relevance, not raw cosine or final composite score. Auxiliary-only candidates bypass it explicitly. |
| `timeRangeStart/End` | Existing soft creation-time boost only; never interpreted as a historical snapshot or hard time filter. Reject reversed bounds; retain existing both-bounds requirement for boosting. |

All supplied hard filters apply in every query mode, including `EmbeddingOnly`. Namespace is resolved once by A4; no implicit all-namespace search. MCP entities/topics/emotions currently compile to required tags: preserve that hard-filter behavior; entity discovery must not weaken it.

V2 response always includes `retrievalSemanticsVersion`, `rankingProfile`, `rifMode`, lifecycle version/policy revision, resolved namespace, `readGeneration` as a GenerationRef, query clock, and a `coverage` object, including compact MCP output. V1 retains its payload/text decoding with additive diagnostics where compatible. CONTRACTS defines all wire version/generation names.

`coverage` contains `candidateLimited`, `budgetExhausted`, `resultLimitApplied`, `degradedChannels`, `reasons`, and per-channel `{examined, eligible, admitted, exhausted, budgetExhausted}`. `exhausted` means the source stream ended; it does not mean its budget was consumed. Reasons are stable codes such as `fts_examined_limit`, `graph_edge_limit`, `supersession_unresolved`, and `derived_view_unavailable`.

`totalMatches` remains the number of unique eligible resolved candidates before final result truncation, not a corpus-wide hit count. A finite admitted pool cannot certify exhaustive fused retrieval even if it fills the requested limit. Report `candidateLimited=true` whenever unseen candidates remain behind a channel cap.

Invalid input is an error. Canonical metadata/read-generation failures return `unavailable`, not empty success. FTS operational failure may return vector/entity results with an explicit degraded channel; a malformed explicit FTS query is invalid input. If every requested retrieval source fails, return unavailable.

## 4. Eligibility and candidate collection

Build one request-scoped eligibility evaluator using A1's coherent read generation and A3's query-time lifecycle snapshot. Predicate: resolved namespace, existing canonical record, not tombstone/hard-deleted, allowed phase/ghost policy, required/excluded tags, minimum strength, and permastore flag. Apply it to direct records, graph nodes, and replacements. Never use a stale cache as an eligibility authority. Under legacy_access_v1, canonical materialized strength supplies both minimum-strength eligibility and ranking; under explicit_feedback_v2 both use A3 projection at the pinned clock/policy. This selection follows namespace lifecycle policy, not API version. RIF cannot alter the hard eligibility decision.

Keep score admission separate from hard eligibility: `minScore` follows fusion. A low direct relevance must not veto a valid inherited replacement. Ghost inclusion permits returning metadata under existing content-stripping rules; it does not authorize ghost graph propagation.

Let `L` be validated result limit and `B=min(1600,max(100,8*L))`. This retains the existing initial direct-channel budget as a default, not a measured optimum.

1. Resolve namespace and pin generation/time. Build eligible UUID membership from canonical namespace metadata; reuse existing tag/phase indexes only if A1 certifies their generation. Initial correctness fallback is a canonical scan.
2. Vector: pass eligible membership into exact flat search, before heap admission. Score eligible vectors and retain top B using the deterministic comparator below. Do not rely on the vector index's partial metadata filter alone.
3. FTS: read a stable ordered stream in pages of B; validate canonical eligibility before admitting up to B records. Stop at source exhaustion or 12,800 distinct examined rows. Expose underfill and work exhaustion. Cursor/order is score descending then UUID ascending within the pinned view; do not use shifting offsets across generations.
4. Entity: read only namespace-scoped postings, canonical-filter all matching UUIDs, rank by shared-count descending then UUID ascending, admit up to `2*L`. Rejected records do not consume this allowance.
5. Metadata-only: scan eligible records through completion and maintain a top-B heap by the actual baseline metadata score plus temporal boost. Remove the current scan-order `take(4*L)`. Report candidate limiting if more than B qualify.
6. Union by UUID, retaining all channel provenance; fuse, apply direct relevance admission, and compute baseline scores. Expand graph only from eligible admitted seeds. Resolve versions before final sorting/limit.

FTS's bounded refill is a deliberate incomplete fallback; it must never claim no eligible matches exist after exhausting work. Vector, entity, and metadata scans initially have no hidden early scan cap. A5 measures their cost before replacing them with bounded or indexed alternatives.

Supersession collapse can underfill a bounded union. Initially report this explicitly rather than repeatedly restarting all sources; no promise to fill L when unseen candidates remain. A later refill optimization must preserve these contracts and report its own limits.

A1 supplies a consistent generation and shared readiness barrier. Canonical point reads are allowed after pending effects replay; an unavailable search view yields an explicit channel failure, not a mandatory canonical FTS fallback. Ready independent channels may serve labeled degraded coverage at the same generation; all requested sources unavailable means unavailable. Canonical eligibility scans do not replace an unavailable scorer. Until coherent views exist, use read exclusion or fail unavailable; A2 adds no independent generation counter.

## 5. Namespace entity indexing

Change `EntityIndex` postings from `entity -> UUID set` to `(NamespaceId, normalizedEntity) -> UUID set`. Add namespace to add/remove/find signatures; use the existing `canonicalize_entity` function unchanged in the correctness release.

Deduplicate postings and UUIDs before counting/admission. Preserve current canonical-form overlap scoring initially; changing alias-count weighting is ranking tuning. Keep UUID tie ordering. Track distinct indexed UUID count idempotently rather than incrementing for repeated adds.

Update startup rebuild, HTTP single/batch insertion and removal, MCP ingestion/cleanup, benchmark fixture ingestion, and autolinking. Autolinking must query the new memory's namespace before its own caps. A memory namespace is immutable; moving content creates a new UUID with separately published insertion/removal.

Index is derived and rebuilt from canonical namespace IDs; no canonical data migration. Rebuild into a new view, publish at its generation, then retire the old view. During rebuilding use readiness failure for the entity channel until its view is published; never silently omit committed writes.

## 6. Supersession and current/history semantics

Replace `GraphReader::superseded_by -> Option<UUID>` with a typed outcome: `Current`, `Resolved {terminal, path}`, or `Unresolved {reason}`. Reasons include missing target, foreign namespace, branch, cycle, and hop/work limit. Inspect incoming Supersedes edges (new → old); do not take the first arbitrary edge.

Default chain ceiling: 64 edges per origin and 12,800 edge examinations per query, independent of associative graph depth. Reaching a ceiling without proving a terminal is unresolved. More than one successor is unresolved rather than arbitrarily selecting by timestamp. A1 atomically rejects new cycles, branching and cross-namespace edge writes; A4 maps the conflicts; legacy graph handling remains necessary.

For each eligible origin that passes score admission, compute its score once with the selected ranking profile, including its own temporal boost. Group all successfully resolved origins by terminal UUID. Load and hard-filter every terminal, even if already present.

For a terminal t, set `finalScore(t)=max(ownScore(t), max(originScore(p)))` over admitted predecessors resolving to t. Missing own evidence contributes no own score. Use the same merge for injected and existing candidates, deduplicate to one UUID, and never sum inherited scores or boost the inherited score again. An independently found weaker terminal cannot reduce its inherited floor; stronger independent evidence may increase it.

In explanations, keep own raw vector/FTS scores separate from inherited score and contributing predecessor IDs. Never relabel predecessor cosine as the replacement's cosine. A terminal failing its own direct minScore can still qualify via an admitted predecessor, but cannot bypass hard filters.

`current`: return only proven current terminals. If a known successor fails hard filters or is unavailable, omit the stale predecessor; do not leak the successor or silently label the predecessor current. Report aggregate unresolved counts/reasons. A chain passing through another namespace is invalid, even if it later returns to the requested namespace.

Content-free tombstone identities may be intermediate Supersedes-chain nodes, with same-namespace validation; never return them, seed activation from them, inherit their score, or expose ineligible IDs. Hard-deleted structure is absent; current status means proven against retained structure, not a reconstructed lost history.

`history`: return independently eligible stored versions without replacement or inherited scores. Include version status where resolvable. This means available stored history, not reconstruction of erased content and not “as known at timestamp.” Historical mode does not bypass tombstones, namespaces, or other hard filters.

**Integrated compatibility decision:** current source keeps originals when replacements are unusable. V2 intentionally changes that fallback to omission with diagnostics. Stage strict-current semantics behind explicit v2 opt-in until A4 publishes migration notes; the inheritance fix can ship earlier in legacy semantics. No `asOf` API ships until A1 defines version/valid-time storage and A3 defines retention; reject unsupported `asOf` rather than interpreting boost bounds as one.

## 7. Actual graph hops

Add `max_hops` to traversal config and hop count to traversal state/results. Seeds are depth 0; only states with depth < max_hops may expand. A depth-1 request can return immediate neighbors only, regardless of activation strength.

Use synchronous breadth-first layers so longer paths cannot consume a shorter path's remaining hop allowance. Within each layer process nodes and edges in UUID/type order and accumulate contributions in that stable order. Deduplicate each directed edge contribution per layer; prevent repeated seed injection. Track `(UUID, depth)` states, bounded by three layers, and expose minimum contributing depth for each result.

Retain existing edge factors, fan attenuation, strength weighting, and threshold constants from default SpreadingActivationConfig. Use fixed budgets of 400 expanded states and 12,800 examined edges for all nonzero depths; remove depth-dependent threshold and hop-decay changes. These budgets are provisional work bounds, not performance claims.

Only hard-eligible live nodes may seed, receive, or relay activation; ghosts and tombstones never relay in v2. Contradicts continues to block spreading. Namespace and filter rejection happen before candidate admission; examined-edge work still counts rejected edges and is reported. Budget stopping reports incompleteness.

Graph expansion uses eligible direct seeds after preliminary current-version resolution, preventing known stale seeds in current mode; repeat version resolution over the expanded union before final ranking. Supersedes traversal is separate from associative hops. Graph-discovered versions retain hop provenance even when replaced.

**Integrated compatibility decision:** v2 removes associative tombstone/ghost/filtered-node relay in both current and history modes. Content-free Supersedes-chain inspection above is structural version resolution, not activation relay. V1 retains explicitly labeled legacy behavior; no removed payload is exposed in either mode.

## 8. RIF experimental boundary

Default v2 `rifMode=off`; `shadow` computes diagnostics without changing returned scores; `experimental` is an operator/harness option, not a caller-selected production default. Legacy query-local RIF remains in contract v1 according to existing configuration and in explicitly named baseline comparators. A3 lifecycle policy does not override this API/retrieval-version choice. A2 never persists suppression or calls stability-update storage from a query.

For the experimental adapter, use preliminary top-L resolved candidates as retrieval anchors, selected with RIF off. Build actual adjacent `(anchor,target)` pairs; deduplicate pairs before caps, discard self-pairs, and require both records to satisfy hard eligibility. Multiple edge types choose the largest computed activation, with edge-type ordering as tie-breaker; do not double-count parallel edges.

Load actual target stability/retrievability and available embedding similarity; no fabricated stability 1.0. Keep this variant one-hop. Sort by activation descending then anchor/target UUID. Admit at most 100 unique targets per anchor and 1,000 pairs per query, recording cap hits. Multiple anchors may affect one target, but each pair contributes once.

Reuse one `QueryRifContext` across the query: cumulative multiplier for any target must remain in [0.75,1.0], i.e. at most 25% reduction. Suppression only; no enhancement in this variant. Apply to effective retrievability, preserving baseline channel formulas; explicitly report channels whose final score is unchanged. Freeze anchors before suppression and do not iterate to a ranking fixed point.

Gate: A6 compares off, legacy comparator, and deduplicated/capped variants on frozen isolated episodes with `recordAccess=false`; A5 measures actual Rust latency. Require all correctness tests, positive paired nDCG@10 change with 95% interval above zero, no >1 percentage-point Recall@10 loss in any prespecified channel/current/history stratum, and p95 engine latency ≤1.10× off. Insufficient samples or missing strata means gate not passed. Thresholds are proposed acceptance policy, not observed results.

Persistent RIF requires a separate A3 lifecycle proposal and longitudinal retention/usefulness evidence; passing this ranking gate does not authorize it.

## 9. Determinism, explanations, and channel metrics

Use descending finite score then ascending UUID bytes at every bounded heap, sort, page boundary, and final merge. Reject nonfinite query inputs; a corrupt/nonfinite source score produces a reported channel error, not arbitrary ordering. Normalize signed zero. Stabilize graph accumulation order; reproducibility means identical snapshot/config/embedding/time and build target, not bit equality across all SIMD architectures.

With `explain=true`, return per result: channel set, own raw vector/FTS scores, fused relevance, minScore disposition, entity overlap/recall, raw/effective retrievability, graph activation/depth, base score, temporal factor, inherited floor and source, RIF multiplier, final score, version status, profile/version. Include read generation and query clock once per response. Bound predecessor/path details to 64 IDs per result and label omitted counts. Never expose foreign/ineligible IDs through diagnostics.

Keep explanations absent by default; coverage remains present. Explanations must reconstruct final scores within floating-point tolerance, including `max` inheritance rather than pretending ranking is one additive formula.

A5 instrumentation: per-channel examined/eligible/admitted/returned counts; exclusive and overlapping channel membership; rejection counts by reason; refill rounds; candidate/edge/chain cap hits; unavailable sources; resolved/injected/already-present replacement counts; RIF unique pairs/targets/cap hits; stage timings. No raw queries, entities, UUIDs, or namespace names as metric labels.

A6 offline metrics: Recall@k, nDCG@k, MRR, fill, candidate recall against exhaustive exact oracle, current-version correctness, historical-version correctness, and per-channel rank changes. Count source provenance non-exclusively and report overlaps rather than summing channel counts as unique results.

Keep fusion (`0.90*relevance + 0.05*entity + 0.05*FTS signal` for direct matches), FTS saturation constants, auxiliary formulas, and temporal Gaussian unchanged initially. Weight calibration, fan attenuation changes, channel-uniform thresholds, and other ranking tuning each require an isolated ablation against the corrected baseline and the same quality/latency gate. Do not bundle them with correctness fixes.

## 10. Concrete implementation touchpoints and dependencies

| Owner/files | Required change |
| --- | --- |
| A2 `src/search/pipeline.rs`, `query.rs`, `response.rs` | Shared eligibility, candidate provenance/budgets, version resolution, score merging, deterministic ranking, coverage/explain structures, access suppression plumbing. |
| A2 `src/search/adapters.rs`, `index.rs`, `fts.rs`, `entity_index.rs`, `mod.rs` | Eligible-ID exact scan, stable FTS paging, namespace postings, typed source outcomes and generation-aware adapters. |
| A2 `src/graph/activation.rs`, `structure.rs`, `autolink.rs` | Layered bounded traversal, typed chain resolution, namespace-scoped autolink queries. |
| A2 `src/rif/engine.rs`, `config.rs`, search adapter | Pair deduplication, true inputs, shared query cap and off/shadow/experimental routing; no persistent mutation. |
| A1 `src/system.rs`, storage/index publication, `index_cleanup.rs` | Consistent snapshots/read generations, canonical point-read fallback, search readiness/unavailable, rebuild/publication and namespace-aware cleanup. A2 supplies derived-index signatures; A1 owns transaction/schema choices. |
| A4 `src/mcp/{args,tools,bridge,bridge_adapters}.rs`, `src/api/{models,state,handlers,adapters}.rs`, daemon protocol | Transport fields/defaults, compact coverage, structured errors, all insertion/removal call-site signatures. |
| A3 lifecycle/time/access recorder | Query clock and strength semantics, recordAccess guarantee, observation/feedback separation. |
| A5 instrumentation; A6 `src/bench/locomo.rs`, `src/test_support.rs`, isolated fixtures | Metrics/oracle and deterministic conformance tests; update benchmark entity-index calls without creating an alternate retrieval contract. |

Order: implement agreed A1/A3/A4 interfaces → implement eligibility/entity publication and inheritance fix → version modes/hops/coverage/explain → A6 end-to-end conformance → A5 measurement → optional ranking/RIF gates. Storage backend replacement is not a prerequisite; coherent reads and derived-view readiness are.

## 11. Acceptance tests

All fixtures use synthetic UUIDs/content, explicit namespaces, deterministic vectors, frozen clock, and isolated stores. These tests are required future work, not tests run by this author.

1. Preserve the actual failing supersession fixture as a regression: injected and already-present weak replacement both inherit 0.9 and rank first; assert one replacement and no predecessor. Add multiple predecessors, stronger own evidence, permutation, and minScore cases.
2. Put >1,600 disallowed high-cosine records ahead of eligible records; exact prefilter returns the eligible top-k. Compare UUIDs/scores to exhaustive eligible oracle, not merely nonempty results.
3. FTS selective-filter pages refill beyond initial B; exhaustion and the 12,800 boundary yield distinct coverage. Duplicates and rejected records do not consume eligible allowance; concurrent writes cannot shift a pinned cursor.
4. Flood foreign namespaces with the same entity; target results/candidate counts remain unchanged. Test idempotent add/remove, namespace immutability, rebuild, and autolink isolation.
5. Metadata-only scan with ghosts, tombstones, and reversed insertion order returns identical top-k; high-ranked eligible records beyond the old 4L cutoff survive.
6. Every hard filter applies to all channels/replacements and EmbeddingOnly. Cover MCP entity/topic/emotion tag translation and minimum-strength boundary at the injected query clock.
7. Chain fixtures cover 0/1/64/65 edges, missing targets, cycles, branches, foreign intermediates, and filtered terminal. Current never falsely returns a stale record as current; history returns eligible versions without inherited scores. Time boosts do not imply as-of filtering.
8. Strong A–B–C–D graph: depth 0/1/2/3 respects edge distance. Add diamond paths, cycles, filtered seeds/relays, zero-strength tombstones, conflicting edges, and budgets; assert determinism and truncation rather than only result count.
9. RIF duplicated neighbor lists and parallel edges leave multipliers unchanged; many anchors never exceed 25% cumulative suppression. Verify pair/target caps, off/shadow rank identity, and no storage writes in any mode with recordAccess=false.
10. Repeat/permutate equal-score fixtures across rebuilds and all channel heaps; UUID tie ordering survives final truncation. Explain recomputation matches final score; explain toggling changes no ranks or access behavior.
11. Inject FTS failure, stale derived generation, missing canonical record, and nonfinite score. Assert explicit failure/degradation; no false exhaustive empty success. Immediate read after committed write obeys A1's barrier/fallback.
12. Compare canonical records, access histories, pending lifecycle queues, and graph state before/after repeated recordAccess=false queries; no changes after draining queued work. Run HTTP/MCP/daemon contract fixtures in addition to pipeline unit tests.

## 12. Rollout, rollback, and empirical gates

First land the supersession inheritance regression/fix and namespace/eligibility guarantees with baseline ranking. Build new entity views from canonical data; verify generation/count parity before publication. Release additive diagnostics, then expose v2 current/history/hop semantics as explicit opt-in. A6 runs isolated correctness/oracle conformance before A4 makes v2 the default.

Canary by explicit configuration, using recordAccess=false for shadow comparisons. Promote only with zero eligibility/version/hop invariant violations and passing transport/readiness tests; A5 reports latency and candidate coverage before default rollout. A measured regression pauses promotion and triggers optimization, not removal of filters or silent smaller budgets. Ranking changes remain separately gated.

Rollback ranking to baseline-v1 and RIF off without reverting correctness. Rebuild/discard derived entity views from canonical records if publication fails. Do not restore an old index that can silently miss newer committed records. If strict-current/hop compatibility fails, stop v2 promotion and retain explicitly labeled legacy semantics for existing callers; do not claim v2 guarantees on the legacy path. No canonical data rollback is required by A2.

CONTRACTS settles snapshot/readiness, branch-write enforcement, recordAccess=true compatibility, policy-specific strength, opt-in v2, strict-current omission and no v2 associative tombstone relay. A2 budgets there are authoritative; no A5 hidden union/node cap applies. Remaining gates are actual transport/oracle conformance, A5 full-pipeline budget/latency measurements, and A6 quality/sample sufficiency. Ranking/RIF gate measurements use the A6 grouped protocol and lifecycle-size minimum (100 groups, 20 events each), with at least 20 independent groups in each prespecified channel/current/history stratum; insufficient coverage leaves RIF off. No design decision remains assigned to a later integrator.

# Retrieval exploration evidence

Date: 2026-11-13. Source baseline: `5aa4692c7d326d2ef10712939eb01484a520e891` plus the working tree inspected in this session.

Scope: read-only source exploration of ranking, filters, supersession, decay, reinforcement, graph expansion, and RIF. This evidence file is the sole authorized write. No experiments, tests, production edits, external research, or recalld MCP calls were performed. The supplied repository instructions were considered; no on-disk AGENTS.md was found in the checked ancestor paths or under docs/src. The explicit prohibition on personal-memory access governs this task.

“Source-proven” below means directly established by the inspected implementation, not validated against a running service. Failure hypotheses describe possible consequences whose incidence and severity remain unmeasured. Priorities indicate investigation order.

## Prioritized findings

### P1: RIF integration differs from the stability-update engine contract

**Source-proven behavior:** Search calls RIF on all filtered candidates before result truncation. It concatenates neighbor IDs without deduplication. The adapter iterates that combined list for every retrieved candidate, supplies stability 1.0 and no embedding similarity, and converts only multipliers below 1.0 into suppressions. The pipeline multiplies candidate effective retrievability in memory; this path does not persist the engine's stability updates. The adapter does not use QueryRifContext's cumulative reduction cap. Direct vector/FTS composite scores do not depend on effective retrievability; entity-only and metadata scores do.

Evidence: [pipeline RIF](../../../../src/search/pipeline.rs#L683), [RIF adapter](../../../../src/search/adapters.rs#L309), [composite scoring](../../../../src/search/pipeline.rs#L1145), [engine and cap contract](../../../../src/rif/engine.rs).

**Untested failure hypotheses:** Shared neighbors can receive repeated suppression and consume the engine's neighbor budget multiple times. RIF can change diagnostics without changing direct-match ranking, while disproportionately affecting entity/metadata candidates. Evaluation counters need not represent persistent forgetting. These are consequences to measure, not evidence of observed retrieval failures.

### P1: Supersession score inheritance depends on candidate discovery

**Source-proven behavior:** Newly injected replacements inherit the best predecessor composite score. Replacements already present in the candidate pool are marked eligible but do not receive that inherited score; their predecessors are still removed. Replacement validation applies namespace, tombstone, ghost, metadata, and tag gates. Missing or unusable replacements leave originals available. Injected replacements have no relevance score and therefore bypass min_score intentionally.

Evidence: [resolution](../../../../src/search/pipeline.rs#L732), especially inherited-score collection and existing-candidate eligibility at lines 777–803, injection at 823–845, and commit at 849–865.

**Untested failure hypothesis:** Independently discovering a low-scoring correction can make it fall below the cutoff where injecting the same correction would preserve its predecessor's high position.

**Additional source-proven boundary:** Graph supersession walks the first incoming Supersedes edge, detects visited-node cycles, and stops after ten hops. At the ceiling it returns the reached node without proving it is terminal. Branches have no explicit newest-version selection policy in this function.

Evidence: [chain traversal](../../../../src/graph/structure.rs#L530).

### P1: Candidate caps precede important eligibility filters

**Source-proven behavior:** Vector/FTS retrieval uses bounded pools, with fetch_k = max(limit × 8, 100), capped at 1600, before metadata filtering. Entity recall admits at most limit × 2 new candidates before namespace filtering; its shared index adapter does not scope the lookup by namespace. Metadata-only scanning takes limit × 4 records after basic metadata filtering but before final ghost filtering and ranking.

Evidence: [candidate collection](../../../../src/search/pipeline.rs#L376), [entity namespace behavior](../../../../src/search/adapters.rs#L486), [metadata scan](../../../../src/search/pipeline.rs#L1033), [final filters](../../../../src/search/pipeline.rs#L1098).

**Untested failure hypotheses:** Selective filters can omit eligible records beyond the initial pools. Foreign-namespace entity matches can consume the entity allowance. Metadata-only results can depend on scan order and underfill when ghosts consume the preliminary cap. Namespace gates prevent foreign records from being returned; the concern is candidate starvation, not demonstrated disclosure.

### P2: Recall refreshes the decay clock without FSRS stability growth

**Source-proven behavior:** Returned search results asynchronously record access after truncation. Graph-discovered results use AssociativeRetrieval; other results use DirectRetrieval. Storage access recording updates last_accessed_at and access history, without invoking FsrsEngine::apply_access or growing stability. Search consumes precomputed strength/decay_strength rather than recalculating elapsed-time retrievability. Sweeps calculate raw retrievability from last_accessed_at and stability, then add connection bonus. Explicit reinforcement separately calls review_stability using a default DecayConfig, preserves the existing phase, resets strength to 1.0, and retains existing permastore membership even when stability falls.

Evidence: [access classification](../../../../src/search/pipeline.rs#L1210), [storage recorder](../../../../src/search/adapters.rs#L379), [access mutation](../../../../src/storage/metadata.rs#L276), [cached retrievability](../../../../src/search/pipeline.rs#L1135), [sweep calculation](../../../../src/decay/sweep.rs#L903), [explicit reinforcement](../../../../src/mcp/bridge_adapters.rs#L1372).

**Untested failure hypotheses:** Repeated exposure can preserve a memory by restarting elapsed time without demonstrating usefulness. Recall immediately followed by reinforcement can shrink the spacing signal used for stability growth. Search strength filters and diagnostics can lag the clock between state refreshes. Default reinforcement configuration may diverge from intended configured policy; configuration impact requires separate verification.

### P2: graph_depth selects traversal aggressiveness, not a hop ceiling

**Source-proven behavior:** Depth zero disables expansion. Values one through three select budgets/thresholds, with depth three also changing hop decay. Traversal tracks fired nodes and a processing budget, but no hop counter. Default fan attenuation is zero at degree seven or higher. Expansion occurs before final query filtering. Ghost nodes neither receive nor fire activation; tombstones are permitted by phase gates, but the spread equation still multiplies by neighbor strength. Contradicts edges block spreading and have a strong RIF edge factor.

Evidence: [depth adapter](../../../../src/search/adapters.rs#L447), [expansion ordering](../../../../src/search/pipeline.rs#L482), [configuration](../../../../src/graph/activation.rs#L85), [traversal and strength gates](../../../../src/graph/activation.rs#L471), [RIF edge factors](../../../../src/rif/activation.rs#L70).

**Untested failure hypotheses:** A depth-one query can reach beyond one hop; comparisons across depth values confound several parameters. Ineligible seeds can introduce eligible neighbors. Useful hubs can become propagation dead ends. Tombstone relay intent may be defeated when a tombstone has zero strength; phase permission alone does not prove effective relay behavior.

### P2: min_score and ranking signals differ by retrieval channel

**Source-proven behavior:** min_score checks fused relevance rather than raw cosine. Fusion adds a saturating FTS boost capped at 0.05 to vector scores; FTS-only relevance is capped at 0.30. Entity-only, graph-expanded, and injected replacement candidates lack relevance scores and bypass min_score. Main direct-match ranking is 0.90 × fused relevance + 0.05 × entity overlap + 0.05 × FTS signal. FTS errors are converted to empty keyword results. Temporal bounds boost scores using a Gaussian; they do not filter records by creation date.

Evidence: [FTS fallback and fusion](../../../../src/search/pipeline.rs#L583), [score threshold](../../../../src/search/pipeline.rs#L1125), [composite scoring](../../../../src/search/pipeline.rs#L1145), [temporal boost](../../../../src/search/pipeline.rs#L711).

**Untested failure hypotheses:** Cosine-threshold expectations can disagree with returned mixtures of channels. Silent FTS failures can resemble weak lexical relevance. Whether the FTS score scale makes the boost useful requires measured score distributions.

## Proposed ablations — not executed

1. **RIF integration:** off versus current versus deduplicated/capped suppression. Measure rank changes by candidate origin, cumulative multipliers, and whether any persistent state changes are intended.
2. **Supersession discovery:** hold an old/new pair fixed while the replacement is absent versus independently present in the candidate pool. Compare final score and cutoff survival; separately inspect branching and chains longer than ten edges.
3. **Filter placement:** current caps versus eligibility filtering before caps. Measure eligible recall, result fill, and sensitivity to foreign-namespace entity volume and metadata scan order.
4. **Access/decay:** frozen access timestamps versus current recall refresh versus explicit reinforcement only. Track stability, strength, phase, and retention over controlled time; separate direct from associative access.
5. **Graph expansion:** off versus current traversal, then vary budget, fan attenuation, and an explicit hop ceiling independently. Include hubs, filtered seeds, ghosts, and zero-strength tombstones.
6. **Retrieval channels:** vector-only, FTS-only, entity-only, and combined retrieval. Stratify quality, raw score distributions, threshold acceptance, and final ranks by discovery channel; include temporal boost off/on separately.

No ablation results or observed production failure rates are claimed.

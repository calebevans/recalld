# Recalld: correctness first, measurable memory utility next

Research baseline: `5aa4692c7d326d2ef10712939eb01484a520e891`, 2026-11-13 local time. This package contains research and proposed implementation work, not a shipped refactor.

## Recommendation

Keep the public memory model and repair correctness before training a policy around it. The most valuable improvements are reliable state transitions, eligible retrieval candidates, explicit feedback semantics, and truthful tool contracts. A simpler canonical storage layer is worth a controlled comparison; a complete rewrite is not yet justified by measurements. Keep exact search as an oracle. Introduce ANN or RL only when a reproducible evaluation identifies a problem they solve better than simpler alternatives.

Six implementation workstreams below are the action items. Each has a dedicated technical specification and a completed [cross-spec integration review](specs/INTEGRATION-REVIEW.md) recording settled contracts and future empirical release gates. The expected implementation sequence is containment and instrumentation, contract/lifecycle alignment, retrieval fixes, then measured backend/index/policy experiments. Future agents should start with [shared contracts](specs/CONTRACTS.md), not implement specifications independently without their dependencies.

## Preparation and evidence quality

GitHub was switched and verified as personal user `calebevans`. Personal recalld was backed up while its daemon was stopped, using both its native archive and a complete data/config archive. The daemon was restarted. Private archives and SHA-256 manifest are at `/Users/caleb.evans/recalld-backups/20261114T051705Z/`; they are deliberately outside the repository. Archive decompression/checksums were checked. A complete application restore drill of the personal data was not performed.

Three Sol agents inspected storage, retrieval and tools. Separate agents ran sandbox experiments. No personal corpus was used for experiments; Docker test execution has no network and no personal directories, credentials or daemon sockets mounted. Production source is unchanged. One delegated testing branch was automatically blocked and closed; subsequent ordinary unit testing continued separately. That interruption is not a test result.

Evidence categories used throughout:

- **Source finding:** behavior traced in this revision, with references. Reachability and failure frequency may still be untested.
- **Engine test:** actual Rust code exercised in an isolated container; scope depends on the test.
- **Synthetic model:** executable reproduction of a narrow algorithm or transaction pattern. It does not establish full-engine performance or natural-language retrieval quality.
- **External evidence:** verified primary-source descriptions. Published results are not recalld results.
- **Proposal:** a design or measurement gate, not a completed implementation.

## Findings that drive the plan

| Finding | Evidence | Consequence | Owner |
| --- | --- | --- | --- |
| Text compaction commits new offsets before a separate recovery marker | [Storage exploration](evidence/exploration-storage.md), `storage/engine.rs` and `storage/text.rs` | An interruption in that interval can leave recovery choosing old text with new offsets; repair the commit protocol | [A1](specs/A1-storage-recovery.md) |
| Storage hard deletion can free a slot recorded on an already tombstoned record | Same source review | Reuse can make that slot belong to another memory; enforce ownership and terminal-state rules | [A1](specs/A1-storage-recovery.md) |
| Restore accepts manifests without enforcing complete canonical files and does not acquire target engine ownership | Same source review | Success can conceal incomplete restoration or competing writers | [A1](specs/A1-storage-recovery.md) |
| Replacement score inheritance applies only when a superseding memory is newly injected | [Retrieval exploration](evidence/exploration-retrieval.md), synthetic ablation and [actual Rust stage probe](experiments/baseline/REPORT.md) | Independently discovering the correction can perversely lower its rank | [A2](specs/A2-retrieval.md) |
| Important filters run after capped candidates; entity collection is capped before namespace eligibility | Same review plus prefilter model | Correct memories can be omitted and pages underfilled despite eligible records existing | [A2](specs/A2-retrieval.md) |
| RIF wiring differs from the standalone engine and ranking branches use it differently | Same review plus RIF model | Complexity has no demonstrated benefit and can cause inconsistent behavior across channels | A2/A3 |
| Retrieval updates access timestamps, while explicit reinforcement uses a separate stability update | Source review | Repeated evaluation is not cleanly observational; retrieval frequency can substitute for usefulness | [A3](specs/A3-lifecycle.md) |
| Malformed quality values default to positive quality 3; namespace dimensions narrow without a range check | [Tool exploration](evidence/exploration-tools.md) | Invalid input can cause a valid-looking mutation | [A4](specs/A4-tools-namespaces.md) |
| Tool side-effect/idempotency hints, session defaults and duplicate-scan coverage disagree with behavior | Same review | Agents and retrying harnesses can make incorrect assumptions | [A4](specs/A4-tools-namespaces.md) |
| Benchmark ingestion/retrieval bypass public MCP contracts | Same review and [external evaluation audit](evidence/external-memory-research.md) | Existing QA results do not establish agent integration correctness | [A6](specs/A6-evaluation-harness.md) |

The source findings are narrow enough to implement regression tests. They do not establish observed corruption in the personal database. Storage namespace mismatch and phase-index interleaving remain internal contract risks unless an externally reachable path is demonstrated.

## Experiments and what they establish

See [sandbox ablations](experiments/ablations/) for scripts, exact container command, pinned image, raw JSON, source hashes and repeatability evidence. The probes use five deterministic seeds `[7,19,41,73,101]`; they model selected formulas/control flow rather than invoking the Rust query engine.

**Candidate eligibility.** In the independent-score synthetic setting with 2,000 candidates, 1% eligible, `k=20`, and a candidate budget of 160, postfiltering achieved mean eligible-oracle Recall@20 of 0.065 across 50 query fixtures. Filtering before selection achieved 1.0 by construction. This demonstrates the candidate-budget failure mechanism, not a prediction of a 93.5-point improvement on real queries. Eligibility-mask construction cost is not included in modeled dot-product counts; actual latency must be measured.

**Supersession invariance.** Across five constructed fixtures, a replacement injected after retrieval ranked first, but the same replacement already present at a lower score ranked eleventh and missed top five. Giving existing replacements the same inherited score restored rank one. An additional actual Rust pipeline probe confirmed the defect: replacement absent scored 0.9 at rank one; already present scored 0.1 at rank eleven and missed top ten. The regression property is invariant replacement ranking regardless of discovery path, subject to identical eligibility; neither fixture is a representative query sample.

**RIF.** The model checks off, current wiring, neighbor deduplication, and deduplication plus cumulative cap. Direct-match rank can remain unchanged despite large suppression because that score path does not use the suppressed strength. This warrants simplifying and testing the integration before tuning psychological analogies. It does not establish that RIF improves or harms real-world QA.

**Canonical transactions.** A SQLite demonstration compares one transaction with separately committed memory/tag/edge updates under deterministic process exits. This isolates all-or-none application state. It is neither a recalld interruption test nor a backend throughput benchmark. The single-transaction variant preserved all-or-none state in 15/15 scheduled exits; separate commits left partial logical state in 6/15. All integrity checks passed. These cut points are constructed cases, not crash probabilities.

**Rust baseline and validation.** The unmodified baseline passed **487 Rust unit tests**, with four doctests ignored. Six added handler validation tests produced three passes and three failures: wrong-type reinforcement, out-of-range namespace dimensions, and wrong-type namespace dimensions. Tests use isolated synthetic storage and the actual Rust handlers. A seventh added test confirmed the supersession defect and failed as expected. Final logs and test additions are in [baseline experiments](experiments/baseline/); a passing baseline does not establish recovery correctness.

**Actual exact-index microbenchmark.** A release-mode `FlatVectorIndex` run used 384-dimensional precomputed synthetic vectors, top ten, 20 warmups and 100 measured queries per condition on Docker ARM64 with Rust 1.100.0. At 1,000 vectors, unfiltered p50/p95 were 22.458/23.042 microseconds; at 10,000 they were 223.752/232.835 microseconds. A namespace filter admitting 10% of vectors measured 3.542/4.000 and 33.042/34.584 microseconds respectively. These are one-run in-memory kernel measurements, excluding embedding, metadata, graph, locks, transport and eligibility construction. They show no need to assume the exact scan dominates the multi-second historic benchmark timings. They do not prove an end-to-end speedup or settle the ANN threshold. Raw samples and harness are in the baseline experiment package.

## Architecture decision

The current engine asks the application to coordinate several persistent resources and their in-memory projections. The compaction issue is an example of that coordination burden. My preferred hypothesis is one transactional canonical database for records, text, vectors, edges and operation receipts, with rebuildable search projections. SQLite is attractive because rusqlite/FTS5 are already present; consolidating into one redb database is a credible lower-dependency-change challenger.

[SQLite's transaction documentation](https://sqlite.org/atomiccommit.html) supports the all-or-none design; its [WAL documentation](https://sqlite.org/wal.html) explicitly limits atomicity across attached databases, so splitting canonical state across databases would recreate the problem. The [backup API](https://sqlite.org/backup.html) is a useful snapshot primitive, but does not replace application-level validation or restore ownership. These are design reasons, not measured speed claims. Full comparison and migration invariants are in [storage options](evidence/storage-options.md).

Do not migrate until representative ingestion/query/restart measurements and a full record/edge/provenance round-trip pass. Preserve UUIDs and all lifecycle state. Import into a new destination and retain the source snapshot. Do not call reopening an old database a lossless rollback after new writes have begun.

## Action items

| ID | Deliverable | Dependency / release gate |
| --- | --- | --- |
| [A1](specs/A1-storage-recovery.md) | Repair persistence and restore invariants; compare consolidated canonical storage; define safe migration | Immediate repairs first; backend migration only after parity and workload gates |
| [A2](specs/A2-retrieval.md) | Make filtering, supersession, graph depth and ranking behavior coherent | Stable A1 generations and A3 query side-effect semantics; test before tuning |
| [A3](specs/A3-lifecycle.md) | Separate retrieval from usefulness feedback; make lifecycle replayable | A1 persistence and A4 versioned public contract |
| [A4](specs/A4-tools-namespaces.md) | Strict, truthful tools and practical namespace workflows | Shared lifecycle/idempotency contracts; backward-compatible transport rollout |
| [A5](specs/A5-performance.md) | Profile and optimize actual bottlenecks under correctness constraints | Baseline instrumentation can start immediately; use A6 comparable workloads |
| [A6](specs/A6-evaluation-harness.md) | Reproducible evaluation and isolated learning environment | Contract fixtures first; RL only after reliable transitions and policy baseline |

Treat these six workstreams as the complete action list; substeps and acceptance tests live in their specifications. No separate unspecced rewrite or model-training project is implied.

## Namespaces and agent behavior

Use namespaces intentionally for durable domains such as personal context, projects and temporary task episodes, with different retention needs. Avoid creating one namespace per minor topic: it fragments retrieval and forces agents to guess where facts live. Resolve omitted namespace from session configuration and expose that resolution clearly. Namespaces are organization, not an authorization mechanism.

Start with discovery and explicit targeting. Do not silently search all namespaces. A future multi-namespace query must return provenance and use rank fusion or compatible embedding profiles; raw scores from unrelated spaces are not automatically comparable. Today all namespaces share one provider, so reject unsupported dimension combinations before writing data. Add per-namespace model routing only when the corpus/workload needs it.

Agent guidance should explain five decisions briefly: where to store, what deserves a memory, when a fact supersedes an older fact, when retrieved material actually helped, and what deletion retains. A long generic instruction block cannot compensate for contradictory tool schemas. Output schemas, deterministic errors and coverage metadata reduce token cost and ambiguity more reliably than additional prose alone.

## Evaluation and RL path

The existing LoCoMo run combines extraction, query planning, retrieval, graph context, answer generation and judging, with decay disabled. Retain it as an end-to-end regression track, and add independent retrieval diagnostics and chronological lifecycle episodes. Match answer-context token budgets, not merely top-k counts. Keep official scoring separate from custom judge accuracy.

The [primary-source research](evidence/external-memory-research.md) recommends cleaned LongMemEval for updates/temporal/abstention coverage and small BEIR corpora for retrieval diagnostics. Preserve source-turn provenance so failure can be attributed to extraction loss, candidate omission, ranking or answering. Group data splits and uncertainty by conversation/history. Public benchmark questions are not independent user environments; holdout labels must stay out of ingestion, observations and policy selection.

For learning, define `(state, action, elapsed time) -> (next state, observation, reward)` with a controlled clock, snapshot/reset and an observable completion barrier. Start with store-all, deterministic deduplication and prompted policies under equal budgets. Learn one component only if a measured policy failure remains. Future answer utility can inform reward; namespace/deletion constraints must remain hard gates, not penalties that a high answer score can outweigh. Neither the Memory-R1 paper nor this investigation establishes that training will outperform those baselines on recalld.

## Limits

No paid LLM evaluation, model training, representative large-scale backend benchmark, personal-data migration or production refactor was performed. Synthetic probes establish mechanisms, not prevalence. Source inspection identifies recovery risks but does not reproduce every interruption/interleaving. The final specifications make these missing measurements explicit release gates rather than assuming them away.

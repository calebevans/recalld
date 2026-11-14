---
type: experiment
status: done
project: "[[recalld]]"
started: 2026-11-13
ended: 2026-11-13
---

# Retrieval ablations and SQLite atomicity PoC

These are executable, source-informed **Python models, not full-engine runs**. They isolate candidate starvation, supersession score inheritance, and query-time RIF rank sensitivity. The SQLite experiment executes real SQLite transactions and abrupt child-process exits against synthetic tables; it does not reproduce a recalld crash. No production edits, recalld MCP calls, personal memory access, network requests, model calls, or new dependencies were used.

## Artifacts and reproduction

- [probes.py](probes.py): deterministic generators, models, assertions, SQLite child writer, benchmark audit.
- [run.sh](run.sh): exact sandbox invocation and streamed input allowlist.
- [results.json](results.json): individual observations, synthetic IDs/scores/ranks, seed summaries, runtime versions, source and benchmark SHA-256 hashes, and audit results.
- [image-inspect.json](image-inspect.json): full locally installed image metadata.
- [source-commit.txt](source-commit.txt), [source-working-tree.diff](source-working-tree.diff): source identity and empty tracked-source diff at execution.
- [repeatability.txt](repeatability.txt): raw JSON hash from the first of two identical final runs.

From the repository root, the exact final execution and verification commands were:

```sh
bash docs/research/2026-11-recalld/experiments/ablations/run.sh
shasum -a 256 docs/research/2026-11-recalld/experiments/ablations/results.json > docs/research/2026-11-recalld/experiments/ablations/repeatability.txt
bash docs/research/2026-11-recalld/experiments/ablations/run.sh
shasum -a 256 -c docs/research/2026-11-recalld/experiments/ablations/repeatability.txt
```

Verification returned `results.json: OK`. Both final runs passed all executable assertions and produced byte-identical JSON. Hash: `c15f91c6c809c3e003e57b0016ac26a3cc3e4e76c727992d7e83b4bce200556d`.

Source commit: `5aa4692c7d326d2ef10712939eb01484a520e891`. Installed image tag: `python:3.12-bookworm`. Pinned Docker image ID and repository digest: `sha256:4209fae09a3e20afd863e528bfddc235da5fd5977983e1996cd54cc9a72e6267`. The inspected descriptor is an OCI image index; the observed runtime is Linux aarch64, Python 3.12.16, SQLite 3.40.1. The runner uses `--pull never` and never builds or downloads an image.

Docker runs with `--network none`, read-only root filesystem, no host mounts, all capabilities dropped, no-new-privileges, one CPU, 512 MiB memory, 64 PID limit, and 128 MiB tmpfs at `/tmp`. Only allowlisted source, repository benchmark results, and the probe script are streamed over stdin and extracted to disposable `/tmp`. Output is returned over stdout into this directory. Docker daemon access required sandbox elevation; the Docker socket is not mounted into the container. The runner overwrites its generated artifacts when rerun.

## Source mapping and model boundaries

| Mechanism | Read-only source evidence | What is modeled |
|---|---|---|
| Candidate limit | `src/search/pipeline.rs:376`, `src/search/query.rs:133` | `fetch_k = min(max(limit * 8, 100), 1600)` for non-metadata queries. At limit 20 this is 160. |
| Filters | `src/search/adapters.rs:113`, `src/search/index.rs:309`, `src/search/pipeline.rs:491` | Adapter passes namespace only to vector prefilter; metadata filtering follows candidate loading. Index can prefilter tags/phases before dot products. |
| Supersession | `src/search/pipeline.rs:748` through the end of `resolve_supersedes` | Plan, validate, remove, inject; inherited maximum is assigned to newly loaded replacements, but existing candidates retain their own composite scores. Resolution precedes truncation. |
| RIF call path | `src/search/pipeline.rs:684`, `src/search/adapters.rs:308`, `src/search/adapters.rs:430` | Concatenated neighbor occurrences, repeated adapter evaluations, multiplication of effective R for candidate targets. |
| RIF formula and cap | `src/rif/activation.rs`, `plasticity.rs`, `config.rs`, `engine.rs` | Default activation and plasticity formula, 100-neighbor cap. `QueryRifContext` exists but is not called by this inspected search adapter path. |
| Composite ranking | `src/search/pipeline.rs:1150` vicinity, `compute_composite_score` | Separate direct, activation, entity-recall, and metadata branches. |
| Benchmark serialization/timing | `src/bench/locomo.rs:372`, `:734`, `:1075`, `:1547` | Count consistency and saved aggregate timings only; no rerun of benchmark inference. |

Exact source hashes are in `results.json`. Line references are for the inspected checkout. The models use Python binary64 arithmetic rather than Rust f32; no result depends on a near tie. There is no embedding inference, SIMD kernel, FTS matching, asynchronous scheduling, cache/storage latency, graph expansion algorithm, decay evolution, or full query validation in these probes.

## Metrics and controls

Seeds: **7, 19, 41, 73, 101**. Each ablation holds its generated inputs constant across variants. Filtering uses ten queries per seed, producing 50 observations per configuration, 300 generated query/filter scenarios, and 2,400 variant rows. Supersession has 20 rows; RIF has 80. SQLite has 30 independent child-process interruption cases. Seed diversity tests robustness to synthetic jitter, not external validity or production prevalence.

`recall@k` is the fraction of designated relevant IDs in the first k results. For filtering, relevance is the exact eligible top-20 oracle; for supersession and RIF it is the single designated correction/target. These are synthetic retrieval metrics, not answer correctness. `fill@k` is returned count divided by k. `best_relevant_rank` is one-based in the complete modeled post-stage ranking, with null if absent. `reciprocal_rank_at_k` is zero when no relevant item reaches the returned window. Filtering also records mean oracle rank of returned IDs and asserts that returned hits form an exact oracle prefix. Missing results therefore hurt recall/fill, without a reranking error among survivors.

## Candidate-limit ablation

Each synthetic namespace has 2,000 records with independent continuous random similarity scores. Eligibility is a single binary tag predicate, so prefiltering it is supported by the inspected index semantics. Eligibility fractions are exactly 1%, 5%, or 20%. One layout samples eligible records independently of similarity; another samples only below global rank 200. Namespace is held fixed, minimum score does not exclude records, entities and FTS contribute no candidates, graph expansion contributes none, and no supersession edges exist. With vector-only relevance, composite rank is monotonic in vector score.

Prefilter changes only whether eligibility is applied before or after the candidate budget. Budgets 20, 400, and 1600 are counterfactual controls; **160 is the source formula at limit 20**. The 1600 case uses the source ceiling as a sensitivity test, not a claim that the source chooses it for limit 20.

| Eligible fraction / layout | Postfilter recall and fill at 160, mean [min,max] | Prefilter recall and fill at 160 |
|---|---|---|
| 1%, independent | 0.065 [0.000, 0.200] | 1.000 [1.000, 1.000] |
| 5%, independent | 0.354 [0.100, 0.650] | 1.000 [1.000, 1.000] |
| 20%, independent | 1.000 [1.000, 1.000] | 1.000 [1.000, 1.000] |
| 1%, below rank 200 | 0.000 [0.000, 0.000] | 1.000 [1.000, 1.000] |
| 5%, below rank 200 | 0.000 [0.000, 0.000] | 1.000 [1.000, 1.000] |
| 20%, below rank 200 | 0.000 [0.000, 0.000] | 1.000 [1.000, 1.000] |

All table cells have n=50. Prefiltering recovers the oracle at every tested budget, by construction of exact search with at least 20 eligible records. At 1% independent eligibility, increasing the postfilter budget from 20 to 160 to 400 to 1600 gives mean recall **0.008, 0.065, 0.201, 0.808**. Even 1600 fails to fill the oracle in every independent 1% trial (range 0.65 to 0.95). This is candidate starvation under the chosen distribution, not proof that recalld commonly suffers those rates.

At budget 160, modeled metadata loads are 160 for postfilter, versus 20/100/160 for prefilter at 1%/5%/20%. Modeled dot-product counts are 2,000 versus 20/100/400, respectively, reflecting the existing index's predicate-before-dot-product order. Both still inspect 2,000 metadata entries. These are operation counts, **not measured latency or speedup**. Python sorts generated scalar scores rather than executing Rust's heap/SIMD implementation. A real broader predicate pushdown would need matching tag semantics and metadata support; this experiment tests only one tag, not arbitrary strength/permastore predicates. Hybrid candidate sources can rescue results and are deliberately absent here.

Raw observations and per-seed aggregates are under `prefilter.rows`, `summaries.prefilter`, and `summaries.prefilter_per_seed`.

## Supersession ablation

Each seed generates one high-scoring obsolete record (0.90 to 0.95), ten fillers (0.30 to 0.80), and a lower-scoring correction (0.15 to 0.25 when already a direct candidate). A newly loaded correction has effective R=0.9 and a default composite score of 0.09 before inheritance. This follows `Candidate::new` and the metadata fallback branch. All records are eligible; there are no boosts. Only the correction's presence in the initial candidate set changes between the two source-model cases.

| Correction state / variant | Correction rank, all 5 seeds | Recall@5 | Fill@5 |
|---|---|---|---|
| Absent / source model | 1 | 1.0 | 1.0 |
| Already candidate / source model | 11 | 0.0 | 1.0 |
| Absent / inherit-existing counterfactual | 1 | 1.0 | 1.0 |
| Already candidate / inherit-existing counterfactual | 1 | 1.0 | 1.0 |

The counterfactual applies `max(existing_score, inherited_score)` to existing corrections. This isolates score inheritance as sufficient to remove the asymmetry in the fixture; it is not a production change or a judgment that inheritance is always desirable. Runnable guards also cover missing replacement, failed load, ineligibility, no duplicate correction, and two obsolete records sharing one correction with maximum-score inheritance. Ineligibility is a single boolean here; the Rust namespace/tag/phase predicates themselves are not reimplemented. Terminal supersession resolution is assumed; chains, cycles, hash iteration ordering, and mixed batch-load failure are outside this probe.

## Query-time RIF ablation

The fixture has a target connected to six leaves by one associative edge each, plus eight unrelated candidates. All 15 candidates reach the RIF stage, even though only five results will be returned. Target strength and decay strength are 0.9; leaves are 0.8; unrelated strengths vary from 0.70 to 0.85 by seed. Edge weight is 0.9, gamma=0.3, distance=1, similarity=None, and default plasticity parameters apply. The target's activation per triggering leaf is `0.9 * 0.3 * 0.9 = 0.243`.

The concatenated neighbor list contains the target six times. Each of its six retrieved leaves reprocesses all six occurrences, yielding **36 target suppressions**, below the per-source max-neighbor cap. Deduplicating the combined list yields six target suppressions, one per triggering leaf. Adding a cumulative 0.75 floor is a separate change from that deduplicated baseline. Neither counterfactual changes the ranking formula.

| RIF variant | Target events | Target cumulative multiplier | Direct / graph rank | Entity-only rank | Metadata-only rank |
|---|---|---|---|---|---|
| Off | 0 | 1.000000 | 1 / 1 | 1 | 1 |
| As wired model | 36 | 0.003557 | 1 / 1 | 9 | 15 |
| Deduplicate neighbors | 6 | 0.390700 | 1 / 1 | 9 | 15 |
| Deduplicate + cumulative cap | 6 | 0.750000 | 1 / 1 | 9 | 15 |

Ranks and multipliers have identical min/mean/max across five seeds per cell. Every variant fills five results. Direct and graph target recall@5 remain 1. Entity and metadata recall@5 change from 1 with RIF off to 0 in all suppression variants. The cap improves the target's effective R but **does not restore recall in this fixture**; unrelated candidates remain stronger.

Direct and graph scores omit effective R, so their lack of rank change is structural for this isolated stage. Entity-only and metadata-only scores include `0.1 * effective_r`. Fixed entity overlap is 0.5, entity recall is 0.5, and FTS is absent. In non-metadata fixtures, leaves are low-scoring direct candidates; the metadata-only fixture uses only metadata candidates. The generated direct/activation signals are held fixed and not recomputed from the suppressed R.

This is query-time score adjustment, not persistent stability mutation. The adapter forwards the raw multiplier for suppressive updates; the engine's stability floor is not an effective-R floor in this path. `QueryRifContext` elsewhere in the codebase does not establish that it is used here. The experiment does not assess asynchronous access-side RIF, changes on future queries, dense graphs at the neighbor cap, parallel edges, spreading activation, or namespace-specific configurations. Six leaves and a chosen sensitive score distribution establish a mechanism, not its frequency in real data.

## SQLite canonical-store atomicity PoC

One SQLite database contains `memory`, `tag_projection`, and `edge_projection` tables. Each starts empty. The synthetic operation inserts one row into each. The transaction variant encloses all three writes in `BEGIN IMMEDIATE` plus one commit. The multi-step variant commits each insert separately in the **same database**. Keeping the schema and storage engine identical isolates the transaction boundary; this is not a model of recalld's actual file layout.

Each cut point runs in a fresh child interpreter, which exits with `os._exit(73)` without closing the connection. The parent opens a new connection and verifies table counts and `PRAGMA integrity_check`. Three repetitions per mode/cut point, 30 child exits total; journal mode DELETE, synchronous FULL. Cut 3 in the transaction variant is after the third insert but before commit. Cut 4 is after commit.

| Interruption point | One transaction: rows in three tables | Separate commits: rows in three tables |
|---|---|---|
| Before first insert | [0,0,0] | [0,0,0] |
| After first insert | [0,0,0] | [1,0,0] |
| After second insert | [0,0,0] | [1,1,0] |
| After third insert | [0,0,0] | [1,1,1] |
| After final commit | [1,1,1] | [1,1,1] |

Every observation matched its expected state and every integrity check returned `ok`. The single transaction preserved logical all-or-none state in 15/15 trials. Separate commits exposed partial logical state in 6/15 trials (the first- and second-write cut points, three repeats each); this is not SQLite corruption. These scheduled cut points are not a probability distribution of real crashes.

The database lives on container tmpfs. This demonstrates process-interruption transaction atomicity, not power-loss durability, disk flush behavior, kill-during-commit recovery, multi-process contention, cross-database transactions, or crash recovery in recalld. It provides no throughput, latency, migration-cost, or speedup evidence for replacing existing storage. No timing claim is made.

## Offline benchmark audit

Only the six checked-in `benchmarks/*/bench_results.json` files were audited. No debug logs, live stores, personal memories, or external services were used. Narrative questions and answers were not exported. All six files contain 1,986 QA rows and passed checks reconciling question counts, correct counts, conversation counts, per-conversation counts, and per-category counts. File hashes, configurations, categories, and schema field names are recorded in `benchmark_audit`.

| Saved run | Correct / 1,986 | Recomputed answer accuracy | Saved mean retrieval ms |
|---|---|---|---|
| Claude Sonnet 4.6 top15 | 1,678 | 84.49% | 3,491.02 |
| Claude Sonnet 4.6 top20 | 1,672 | 84.19% | 3,508.46 |
| Gemini 2.5 Flash top15 | 1,537 | 77.39% | 1,789.31 |
| Gemini 2.5 Flash top15 stress | 1,525 | 76.79% | 1,973.98 |
| Gemini 2.5 Flash top20 | 1,556 | 78.35% | 1,893.42 |
| Gemini 2.5 Flash top20 stress | 1,555 | 78.30% | 2,470.31 |

These saved answer scores cannot identify the causal effects tested above. Question results contain question/gold/generated answer/correct/category fields, not candidate IDs, eligibility flags, per-stage ranks, or relevance judgments. Retrieval recall, fill, and rank cannot be reconstructed from these JSON files. The reported timing wraps `search_memories`, which can construct a query through an LLM; it is not an isolated vector scan measurement. Only aggregate retrieval timings are saved, so they were transcribed, not independently recomputed. The source commit and hardware that produced the historic results are not established by this audit. Differences between top-k or stress runs are not controlled ablations of prefiltering, supersession, or RIF.

## Findings and follow-up

The strongest follow-up is a Rust regression fixture for a correction that is already present with a lower score than its obsolete predecessor. A second fixture should check shared-neighbor multiplicity in the real RIF adapter and separately test a cumulative cap. Predicate pushdown deserves a controlled full-engine test using synthetic tags and all hybrid candidate sources, with stage timings and load counts; the scalar models alone cannot justify a latency claim. SQLite establishes the value of one transaction boundary for one logical write, but does not by itself justify a storage migration.

## Run log

2026-11-13: Read the requested experiment skill and the local ponytail skill. The user's explicit output directory overrides the experiment skill's usual notebook/daily-note destinations; no other notes were written. Inspected source and benchmark schemas offline. Docker access failed under the default sandbox, then succeeded with requested elevation. The initial schema-only tar extraction emitted ownership warnings because all capabilities were dropped; the final runner uses `tar --no-same-owner` and `sh -e` and completed cleanly. An initial modeled metadata case mixed direct leaves into the metadata-only fixture; this was corrected to all-metadata candidates before the final two runs. Final assertion suite passed twice with identical raw JSON. No full Rust build or real engine crash experiment was attempted.

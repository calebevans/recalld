# Paired namespace-read batching study

This dependent study preserves `impl/evaluation` at `159f987` and the original
study branch at `9a9ef83`. It measures the
unchanged internal diagnostic harness against two exact source revisions:

- Corrected baseline: `1a634006023cfab6e277a1ebe8503b5e3de4e40a`.
- Namespace-batch candidate: `0e58f3b3dd36e2d74dfd5f97bfb522ba96c6efe8`.

Only tracked Rust/Cargo inputs from those commits enter the two scratch builds.
The same `examples/offline_eval.rs` is overlaid on each, without changing its
embedding fixture or acknowledged no-write access adapter. All snapshots have
file hashes; the image ID, driver hash and study plan are frozen before runs.
Each arm has separate copied Cargo/target directories, with offline builds and
tests, 2 CPUs, 4 GiB and two build jobs. This never checks out or modifies core
files in the assigned worktree, integration worktree or original repository.

Start a study; this runs both revisions' library/harness tests, release builds,
and **stops after the 1,000-record seed-7 pair**:

```sh
python3 scripts/paired-namespace-read-study.py \
  --cache /tmp/recalld-unit-tests-l__ja1yo/cargo \
  --build-cache /path/to/recalld-evaluation-prior/target
```

After recording the pilot, continue its exact frozen inputs:

```sh
python3 scripts/paired-namespace-read-study.py \
  --continue-study /tmp/recalld-namespace-read-study-PRINTED-SUFFIX
```

Continuation rejects changed driver/harness/source inputs, keeps the pilot as
the first pair rather than duplicating it, and finishes the matrix:

| Parameter | Frozen choice |
| --- | --- |
| Total records | 1,000 and 10,000 |
| Target namespace records | 100 and 1,000, respectively; 90% foreign distractors |
| Dimensions / result limit | d384 / k20 |
| Seeds per scale | 7, 8, 9, 10, 11, explicitly requested for this study |
| Per-process, per-cell samples | 20 warmups + 100 measured |
| Cells | Exact index 100%/10% eligibility, internal engine vector/hybrid |
| Pair order | Alternate first arm by seed; reverse starting order at 10k |
| Processes | 20, including the pilot's two; 10 pairs |
| Fixture store | tmpfs, reconstructed per process; no public transport or providers |

Every pair must have identical fixture, ingestion ledger, record-state and
rank/score hashes. The unchanged harness also enforces its scalar oracle,
namespace checks, FTS activation/control assertions and unchanged stored records.
This fixture has no graph edges, no corrections, no active decay and no metadata
exclusions within the target namespace. It does not replace retrieval correctness
tests or establish quality on adversarial filtering/supersession workloads.

Primary descriptive outputs are paired per-process p50/p95 for the two engine
cells, candidate/baseline p95 ratios, and all five seed pairs separately. Exact
index timings are controls. Candidate `eligibilityUs` measures its canonical
namespace enumeration/loading/filtering stage and is retained automatically by
the harness. Stage intervals are nested, not additive percentile costs.

These are overhead characterizations with a small sample plan, not release gates:
no absolute budgets were preregistered, the host is shared, 20/100 warmup/sample
counts are below A5's release plan, and synthetic self-retrieval is not semantic
quality. No throughput, persistent-disk cost, public-API latency, instrumentation
overhead in isolation or general production benefit can be inferred. Both revisions already include corrected hard eligibility and the timing field.
The candidate changes namespace metadata loading to one read transaction and
one blocking task; no new caching or filter semantics are requested. The
original study driver is preserved byte-for-byte; this separately pinned driver
changes the two revisions and scratch prefix, retaining the same schedule.

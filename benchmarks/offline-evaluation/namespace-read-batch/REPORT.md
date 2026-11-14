# Namespace metadata batching characterization

2026-11-14. Corrected baseline `1a634006023cfab6e277a1ebe8503b5e3de4e40a`
versus namespace batch candidate `0e58f3b3dd36e2d74dfd5f97bfb522ba96c6efe8`.
See [frozen plan](README.md). Diagnostic measurements, no preregistered release budget.

At 10k, median paired engine p95 fell **16.7% vector / 19.3% hybrid**.
Hybrid improved in all five pairs; vector improved in four, regressing 9.2%
in seed10. Eligibility p95 ranges fell from 549–587 to 347–489 µs (vector)
and 566–597 to 335–373 µs (hybrid). This supports a measured benefit for
batching on this synthetic workload, while canonical namespace loading still
accounts for roughly half of engine wall time. It does not remove the namespace
size dependence or establish a general production speedup.

At 1k, median paired p95 changes were -4.8% vector / -33.3% hybrid, with large
baseline hybrid spikes and mixed vector direction. Unfiltered exact-index
controls were close to parity in median, but filtered controls were noisy
(1.415x at 1k, 1.309x at 10k). Index code is unchanged; these controls caution
against treating every observed latency change as caused by batching. No pairs
were discarded or selectively rerun.

## Pilot recorded before extending

1,000 records, 100 in the target namespace, d384, k20, seed7, baseline first.
Each of four cells had 20 warmups and 100 measured samples per process.

| Engine cell | Baseline p95 (µs) | Candidate p95 (µs) | Candidate / baseline |
| --- | ---: | ---: | ---: |
| Vector | 189.709 | 138.751 | 0.7314 (-26.9%) |
| Hybrid | 191.167 | 154.792 | 0.8097 (-19.0%) |

Both arms passed 509 library and two harness tests, followed by release builds
in offline Docker. Fixture, ledger, stored-state and result hashes match.
The Rust harness and no-write access recorder are unchanged from the prior study.
One paired seed is preliminary; proceed with the frozen five-seed/two-scale
matrix before assessing consistency. No release-gate conclusion follows.

[Pilot evidence](pilot-evidence.tgz) contains the pilot manifest, raw traces,
summaries, environment and build/test logs.

## Completed matrix

All ten pairs completed: 20 processes, 8,000 measured operations (4,000 engine and 4,000 exact-index), plus 1,600 warmups. Both arms passed 509 library + 2 harness tests. Every pair matched fixture, ledger, stored-state and rank/score hashes. No measured failures. Source/artifact hashes, raw sample counts and every stage p50/p95/p99 were independently reconciled.

Statistics describe five paired process p95 ratios, not pooled/averaged query percentiles. All pairs are retained, including baseline hybrid spikes at 1k seeds10/11 and the candidate vector regression at 10k seed10. Shared-host scheduling and contention are not controlled or attributed.

| Records | Mode | Median paired p95 ratio | Pair range |
| --- | --- | ---: | ---: |
| 1,000 | internal-engine-vector | 0.952 | 0.731–1.132 |
| 1,000 | internal-engine-hybrid | 0.667 | 0.150–1.045 |
| 1,000 | exact-index-100pct | 1.002 | 0.976–1.101 |
| 1,000 | exact-index-10pct | 1.415 | 0.761–1.626 |
| 10,000 | internal-engine-vector | 0.833 | 0.780–1.092 |
| 10,000 | internal-engine-hybrid | 0.807 | 0.734–0.868 |
| 10,000 | exact-index-100pct | 0.991 | 0.897–1.038 |
| 10,000 | exact-index-10pct | 1.309 | 0.964–1.818 |

Every engine pair, microseconds:

| Records | Seed | Mode | Baseline p50 | Candidate p50 | Baseline p95 | Candidate p95 | Ratio |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1,000 | 7 | vector | 164.792 | 88.334 | 189.709 | 138.751 | 0.731 |
| 1,000 | 8 | vector | 172.917 | 190.751 | 275.543 | 310.501 | 1.127 |
| 1,000 | 9 | vector | 155.000 | 133.792 | 220.418 | 191.501 | 0.869 |
| 1,000 | 10 | vector | 144.167 | 130.625 | 190.042 | 180.876 | 0.952 |
| 1,000 | 11 | vector | 125.417 | 115.000 | 150.833 | 170.750 | 1.132 |
| 1,000 | 7 | hybrid | 115.333 | 136.583 | 191.167 | 154.792 | 0.810 |
| 1,000 | 8 | hybrid | 229.501 | 202.209 | 315.293 | 329.376 | 1.045 |
| 1,000 | 9 | hybrid | 145.000 | 148.709 | 306.584 | 204.500 | 0.667 |
| 1,000 | 10 | hybrid | 147.750 | 118.500 | 1210.795 | 181.293 | 0.150 |
| 1,000 | 11 | hybrid | 212.292 | 107.917 | 763.377 | 170.001 | 0.223 |
| 10,000 | 7 | vector | 772.289 | 648.127 | 870.746 | 725.293 | 0.833 |
| 10,000 | 8 | vector | 870.788 | 697.914 | 975.913 | 821.579 | 0.842 |
| 10,000 | 9 | vector | 836.872 | 660.956 | 944.996 | 756.621 | 0.801 |
| 10,000 | 10 | vector | 814.830 | 696.497 | 918.746 | 1003.496 | 1.092 |
| 10,000 | 11 | vector | 843.455 | 645.581 | 940.371 | 733.247 | 0.780 |
| 10,000 | 7 | hybrid | 769.330 | 589.043 | 857.663 | 655.752 | 0.765 |
| 10,000 | 8 | hybrid | 862.830 | 663.164 | 937.538 | 756.330 | 0.807 |
| 10,000 | 9 | hybrid | 846.330 | 654.956 | 915.621 | 755.371 | 0.825 |
| 10,000 | 10 | hybrid | 816.954 | 665.123 | 928.537 | 806.288 | 0.868 |
| 10,000 | 11 | hybrid | 888.038 | 599.956 | 962.580 | 706.997 | 0.734 |

Canonical eligibility stage. Shares are sum(eligibilityUs)/sum(engineCallUs) within each process; nested stages are not additive percentile costs.

| Records | Mode | Arm | Eligibility p95 range (µs) | Wall-time share range |
| --- | --- | --- | ---: | ---: |
| 1,000 | vector | baseline | 58–107 | 35.0%–45.8% |
| 1,000 | vector | candidate | 41–105 | 24.7%–30.3% |
| 1,000 | hybrid | baseline | 88–1046 | 37.7%–65.6% |
| 1,000 | hybrid | candidate | 41–96 | 22.1%–35.5% |
| 10,000 | vector | baseline | 549–587 | 61.7%–66.3% |
| 10,000 | vector | candidate | 347–489 | 49.1%–51.7% |
| 10,000 | hybrid | baseline | 566–597 | 62.4%–67.7% |
| 10,000 | hybrid | candidate | 335–373 | 50.2%–52.8% |

[Matrix raw evidence](matrix-evidence.tgz), 763,568 bytes, SHA-256 `724c8f23229439baa026ee927f59c2d2444667a58aade67c7b9fe83796f19187`. Includes frozen manifest, all 20 raw traces/summaries and build/test/environment logs. Bulky fixture/state outputs remain in scratch with their hashes in the manifest; binaries and caches are excluded.

Image: `sha256:b5de9590d0d460e34c1657ca68d6d56bdcf95c4ab23471b76bf08daa2b236efd`.

Unchanged Rust harness SHA-256: `aa8ca7edbb7d023e96de4bc5f3611aa81ca87921c14d44557c5bd3e5b5696579`.

New study driver SHA-256: `16dcbdf0478e0e7e3076bdff438592727de8ec342fa0720d7c96555dc9cfee8c`.

Original study driver SHA-256 remains `343c2fbe769d936ac8f4e9fec48ab6b5a6153eb2cf6383d1331b9989ef91d0c0`.

## Limits and handoff

The unchanged harness invokes actual internal production search/storage adapters,
with deterministic precomputed embeddings and an acknowledged no-write access
recorder. It does not exercise public transport or a live embedding provider.
Storage is tmpfs, all target records are live, graphs are empty, and there are no
supersession/filter adversarial fixtures. Five process pairs and 100 measured
samples per cell on a shared host give descriptive evidence, not a confidence
interval or a release gate. No absolute budget was preregistered. CPU, lock wait,
worker scheduling, persistent-disk cost, public latency, semantic extraction and
answer quality remain unmeasured. Correctness checks here supplement rather
than replace the candidate's tests. No full A5/A6 completion is claimed.

This branch changes only a separately pinned study driver and evidence/docs.
The initial foundation at `159f987`, prior study at `9a9ef83`, prior driver and
Rust harness are unchanged. The next public-transport harness uses read-only
parent `03d9b4d` in a separate worktree. No personal recalld, paid model, merge,
push or deployment was used for this study.

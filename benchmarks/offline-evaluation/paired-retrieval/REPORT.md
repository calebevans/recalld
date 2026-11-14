# Paired retrieval characterization

2026-11-14. Dependent on the stable foundation branch `impl/evaluation` at
`159f987`. Source revisions, unchanged harness, image, seed schedule, order and
measurement limits are described in [README](README.md). This is a diagnostic
cost characterization, not a performance release gate.

## Pilot recorded before extending the matrix

The 1,000-record seed-7 pair completed successfully. This means 100 records in
the queried namespace and 900 foreign-namespace distractors, d384, k20,
20 warmups and 100 measured operations per cell per process. Baseline ran first.

| Engine cell | Baseline p95 (µs) | Candidate p95 (µs) | Candidate / baseline |
| --- | ---: | ---: | ---: |
| Vector | 155.708 | 192.500 | 1.2363 (+23.6%) |
| Hybrid | 151.875 | 127.834 | 0.8417 (-15.8%) |

The mixed single-pair outcome does not establish a general overhead trend.
Neither an improvement nor a failure of a release gate follows from it.
Proceed with the frozen five-seed, two-scale plan to see whether the effect
persists and whether namespace size changes the cost of canonical loading.

Both arms passed their exact library suites (baseline 496; candidate 509), two
harness tests each, and release builds in isolated offline Docker. The requested
candidate is a retrieval branch revision, not the full integration branch, so
its library test count differs from the separately reported integrated suite.
Fixture, ledger, before/after stored records and result ranks/scores matched
across the pair. Scalar oracle, namespace and FTS checks passed. The access
recorder and all Rust harness code were unchanged.

[Pilot raw evidence](pilot-evidence.tgz) retains the frozen manifest, two raw
traces, summaries, compiler/environment and build/test logs. No core source was
modified and no personal recalld instance was contacted.

## Completed matrix

All ten pairs completed: 20 processes, 8,000 measured operations (4,000 engine
and 4,000 exact-index), plus 1,600 warmups. Every pair passed fixture, ledger,
record-state and rank/score hash equality, with no measured failures. Baseline
496 + 2 tests and candidate 509 + 2 tests passed before collection.

The candidate had a median paired p95 ratio of about **1.24x vector / 1.22x
hybrid at 1k**, increasing to **2.85x for both at 10k**. At 10k, canonical
namespace enumeration/loading/filtering accounted for 67–70% of summed
engine-call wall time in each run. This localizes measured cost to that stage;
it does not distinguish CPU, storage-lock wait or worker scheduling inside it.
The target namespace grows from 100 to 1,000 records, while the vector candidate
budget remains 160 for k20. This is consistent with the inspected candidate's
canonical namespace-wide load before retrieval; no optimization was attempted.

Summary statistics below describe five **paired process p95 ratios**, not pooled
or averaged query percentiles. No confidence interval or release-gate conclusion
is claimed from five seeds on a shared host. Stage share is sum(eligibilityUs) /
sum(engineCallUs) over each run's 100 samples, not a ratio of percentiles.

| Records | Mode | Median paired p95 ratio | Pair ratio range | Candidate eligibility p95 range (µs) | Eligibility share range |
| --- | --- | ---: | ---: | ---: | ---: |
| 1,000 | vector | 1.236 | 0.806–1.369 | 53–102 | 42.4%–49.0% |
| 1,000 | hybrid | 1.222 | 0.842–1.367 | 54–92 | 39.6%–46.5% |
| 10,000 | vector | 2.853 | 2.821–3.488 | 540–595 | 66.8%–68.8% |
| 10,000 | hybrid | 2.852 | 1.683–3.719 | 544–571 | 67.8%–70.1% |

Every engine pair, with latency values in microseconds:

| Records | Seed | Mode | Baseline p50 | Candidate p50 | Baseline p95 | Candidate p95 | p95 ratio |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1,000 | 7 | vector | 99.667 | 174.876 | 155.708 | 192.500 | 1.236 |
| 1,000 | 8 | vector | 138.583 | 153.084 | 158.125 | 216.542 | 1.369 |
| 1,000 | 9 | vector | 138.750 | 107.584 | 160.250 | 178.625 | 1.115 |
| 1,000 | 10 | vector | 138.166 | 174.458 | 155.500 | 193.833 | 1.247 |
| 1,000 | 11 | vector | 137.583 | 107.125 | 153.667 | 123.917 | 0.806 |
| 1,000 | 7 | hybrid | 93.792 | 114.375 | 151.875 | 127.834 | 0.842 |
| 1,000 | 8 | hybrid | 87.709 | 132.458 | 147.875 | 202.208 | 1.367 |
| 1,000 | 9 | hybrid | 145.125 | 114.125 | 158.000 | 193.001 | 1.222 |
| 1,000 | 10 | hybrid | 85.208 | 179.625 | 146.416 | 192.291 | 1.313 |
| 1,000 | 11 | hybrid | 137.625 | 124.375 | 157.458 | 191.958 | 1.219 |
| 10,000 | 7 | vector | 262.583 | 779.126 | 281.417 | 981.584 | 3.488 |
| 10,000 | 8 | vector | 239.500 | 748.958 | 278.166 | 784.667 | 2.821 |
| 10,000 | 9 | vector | 207.625 | 756.834 | 275.917 | 787.000 | 2.852 |
| 10,000 | 10 | vector | 259.875 | 776.917 | 307.125 | 876.333 | 2.853 |
| 10,000 | 11 | vector | 264.000 | 777.834 | 280.291 | 810.709 | 2.892 |
| 10,000 | 7 | hybrid | 200.291 | 768.584 | 219.208 | 815.292 | 3.719 |
| 10,000 | 8 | hybrid | 260.625 | 756.209 | 278.833 | 785.167 | 2.816 |
| 10,000 | 9 | hybrid | 210.083 | 764.334 | 262.000 | 797.959 | 3.046 |
| 10,000 | 10 | hybrid | 276.167 | 775.959 | 494.834 | 832.751 | 1.683 |
| 10,000 | 11 | hybrid | 266.876 | 776.626 | 278.917 | 795.584 | 2.852 |

Exact-index controls (also paired process p95 ratios):

| Records | Cell | Median ratio | Pair range |
| --- | --- | ---: | ---: |
| 1,000 | exact-index-100pct | 1.036 | 1.008–1.114 |
| 1,000 | exact-index-10pct | 1.105 | 1.077–1.175 |
| 10,000 | exact-index-100pct | 1.028 | 1.024–1.267 |
| 10,000 | exact-index-10pct | 1.026 | 1.018–1.192 |

[Full matrix raw evidence](matrix-evidence.tgz), 735,025 bytes,
SHA-256 `93bd371062512c82017fc8849af6bec9255fe59f5ae8217de1ef181659024a9a`.
The archive contains the full frozen manifest with source hashes and all ten
pair summaries/artifact hashes, 20 raw traces, summaries, test/build logs and
environment records. Bulky vectors, record dumps, binaries and caches remain in
isolated scratch; their generated output hashes are retained in the manifest.
Every packaged run's raw sample count and every recorded stage p95 were
reconciled against its summary, and source/result hashes were verified.

Image: `sha256:b5de9590d0d460e34c1657ca68d6d56bdcf95c4ab23471b76bf08daa2b236efd`.
Unchanged Rust harness SHA-256: `aa8ca7edbb7d023e96de4bc5f3611aa81ca87921c14d44557c5bd3e5b5696579`.
Study driver SHA-256: `343c2fbe769d936ac8f4e9fec48ab6b5a6153eb2cf6383d1331b9989ef91d0c0`.

## Limits and handoff

This study has no preregistered absolute latency/RSS/error budget, uses 20/100
warmup/measured counts rather than A5's release sample plan, and shares a host
with other work. Values are internal engine calls on tmpfs with fixture vectors,
no access writes, empty graphs and all target-namespace records live. Public
transport, persistent-disk ingestion, contention, chronological lifecycle, real
semantic quality and instrumentation overhead in isolation are unmeasured.
These numbers neither waive correctness nor certify or reject a release gate.

The measured larger-namespace cost is evidence for the retrieval owner to profile
and optimize canonical eligibility while retaining the repaired semantics. The
independent foundation branch stays at `159f987`; this report and driver belong
to the dependent study branch. A subsequent public-transport harness can consume
the forthcoming read-only branch and the A1/A3 readiness/clock interfaces, as
already planned in the foundation README. No public-harness implementation or
core optimization is included here. No personal recalld or paid models were used.

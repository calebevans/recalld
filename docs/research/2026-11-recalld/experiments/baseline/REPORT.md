---
type: experiment
status: done
project: "[[Recalld]]"
started: 2026-11-13
ended: 2026-11-13
---

# Isolated Rust baseline and targeted regression probes

## Question

Does the existing Rust suite pass in a clean Docker scratch copy, and do actual handler/storage and pipeline paths enforce quality/dimension validation and consistent supersession score inheritance?

## Setup

- Source commit: `5aa4692c7d326d2ef10712939eb01484a520e891`.
- Local experiment date: 2026-11-13 America/Los_Angeles; raw container timestamps are 2026-11-14 UTC.
- Scratch: `/tmp/recalld-unit-tests-l__ja1yo`. Tracked regular source files were copied using `git ls-files`, excluding `.git`, `target`, and symlinks. Untracked research artifacts were not copied.
- Image: `rust:1-bookworm`, arm64. Recorded digest and image ID: `sha256:ade39a710ef85f3506cffcfec3e8a24ba090badef1dff2fb798f71d878833fea`.
- Compiler: `rustc 1.100.0 (0ef1bc386 2026-11-09)`, host `aarch64-unknown-linux-gnu`, LLVM 23.1.5. Cargo `1.100.0 (3acc79c0c 2026-11-05)`.
- Test profile: Cargo default test profile, unoptimized plus debuginfo. No production source modifications.
- `cargo fetch --locked` completed first with network access. The repository's `stable` toolchain file triggered a toolchain download during this fetch. Subsequent containers explicitly set `RUSTUP_TOOLCHAIN=1.100.0`, selecting the already installed image compiler and preventing rustup network checks.
- Every test and benchmark container used `--network none` and `--offline`. Only scratch source, scratch Cargo registry cache, and scratch target cache were mounted. No personal data, host home, daemon socket, Docker socket, original repository, or credentials were mounted. No recalld MCP calls were made.
- The suggested `recalld-research-baseline:latest` image was inspected. Its digest was `sha256:738e99eaf63f3c188ed5a8b0783453104db66b1a7d013d1701b9b0d6cef6b27a`; its Dockerfile/build history showed the same Rust base plus `cargo fetch --locked`, with no compile step. Our fetch had already completed, so this image was not used.

## Artifacts and commands

- [Single reproduction runner](run.sh) and [historical Docker command arguments](commands.txt)
- [Compiler and container environment](logs/environment.log), [image identity](logs/image.log)
- [Source commit](source-commit.txt), [original tracked-file SHA-256 manifest](source-sha256.json), [original-source integrity verification](source-integrity.txt)
- [Full dependency fetch output](logs/fetch.log)
- [Full baseline output](logs/baseline-test.log), [exit code](logs/baseline-exit.txt)
- [Full validation probe output](logs/validation-probes.log), [exit code](logs/validation-probes-exit.txt)
- [Full supersession probe output](logs/supersession-probe.log), [exit code](logs/supersession-probe-exit.txt)
- [Scratch-only test patch](validation-probes.patch)
- [Standalone benchmark example source](research_flat_vector.rs)

Run the single maintained reproduction entry point with `bash docs/research/2026-11-recalld/experiments/baseline/run.sh`. It archives the recorded commit into a new `/tmp/recalld-unit-tests-*` tree, pulls the pinned Rust image, fetches locked dependencies, runs the unmodified baseline, applies the exported test patch to scratch, runs validation and supersession tests separately, and runs the release microbenchmark. Expected regression exits (101) are logged and do not stop the benchmark. New logs remain in the new scratch directory, preserving the archived results here.

[Reproduction runner](run.sh) is the executable workflow; [commands.txt](commands.txt) preserves historical commands from this session. The runner pins the image digest and toolchain from the initial fetch, avoiding the historical fetch's redundant rustup stable download. Its validation filter is `mcp::tools::tests::research_`, because the final patch already contains the supersession test; this selects the same six validation tests as the historical first probe run. The obsolete preparatory Dockerfile and earlier runner have been replaced.

The final patch intentionally contains failing regression assertions. It is an exported research artifact, not a fix. The original source manifest comparison reported no changes.

## Results

| Run | Tests passed | Tests failed | Ignored | Scope and duration |
| --- | ---: | ---: | ---: | --- |
| Unmodified baseline | 487 | 0 | 4 doctests | Library suite 1.54 s; Cargo compile/build 29.24 s; both binary test targets have zero tests |
| Added validation probes | 3 | 3 | 0 | Six tests, 32 input cases; 1.59 s; 487 baseline tests filtered out |
| Added supersession probe | 0 | 1 | 0 | One test comparing two candidate-presence cases; <0.01 s; 493 other tests filtered out |

Across the seven added tests: three pass and four fail. These are separate from the 487 passing baseline tests. The 32 validation cases comprise 12 matched expectations and 20 violations. Added probes were not included in the baseline count.

The baseline emits manifest warnings for unused `rust-stemmers` and `tempfile` dependencies. They did not fail the suite.

## Validation findings

The probes call real `dispatch_tool` handlers with real storage adapters over `Fixture` temporary databases. Quality probes first insert an existing synthetic memory, and compare its timestamp, access-history length, and stability before and after the call. Namespace probes read back the namespace from the same temporary database. Rejection assertions require a relevant validation error and no mutation or namespace creation; an arbitrary storage failure cannot satisfy them.

| Probe group | Inputs | Observed |
| --- | --- | --- |
| Invalid quality types / negative value | `-1`, `1.5`, `"3"`, `true`, `null`, `[]`, `{}` | All seven accepted; all seven mutated the synthetic memory |
| Out-of-range unsigned quality | `0`, `5`, `256`, `259`, `u64::MAX` | All five rejected with quality validation; record unchanged |
| Valid/default quality controls | omitted, `1`, `2`, `3`, `4` | All five succeeded and updated the synthetic memory |
| Invalid namespace dimension range | `0`, `8193`, `65535`, `65536`, `65537`, `u64::MAX` | All six succeeded and created a namespace |
| Invalid dimension types / negative value | `-1`, `1.5`, `"4"`, `true`, `null`, `[]`, `{}` | All seven succeeded, inheriting default dimension 4 |
| Dimension boundary controls | `1`, `8192` | Both succeeded with the requested persisted dimension |

Persisted dimensions for invalid range cases were:

| Input | Persisted dimension |
| ---: | ---: |
| 0 | 0 |
| 8193 | 8193 |
| 65535 | 65535 |
| 65536 | 0 |
| 65537 | 1 |
| 18446744073709551615 | 65535 |

The quality handler's `as_u64().unwrap_or(3)` silently substitutes quality 3 for malformed supplied values. Namespace `as_u64()` silently treats malformed supplied values as omitted; `as u16` truncates large unsigned values. These behaviors explain the observed mutations and persisted dimensions. The 1..=8192 rejection expectation follows the existing HTTP handler's validation. These probes do not exercise the HTTP route or external MCP transport.

## Supersession finding

The added test invokes actual `QueryEngine::resolve_supersedes`, using the module's existing graph/cache/metadata fixtures. It holds the old candidate at score 0.9 and ten distractors at 0.5. The replacement is either absent initially, or already present at score 0.1. Both cases remove the old memory and retain exactly one replacement. After descending score sort:

| Replacement initially present | Replacement score | Rank | In top 10 |
| --- | ---: | ---: | --- |
| No | 0.9 | 1 | Yes |
| Yes | 0.1 | 11 | No |

The existing candidate never receives the inherited score, while a newly injected candidate does. The failing assertion requires both paths to preserve at least 0.9 and rank 1. This is an actual Rust stage-level regression reproduction, with synthetic candidates and stubbed backing interfaces; it is not an end-to-end retrieval or live recall measurement.

## Performance microbenchmark

The release benchmark completed successfully (exit 0), with a 52.43-second optimized build. It directly calls the actual `FlatVectorIndex::search`, using precomputed synthetic normalized 384-dimensional vectors and queries, top-k 10, 20 warmup searches and 100 timed searches per scenario. Namespace filtering admits exactly 10% of vectors. Vector generation, index construction, assertions, result destruction, and JSON output are outside the timed interval; search allocation/result construction remain inside it. The interval includes the call, result unwrap/black-box, and timer overhead. Percentiles use nearest ranks 50 and 95 of 100 sorted samples.


Release settings come from the unchanged Cargo manifest: default release optimization (level 3), `lto = "thin"`, `codegen-units = 1`, and `strip = "symbols"`. No custom RUSTFLAGS or target-cpu flags were set. The same Rust 1.100.0 arm64 Linux image was used. [CPU details](logs/cpu.log) describe the container environment. This is one run in an OrbStack VM, with no CPU pinning or isolation from other host workloads.

The harness uses a deterministic xorshift generator with seed 42 for index vectors and seed 43 for queries, followed by L2 normalization. The 1,000-vector dataset is a prefix of the 10,000-vector dataset. Every tenth vector is assigned namespace 1, all others namespace 2; the filtered scenario admits namespace 1. All phases are full, with no tags or minimum-score filter. Scenarios run in the listed order; the first 20 precomputed queries provide warmup, then all 100 are measured. Samples are individual query latencies, not independent repeated benchmark runs.

| Vectors | Namespace filter | Eligible vectors | p50 (µs) | p95 (µs) | Timed queries |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1,000 | None | 1,000 | 22.458 | 23.042 | 100 |
| 1,000 | 10% selectivity | 100 | 3.542 | 4.000 | 100 |
| 10,000 | None | 10,000 | 223.752 | 232.835 | 100 |
| 10,000 | 10% selectivity | 1,000 | 33.042 | 34.584 | 100 |

[Full benchmark build/output log](logs/vector-benchmark.log), [exit code](logs/vector-benchmark-exit.txt), and [machine-readable results with every raw nanosecond sample](vector-benchmark-results.json) are included. Recorded percentiles were independently recomputed from the saved samples and matched.

These timings cover RAM-resident vector search, including metadata prefiltering, dot products, heap selection, and result construction. They exclude embedding generation, HTTP/MCP transport, storage loading, graph expansion, scoring, reinforcement, daemon behavior, and the full retrieval pipeline. They do not establish end-to-end recall latency or production capacity. The faster filtered case skips dot products for 90% of vectors, while still scanning metadata.

## Conclusion

The unmodified suite passes, but targeted actual-code probes reproduce missing malformed-value rejection, unchecked/truncating namespace dimensions, and inconsistent score inheritance for already-present supersession replacements. The exported patch makes those expectations reviewable without changing production code. The benchmark supplies a narrow reproducible vector-search baseline; broader retrieval performance remains unmeasured.

## Log

### 2026-11-13

1. Copied tracked source to scratch, recorded SHA-256 hashes and commit.
2. Inspected Rust image, fetched locked dependencies, then passed baseline with network disabled.
3. Added six scratch-only validation tests; observed three passing control/range tests and three failing rejection tests.
4. Added one actual pipeline supersession test; observed the 0.9/rank-1 versus 0.1/rank-11 discrepancy.
5. Verified original tracked files still match all recorded hashes.
6. Completed the four-scenario release FlatVectorIndex microbenchmark; saved all 400 timed samples and verified percentile arithmetic.

Runner audit: `bash -n` passed, the exported patch passed `git apply --check`, and a [mock-Docker orchestration check](logs/runner-audit.log) verified both expected probe failures still lead to the benchmark. The consolidated runner was not used to repeat the real suite; the recorded actual Docker runs above remain the evidence. The directory entry is verified as uppercase `REPORT.md`.

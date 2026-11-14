# Frozen pipeline instrumentation measurement

**Decision: enabled emission does not pass the 5% overhead gate.** Keep tracing opt-in/default-off. The disabled arm numerically clears this workload's interval bounds; that is not production, latest-integration, or full-A5 promotion.

## Exact source and scope

Candidate `dec5b23199bff767cef1e3ce2c91701f0f730f98` and probe-only anchor `1ceec0b9134456f902de7487d024801f0e5e85cc` both descend directly from frozen Observed #58 `3b590fb28c84bb2aa2d8afaac66e34556d7da6a6`. Their probe bytes are identical. Publication targets the current #58 branch (`faa0b0a647322877c5d44e97107c93d3a61b4e32` at handoff); that newer composition was **not measured** and must be validated separately. No runtime/protocol changes occurred during measurement.

Candidate binary SHA256 `8e878b091971113de622d6c3913d8e710a30ef99e4cfeee74f8043d6c7f55830`; anchor binary `da1bb69833b40cdb1b28c69dc69692d6fc6e2bd3844e5bc422591635e0aac3bf`; probe `d7c1ca01f28ce3c91cd7411596f5085a83b9ca5e0b3dd75930e4994608503079`.

The [preregistered protocol](PROTOCOL.md) fixes five seeds, counterbalanced A/D/E order, 10k records, 384 dimensions, ten namespaces, k=20, hybrid legacy retrieval, graphDepth=0, fixed clock, recordAccess=false and no provider calls. Each arm uses a fresh copy of its closed seed store, 200 warmups, 1,000 latency samples, and at least 60 seconds of completed sequential throughput. Enabled JSON emission and per-request synchronous flush are included. This measures the actual engine/production adapters, not HTTP/MCP serialization or a second ranking implementation.

## Paired overhead

Ratios are candidate/anchor. Intervals are paired bootstrap 95% intervals over all five process seeds (10,000 resamples, fixed seed 20261114), not intervals treating individual requests as independent.

| Arm | p95 ratio (95% interval) | Throughput ratio (95% interval) | 5% cell gate |
| --- | --- | --- | --- |
| Compiled-disabled | 1.0025 (0.9719–1.0412) | 0.9889 (0.9834–0.9946) | Bounds clear in this cell |
| Enabled + synchronous sink | 1.1063 (1.0413–1.1892) | 0.9333 (0.9261–0.9391) | Not passed |

Enabled throughput is below the required 1/1.05 = 0.95238 even at the upper interval bound. Its p95 adverse bound also exceeds 1.05. Retain all five pairs, including seed 101; no retry, outlier removal, or post-hoc workload change was applied.

| Seed / order | p95 ms A / D / E | requests/s A / D / E | maximum RSS MiB |
| --- | --- | --- | --- |
| 7 / ADE | 1.242 / 1.221 / 1.317 | 859.9 / 842.0 / 790.4 | 73.12 |
| 19 / DEA | 1.301 / 1.249 / 1.314 | 844.3 / 833.5 / 789.5 | 72.75 |
| 41 / EAD | 1.211 / 1.293 / 1.359 | 851.0 / 840.3 / 799.4 | 73.00 |
| 73 / AED | 1.213 / 1.245 / 1.311 | 850.2 / 843.9 / 800.0 | 72.20 |
| 101 / DAE | 1.249 / 1.223 / 1.593 | 847.2 / 845.9 / 789.6 | 73.07 |

## Absolute budgets, integrity and host conditions

All 15 arms passed the separate experimental budgets: p95 ≤25 ms (maximum 1.593 ms), throughput ≥40 requests/s (minimum 789.5), and peak RSS ≤1 GiB (maximum 73.12 MiB, Linux VmHWM). These are test-cell budgets, not production SLOs. All process exits were zero; no failed, incomplete or timed-out measured arms occurred.

Every seed has equal result UUID/score hashes and equal canonical record/config/control/vector hashes across A/D/E. Every query interval independently verifies unchanged canonical file bytes. Physical redb hashes across close/reopen are deliberately not treated as query-write evidence; the failed earlier smoke comparison is retained.

The coordinated window began at 13:38:27 UTC on 2026-11-14, with hard deadline 13:58:27. The runner started after authorization, all arms finished by the 13:57:21 status check, and cleanup verification at 13:57:34.903 UTC found zero owned probe containers, including stopped containers. Other agents were released before post-run parsing/compression.

This was build-exclusive, **not whole-host exclusive**. The same 14 unrelated PostgreSQL containers remained running. Preflight host idle samples were 96.58%, 96.13%, 96.66%, satisfying the frozen ≥90%/≤5-point-spread rule. All 77 periodic captures succeeded; sampled container counts were only 14 or 15 (background plus probe). Host idle during measurement ranged 82.67–96.13%, including probe work. Background PostgreSQL CPU summed across containers peaked at 31.87% of one Linux CPU (not 31.87% of the whole host). Sampling cannot exclude shorter contention. Treat these as shared-host characterization; no universal ≤5% promotion claim follows from the disabled result. Published telemetry contains aggregate images/counts/resources, no unrelated names/IDs.

## Archived trace QA

Post-run QA streamed all 9,277,282 enabled events across 244,139 requests. Each request had exactly 38 events and its expected monotonic performance_request ID. Every E file was nonempty and had exactly warmup + sample + completed-throughput request counts. All ten A/D files were empty. A missing/truncated E trace would invalidate its pair.

The checker validates each parent/span stack and worker continuity; exact required stages/counters; exclusive intervals summing to both accounting fields; adapter durations nested within their parent stage; root duration bounded by the caller sample including flush; and a strict field/type/value allowlist excluding content, names and memory IDs. The workload uses a single engine span. Nested REST/MCP-parent behavior is covered by the source tests, not claimed by these engine-only archives.

Per request: canonical namespace enumeration reads 1,000 records / 93,000 encoded bytes, exact vector scan examines 10,000 entries and computes 1,000 dot products, vector and legacy FTS each admit 160, and 20 results are returned. SQLite internal work and legacy graph examination counts remain explicitly unknown. No receipts are emitted in false mode.

Selected mean exclusive stage attribution over the 5,000 enabled latency samples follows (the table is not exhaustive). This is diagnostic attribution, not a separate benchmark or proof of optimization opportunity; synchronous event cost falls into the next interval/residual. Nested lock/adapter intervals must not be added again.

| Exclusive stage | mean µs | share of pipeline |
| --- | --- | --- |
| canonical_read | 265.27 | 21.91% |
| hard_filter | 59.46 | 4.91% |
| vector | 138.80 | 11.46% |
| fts | 590.50 | 48.76% |
| candidate_assembly | 37.29 | 3.08% |
| residual | 11.53 | 0.95% |

Mean pipeline total 1210.91 µs; root 1249.19 µs; caller including sink flush 1264.27 µs. The `rif` interval (31.01 µs) is disabled legacy-processor orchestration, not experimental suppression/pair work. GraphDepth=0 provides no dense graph-cost characterization. The absolute query clock and materialized legacy policy remain unchanged.

## Validation, archives and remaining scope

Candidate source passed 745 library tests and one CLI test in normal parallel execution; four doctests were ignored. Offline fmt/clippy and both final release builds passed with existing repository warnings. Enabled/disabled/anchor functional smokes passed. Earlier failed probes/build attempts are retained, not counted as passing validation. See the [work notes](../../docs/implementation/pipeline-performance.md).

Machine-readable results: [RESULTS.json](RESULTS.json). Raw files remain at `/tmp/recalld-implementation/pipeline-performance/.validation`; the repository [archives/MANIFEST.json](archives/MANIFEST.json) records all per-file/part hashes. The prepared compressed parts remain untracked at `/tmp/recalld-implementation/pipeline-performance/benchmarks/pipeline-performance/archives`; automatic approval review rejected staging the payloads, so publication proceeds with code, reports, QA and the manifest only. A second hash-verified copy is retained at `/tmp/recalld-implementation/pipeline-performance/.validation/verified-archive-copy`. Raw and compressed payloads are not in Git; no alternative upload route is used. Concatenate each group's ordered parts, then extract gzip/tar. The seven parts total 145,349,318 bytes (138.62 MiB), all below 32 MiB each. The 300 members represent 3,739,949,676 raw bytes. Each archive was streamed back and every file compared by size/SHA256; gzip trailers were validated. Seed archives include their original closed canonical store, all A/D/E event/sample/summary/process/host files, and setup evidence. Repeated per-arm store copies are not duplicated in the archive. The validation archive retains source patches, exact probe/protocol, logs, prior failed smokes and all host captures. No raw latency or event sample is dropped.

Unmeasured: full transport serialization/network send, provider queues, write-fsync detail, true-mode throughput/publication overhead, dense graph traversal, RIF pair work, compaction and transitive readiness. No quality gate, native-policy integration, full-A5 completion, default activation, cache/backend change, or later PR58 composition is implied. The next instrumentation step is a separately frozen lower-emission-cost candidate and repeat gate; it must retain eligibility and truthful work counters.

Verified durable work-notes copy: `/Users/caleb.evans/work-notes/work/Experiments/Artifacts/recalld/2026-11-14-pipeline-performance-dec5b23`. All 14 copied files, including the seven compressed parts, were hash-verified. `COPY-MANIFEST.json` SHA-256: `35e18027fe71f6aac6a4785cc7758b0478f2cccd71545015fda8041f373c4ef0`.

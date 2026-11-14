# Actual retrieval instrumentation and bounded overhead evidence

Runtime commit `dec5b23199bff767cef1e3ce2c91701f0f730f98` is a direct child of
Observed #58 `3b590fb28c84bb2aa2d8afaac66e34556d7da6a6`. Its real prepared
selection/exposure/publication/final-admission boundary is needed to time the
whole engine request and retain receipts on post-commit error. No broad/native
integration ancestry is required. The identical probe-only uninstrumented anchor
is `1ceec0b9134456f902de7487d024801f0e5e85cc`, also a direct child of `3b590fb`.
The publication base is current #58 `impl/observed-consumer-join`, head `faa0b0a`
at handoff. Neither that newer composition nor future joins inherit these
measurement claims. The measured source/protocol were not changed mid-run.

Internal target `recalld::performance=trace` adds default-off request spans,
exclusive stages/residual accounting, canonical read bytes/counts, vector work,
FTS coverage/unknowns, graph diagnostics, blocking-worker queue time and lock
wait/hold time. Actual exposure commit, cache publication and final admission
retain their real receipt/error classification. Public StageTimings and transport
payloads are unchanged. No ranking, hard eligibility, projection clock/policy,
lifecycle, readiness or writer semantics change. Disabled context capture does
not start a queue timer. Parent span IDs distinguish nested work; lock durations
must not be added to exclusive stages. Ordinary writer I/O failures are tested;
arbitrary subscriber panic/unwind safety is not promised.

## Source validation

Isolated Docker `recalld-ci:1.94`, image
`sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`,
Rust 1.94.1, network none, 2 CPUs / 4 GiB, CARGO_BUILD_JOBS=2, private copied
Cargo/target directories, no personal data/home/credentials/socket mounts.

- `cargo test --locked --offline`: 745 library and one CLI pass, four ignored
  doctests, normal parallel execution; `.validation/all-tests-final.log`.
- `cargo fmt --all --check` and `cargo clippy --locked --offline --all-targets
  --all-features`: pass with existing warnings; final fmt/clippy logs retained.
- Both `cargo build --locked --offline --features bench --example
  performance_probe --release` builds pass. Final probe-only clippy also passes.
- Actual-engine tests check disabled/enabled result/byte parity, exact work
  counters, no leaked ineligible identifiers, nested spans, cancellation/error
  outcomes and committed-not-ready receipt identity with failing writer I/O.
- Three final functional smokes (anchor, disabled, enabled; 1k records) have equal
  result/canonical state hashes and per-interval physical byte immutability.

Failure history is preserved. An early test tried the offline repair-oriented
hashing helper on a live DB and got RepairAborted; the final test reads its known
canonical files directly. A release probe initially failed to format the current
SHA array via LowerHex; explicit byte formatting fixed it before freeze. A 200-row
smoke could not guarantee 20 nonnegative-cosine candidates under the unchanged
minScore and correctly failed; the 1k functional smoke retains that assertion.
An early cross-open physical redb hash comparison differed while each query
interval was unchanged. The frozen probe separately verifies physical interval
immutability and logical canonical state, using fresh closed-store copies in
measurement. No failed measured arm was retried or hidden.

## Measurement and durable evidence

See [REPORT](../../benchmarks/pipeline-performance/REPORT.md),
[RESULTS](../../benchmarks/pipeline-performance/RESULTS.json), and the unchanged
[PROTOCOL](../../benchmarks/pipeline-performance/PROTOCOL.md). All 15 offline
arms completed within the authorized 2026-11-14 13:38:27–13:58:27 UTC window.
Own-container cleanup was verified at 13:57:34.903 UTC. All five paired seeds
retain result/state parity; absolute experimental latency/throughput/RSS budgets
passed. Enabled emission fails the 5% overhead gate; compiled-disabled intervals
numerically clear this cell but shared-host conditions prevent broad promotion.

Post-run `scripts/pipeline-performance-qa.py` streamed every event, requiring
exact nonzero E traces, zero A/D events, expected counters, hierarchy, accounting
and strict payload allowlists. All 9,277,282 events / 244,139 enabled requests pass.
This QA and evidence packaging are post-run additions, not measured runtime edits.

Exact raw study, failed smokes, test/build logs, source patches and readiness
hashes remain under `/tmp/recalld-implementation/pipeline-performance/.validation`;
anchor build/smoke logs remain under
`/tmp/recalld-implementation/performance-anchor/.validation`.
Prepared archives total 145,349,318 compressed bytes (138.62 MiB) in seven
parts, each below 32 MiB. All 300 members (3,739,949,676 raw bytes) passed
streamed size/SHA256 roundtrip and gzip-trailer verification. The repository manifest records every raw
member and part hash, decompression roundtrip verification and original raw root.
The seven compressed parts remain untracked in the worktree archive directory:
automatic approval review rejected their staging, interpreting the request as
manifest-only and citing repository bloat. No archive was removed or silently
excluded from the manifest. The user confirmed manifest-only Git publication;
no archive staging retry or alternate upload is authorized. A second verified
copy is at `/tmp/recalld-implementation/pipeline-performance/.validation/verified-archive-copy`;
all seven copied hashes match the repository manifest. The manifest also records
source, binary and probe hashes. Neither raw nor compressed payloads are in Git.
The five original closed fixtures are retained; redundant post-query store copies
stay outside Git. Bulky Cargo/target caches are never staged. One instrumentation
and evidence PR targets #58; the orchestrator publishes it after this handoff.

Remaining: full transport/provider/fsync/RIF/dense-graph/true-mode cost cells,
transitive coherent readiness, later native integration and separate A6 quality
and sample-sufficiency gates. No default enablement or full A5 completion claim.

Untracked archive payloads at
`/tmp/recalld-implementation/pipeline-performance/benchmarks/pipeline-performance/archives`:

- `anchor-validation.tar.gz.part001`
- `seed-7.tar.gz.part001`
- `seed-19.tar.gz.part001`
- `seed-41.tar.gz.part001`
- `seed-73.tar.gz.part001`
- `seed-101.tar.gz.part001`
- `validation-and-host.tar.gz.part001`

The orchestrator should push only the committed branch head; `.validation`,
these parts and Python bytecode remain untracked local evidence/build state.

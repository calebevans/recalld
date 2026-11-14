# Internal offline foundation: measured validation

2026-11-14. This is an A5/A6 foundation slice, not full A6 implementation or a
performance/quality promotion result. See [scope and follow-up stack](README.md).

The [evidence archive](runs/2026-11-14-smoke/evidence.tgz) contains the actual run
manifest, three raw JSONL traces and summaries, compiler/environment records,
format/test/Clippy/build logs. Its SHA-256 is
`9c91bb3b1ca19e70ec729eb9662440f25b15def79cc61ef7bb5f309b249116e3`.
The archive is 131,726 bytes; build caches, binaries, vector fixtures, full record
dumps and CPU/memory inventories are excluded. The manifest retains their hashes
where generated; excluded fixtures can be reconstructed with the pinned source.

Validation used `recalld-ci:1.94`, resolved before execution to
`sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`,
Rust **1.94.1**, `aarch64-unknown-linux-gnu`, release profile with repository
defaults, two Tokio workers, two build jobs, **2 CPUs / 4 GiB**. All builds, tests
and measurements ran with Docker `--network none`, only a fresh scratch bind
mount, no home/credential/socket mounts and a fresh temporary home. Fixture
storage was **tmpfs**, so ingestion is a CPU/logic control. No personal recalld
process, data or endpoint was used. No providers, models or datasets were fetched.

The source baseline was `653c8d32f526e78d3f51a39cf6dea2d50c07bcf2`, including the
recent tombstone-sweep and daemon-socket fixes. The uncommitted harness files were
captured by per-file hashes, not just the empty tracked patch. Executed hashes:

| Input | SHA-256 |
| --- | --- |
| `examples/offline_eval.rs` | `aa8ca7edbb7d023e96de4bc5f3611aa81ca87921c14d44557c5bd3e5b5696579` |
| `scripts/offline-evaluation.py` | `aad5ddc3c8c804bc4867c243ff1b079ae521a99dc120783a6fc70789c3f839fa` |
| Entire captured source tree | `bab3e1a12449870b6fbec0f5983e302bb8f18f7f968e10f8730e43cf5f3fad17` |

The report/archive were added after execution; the manifest identifies the
actual build inputs. No core source or dependency files changed.

## Executed checks

- `rustfmt --check --edition 2024 examples/offline_eval.rs`: passed.
- `cargo test --locked --offline --lib`: **496 passed, 0 failed**.
- `cargo test --locked --offline --example offline_eval`: **2 passed, 0 failed**;
  hand-calculated source metrics/percentiles and seeded normalized fixtures.
- `cargo clippy --locked --offline --example offline_eval`: succeeded; **207
  existing library warnings**, no warnings in the new example. This is not a
  warning-free repository claim.
- `cargo build --locked --offline --release --example offline_eval`: passed.
- Three independently started release processes with fresh stores: passed.
  Fixture vectors/mappings, attempted/accepted turn ledger, stored records before
  and after queries, and returned ranks/scores matched byte-for-byte across all
  three processes. Each process also verified unchanged stored-record bytes.
- Packaged artifacts match the manifest hashes; raw sample counts and every
  stage's p95 were independently reconciled with the summaries.

## Actual latency samples

Each process used seed 7, **1,000 records, d384, k20**, 20 warmups and 100 measured
queries per cell: **1,200 measured completions**, no failed measured queries,
plus 240 warmups. Two namespaces contain 10% and 90% of records. These are
repeatability runs with the same seed, not five independent stochastic seeds.

Values below are **p50 / p95, microseconds**, shown separately for each process.
Index timings cover `FlatVectorIndex::search`; engine timings cover the awaited
`QueryEngine::search`, excluding the diagnostic access drain and JSON encoding.
Full per-stage p50/p95/p99, counts, raw samples and response bytes are archived.

| Cell | Run 0 | Run 1 | Run 2 |
| --- | ---: | ---: | ---: |
| Exact index, 100% eligible | 26.916 / 31.459 | 26.917 / 33.041 | 27.625 / 32.750 |
| Exact index, 10% eligible | 5.000 / 10.834 | 5.834 / 15.541 | 4.875 / 5.834 |
| Internal engine, vector control | 121.834 / 153.291 | 148.042 / 187.791 | 92.959 / 116.293 |
| Internal engine, vector + FTS | 114.875 / 141.459 | 139.875 / 184.500 | 102.000 / 203.501 |

The exact index matched the separate scalar exhaustive oracle at both
selectivities, with identical IDs and absolute score tolerance 2e-6. The vector
engine matched the eligible nonnegative-score oracle. Both engine cells returned
the self-query source first and passed namespace/live-record checks. The hybrid
cell required a positive FTS score; the vector control required no FTS hits.

All 1,000 raw synthetic units were accepted per process. On the 600 measured
engine queries, extraction evidence retention and retrieval Recall@5/10/20,
all-required coverage and binary source nDCG@10 were 1.0. These are **synthetic
self-retrieval checks**, not evidence of semantic extraction, proposition
preservation, conversational retrieval or answer quality. Golden tests cover
partial/empty/duplicate/unknown evidence separately.

No percentiles are averaged, no throughput is inferred, and no comparison with
the older 10k/k10 index measurement is claimed. Variation between processes is
visible; the host was not reserved for performance measurement. Absolute budgets,
paired candidate comparisons and grouped confidence intervals are absent.

## Remaining scope

This uses real index/engine/storage code with direct internal fixture ingestion,
a synthetic embedding registry and an acknowledged no-write access adapter.
It does **not** exercise public MCP/HTTP validation or prove public false-mode
reads, A1 receipts/readiness/snapshot/reset, A3 injected time, or corrected A2
retrieval semantics. It does not alter those owners' production files.

The dependent stack in README covers public serialized actions and conformance,
generation-aware replay/clock controls, correction/deletion/failure matrices,
complete source mappings/dataset scorers/grouped evaluation, and A5 transport,
lock/resource counters and preregistered release gates. Real extraction and
answering, 10k/100k matrix cells, contention, persistent-disk ingestion, policy
experiments and all learning work remain unimplemented/unmeasured here.

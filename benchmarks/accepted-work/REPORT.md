# Accepted-work core: isolated validation

Measured runtime: `50e0c5e0d42bcdafa24acb9a9c267c5d3318fe85`, branch
`impl/accepted-work`, base `f6e4d9263fe2d96fca16282eed47b1251ad5398b`.
Evidence branch: `study/accepted-work`, based on frozen PR64 evidence tail
`0dd9954d0b454726c333f7635300e5547790dd45`. It adds no runtime ancestry and does not
modify prior public-conformance or performance evidence.

## What changed

MetadataStore owns one accepted-work tracker. Non-cloneable tickets register
roots before enqueue/publication waits and children before their parent finishes.
Closing rejects new roots while accepted descendants finish. Abandoned or
uncertain work remains unresolved. Same UUID is insufficient authority: tickets
must refer to the same owned core.

Actual metadata mutation, purge and canonical acknowledgement gates participate.
Insertion/tombstone/purge hold the same ticket-bearing guard through canonical
effect application. The actual DerivedCompletion insertion, forget and completion
paths propagate parent tickets through workers, purge resume and acknowledgement.
The completion root stays active until worker and publication leases are released.
Committed receipt error wrappers are preserved.

Metadata capture consumes the closed admission lease, takes publication, checks
durable manifests and pins the real canonical GenerationRef. It is deliberately
not whole-store readiness, a ReadView, a generation-targeted barrier or a snapshot.

## Exact results

| Source | Tracker/metadata | Coordinator | Full library | CLI | fmt / Clippy |
|---|---:|---:|---:|---:|---|
| `9b6c7ec` | 13 pass, 1 fail | not run | not run | not run | fmt pass / not run |
| `b627cd5` | 14 pass | 9 pass | 715 pass | 1 pass | pass / pass |
| `50e0c5e` | 14 pass | 9 pass | 715 pass | 1 pass | pass / pass |

The initial failed fixture attempted to purge the protected default namespace.
It was corrected to create a separate namespace; production rejection was not
weakened. The final revision removes one newly introduced needless borrow.
Both earlier runs are retained, including their exact source and raw failure.

Final focused groups took 0.87s and 0.91s; the full library result reports 71.59s,
and the CLI reports 0.87s. These are functional test runner durations, not a paired
performance study or latency budget. Library tests used normal parallel execution
without RUST_TEST_THREADS override. No example target exists on this source base.
Clippy exited 0 with 427 library warnings, 447 library-test warnings (427 duplicates)
and one binary warning; this is not a warning-free result.

The 17 new regressions cover ancestor cancellation while a child is running,
consumed parent authority, transitive descendants during closing, same-UUID foreign
cores, reclose after timeout, concurrent closer refusal, spurious wakeups/expired
deadlines, sequence exhaustion, a real writer waiting for publication, actual
queued insertion and purge after close, async cancellation, unavailable/panicking
writers, publication contention and durable pending effects/purge despite stale
volatile flags. Existing coordinator receipt/retry/cache tests also pass.

## Isolation and reproduction inputs

All runs used image
`sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`,
Rust 1.94.1, network-none Docker, two CPUs, 4 GiB RAM, read-only source snapshots,
temporary HOME, no credentials/home/socket mounts, and private copied Cargo/target
caches. Actual cgroup files report `200000 100000` and `4294967296`. The A5 hold
was respected; jobs began only after explicit END/release.

Each archive includes the exact allowlisted source snapshot, raw output logs,
manifest, driver and dirty patch. The existing source-snapshot policy rejects
undeclared target assets and unknown untracked files; it includes new tracked
Rust modules and explicit Cargo targets. Legacy external datasets are excluded
and recorded as excluded inputs, not invoked by these synthetic tests. Every
run's 167 source hashes and all recorded output hashes were reverified, including
each archived member. No cache/build output tree is archived. Exact Docker
commands are in `output/manifest.json`; the main checks are:

```text
cargo fmt --all --check
cargo test --offline --locked --all-features --lib work::tests -j1 -- --nocapture
cargo test --offline --locked --all-features --lib derived_completion::tests -j1 -- --nocapture
cargo test --offline --locked --all-features --lib --bins --tests --examples -j1
cargo clippy --offline --locked --all-features --all-targets
```

Cargo test build concurrency was one; CARGO_BUILD_JOBS was capped at two for other
commands. The driver does not record exact wall-clock launch/finish timestamps;
do not infer them from archive times. Each source root was immutable for its run.
See [artifacts.json](artifacts.json) for SHA-256, sizes, verified counts and raw paths.

The reported BEFORE/AFTER/switch-back target hazard was not used here: each source
revision had a separate physical private target directory, seeded from a completed
prior cache, and no container switched sources within its run. All logs show
current crate compilation and the named new tests. Supplemental
[binary-hashes.json](binary-hashes.json) records post-run SHA-256/size for every
executable named in each Cargo `Running` line (1 in the stopped run; 5 in each
passed run), with per-source target roots and log associations. Those targets
were not reused for another source after their run. The original archives remain
unchanged. This is not a new execution or independent CI claim.

Final raw output: `/tmp/recalld-accepted-test-wjgq37hv/output`.
Final archive: [passed-50e0c5e.tgz](passed-50e0c5e.tgz), SHA-256
`f1f59723dcb51127d9a6376d1b76d3e611ee7c501f2fc175793e1255c81d0456`.

## Remaining scope

No new public-transport replay was run on this core-only base. Earlier public
9-episode/receipt/false-byte evidence remains pinned to its own measured sources.
No native/public-v2 activation, deterministic replay, semantic quality, CI/release
gate or performance acceptance is claimed.

Remaining joins include all queued public/background writers, maintenance and
graph/autolink work, native writers, ordered-batch pending manifests, real derived
generation publication and coherent query ReadViews. Current capture checks its
deadline after full manifest scans: late success is refused, but scans and blocking
OS I/O are not bounded by that check. A real 30-second barrier needs cooperative
deadline checks or bounded already-validated pending queries while preserving
strict startup validation.

Whole snapshot/reset additionally needs actual frozen clock/config/ID-RNG/cache
state and validated canonical files, terminated old workers, a fresh isolated
destination and verified logical hashes. MemoryId UUIDv7, store/operation UUIDs and
reserved purge/batch child IDs are not controlled by ManualClock. A shared owned
ID/RNG source or a proven reference- and tie-order-preserving bijection remains a
prerequisite; no global RNG override or private fixture ingestion shortcut.

After controls, A6 still requires manifest/schema validation, fail-closed offline
replay misses, dataset adapter/scorer contracts and scripted chronological fixtures.
No external models, paid calls or training were used; human-judge calibration
remains an external gate. This is a validated foundation slice, not full A6.

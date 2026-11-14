# Canonical harness compatibility, 2026-11-14

Measured source: `90574b9ac5ddd83e5fccb66bca290e2442841200`, branch
`impl/canonical-harnesses`. Actual parents: writer restrictions PR70 `af20d73`,
original offline foundation PR18 `159f987`, and frozen A5 source `16c574b`.
There is no accepted-work, native-writer or broad integration parent.
Only the example/helper, runner and implementation-document files are newly
changed after those actual parents; [runtime patch](runtime.patch) preserves the
exact source delta. Historical benchmark directories remain byte-identical to
their respective parent commits.

Both harnesses seed using the existing canonical `InsertMemory` receipt path,
then real production startup/completion. This is an internal Rust path, not public
transport conformance. No raw production writer escape or new library API is
introduced. Existing exact-index vectors, scalar oracle and supplied memory IDs
remain. Lifecycle uses ManualClock; store and operation IDs are still random.

## Actual checks

| Check | Result |
|---|---|
| Library, normal parallel runner | 851 passed, 0 failed, 169.60s |
| CLI | lifecycle 1 passed; text migration 1 passed |
| Examples | offline_eval 3 passed; performance_probe 1 passed |
| Source boundary/receipt projection runner | 7 passed |
| Formatting / Clippy all features, all targets | Passed; Clippy warnings retained |
| Fresh offline processes | 3; each 100 records, d16, seed 7, k5, 1 warmup, 3 samples/cell |
| Probe smoke | 1,000 records, d384, seed 7; disabled/enabled; 1 warmup, 3 samples, 2 throughput requests |
| Historical matrix guard | Changed probe rejected before output creation or Docker invocation |

Each offline process retains 100 full insertion receipts and reaches generation
104: initial control, two fixture namespace creations, 100 insertions and one
production default-namespace creation. Source/memory identities, unique operation
IDs, receipt lineage, contiguous insertion generations and final write counts
are checked. Full receipt semantics and control fields match across processes
except the explicitly validated generated store/operation identities. Arbitrary
semantic differences are rejected. Receipt logs are never rewritten.

The four identical artifacts are `fixture.json`, `state-before.json`,
`state-after.json` and `replay.json`. Within each process, records and control
remain unchanged across false-mode queries. The exact claim is **deterministic
fixture/result projections only**, not deterministic canonical state/reset/replay.
There is no identity-control or whole-core snapshot claim.

Probe result SHA-256 is
`30db0cf5ef44be450df0e6ef6227c9dddbfd5713befeeb5d13facf41a3198967`;
its limited canonical logical-state digest is
`c49ead70d7af95e2148f99159a8f58fb3fa2e6396d4f5a6926528e3970f2e929`
in both processes. The digest covers control/namespaces/record/vector projections
and requires no canonical edges; it does not enumerate every receipt/manifest.
Enabled tracing emits 228 events, disabled tracing zero. Both actual probe
processes pass their per-query-interval physical-file and logical checks.
Across restart the physical file hashes differ; both hashes are retained in
[output checks](output-checks.json). Startup is outside that false-query interval.
No physical restart determinism, overhead comparison, release gate or quality
claim is inferred. No new performance matrix was run.

## Failure evidence and correction

1. `0c8ab9e`: two original example tests passed; the new helper test failed in the
   production adapter because it used single-thread Tokio. Its corrected test
   uses the same two-worker runtime kind as the real example. No production change.
2. `e23e7f8`: six runner checks, all four example tests and three offline processes
   passed. The 200-record probe stopped at its exact-20-result guard: its selected
   namespace contained only 20 records before score filtering. The bounded smoke
   was increased to 1,000; the guard and filters remain unchanged.
3. `90574b9`: seven runner checks, four example tests, all three offline processes
   and both 1,000-record probes passed. A supplemental checker incorrectly
   required physical database hashes to match across startup. The failed checker,
   both hashes and raw output remain immutable. The corrected supplement checks
   logical/results across processes and preserves the physical difference.
4. Same `90574b9`, final supplement: corrected output checks, full tests, fmt and
   Clippy pass. No source was changed between these last two runs.

Clippy reports 459 library warnings, 519 library-test warnings (452 duplicates),
and one binary warning. Logs retain the details; no warning-free claim.

## Isolation, provenance and files

All execution used Rust 1.94.1 / aarch64 in image
`sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`,
Docker `--network none --cpus 2 --memory 4g --memory-swap 4g`, dropped capabilities,
no-new-privileges, read-only root/source and temporary HOME. No personal
home/data/credential/socket mounts or external calls. Cargo was offline/locked
with `CARGO_BUILD_JOBS=2` and one build job on the explicit test commands.

Each source revision used a distinct physical private target, copied only from a
completed own cache, then `cargo clean -p recalld` before rebuilding. The final
supplement uses the same immutable source and same private target as its preceding
run, with no source switching. Exact executable hashes are in
[artifacts.json](artifacts.json); pre/post supplemental hashes of the original
example binaries match. Library executable SHA-256:
`da6e36ab8737e9ce13463bcdc8aeee563a773506a201feba75898b14b05fcadf`.

Each archive contains its 201-file source snapshot, raw outputs, command/source
manifest and exact validation driver; all source/output hashes and archive members
were reverified. The failed `90574b9` archive contains the successful process runs
plus failed checker. The passed archive is their corrected validation supplement,
not a relabeling of that attempt. Closed synthetic probe stores are retained with
file hashes to preserve their receipt history. They are forensic evidence, not
validated frozen/reset snapshots. Build caches, executables and ephemeral lock
files are excluded. Combined archives total about 5.7 MiB.

Final archive SHA-256:
`6d3d27f4519376ff37690150d26e10d52282b006b0803e6a3fe85f28a73f6a59`.
Raw roots and all other archive hashes are in `artifacts.json`. Final output:
`/tmp/recalld-canonical-harness-supplement-lpi73dbg/output`.

## Remaining scope

No new public nine-episode suite was run here. Accepted-work PR73 is untouched.
The next task is actual shared publication-registry/completion/barrier integration
with Feynman's scoped canonical snapshot, Curie's batch work and Pascal's native
owned core. Catalog/policy/Observed/batch commits must participate, with exact
derived GenerationRefs rather than effects-epoch or metadata-only readiness.
Whole-core drain, bounded/cooperative scan checks, owned clock/ID/RNG/cache freeze
and verified fresh-root reset remain prerequisites. Blocking OS I/O precludes an
unconditional hard timeout. A6 manifests/schema faults/dataset adapters/scorers and
chronological fixtures remain; public/native-v2 and human judge calibration are
separate gates. No external models, training or full A6 completion is claimed.

# Canonical writer harness compatibility

Minimal actual parents are writer restrictions `af20d7331410feef8ea1d2a8803e6a0772bb5d99`
(PR70), original offline foundation `159f987` (PR18), and A5
`16c574ba9e6fde4efa3a8ad6ba065731e67f9e94` (runtime `dec5b23`, parent `3b590fb`).
This branch does not import the tracker or broad integration history. Feynman
retains offline export/restore ownership; no duplicate example changes were planned.

Both examples now call the existing canonical `insert_memory_with_receipt` API
with `InsertMemory`, then use `Recalld::with_clock` and existing
`complete_pending_effects` for actual production startup/view publication. Raw
DiskRecord insertion, private metadata access, ad hoc effect acknowledgement and
public fixture-loader escapes are not used. The helper is example-only code;
no production Rust API is added. Sweeps are disabled, configuration is constructed
without ambient loading, and the provider is offline passthrough. All external
model/network/training calls remain zero.

The original exact-index synthetic vectors, IDs and scalar oracle are preserved.
Canonical seeding explicitly supplies supported caller-chosen memory IDs,
timestamps and vector bytes. The real writer validates and normalizes them and
creates receipts. Store/operation UUIDs remain random: this is not whole-store
deterministic replay, a public transport fixture, a ReadView or a snapshot/reset.
Production startup may create its default namespace; it is part of the new
fixture state. A bounded helper regression proves invalid vector and duplicate
ID rejection leave control unchanged, then queries the accepted record through
the actual system QueryEngine after startup.

The offline harness is now `canonical-internal-offline-v2`, with the production
access recorder and explicit false mode. It compares record bytes and canonical
control before/after its query interval. It does not fabricate a background-work
barrier. The turn ledger retains actual insertion receipts. Cross-process stable
artifacts cover deterministic fixture/record/result projections only, not
deterministic canonical state, reset or replay. Full receipts remain in evidence.
The runner verifies source/memory references, unique operation IDs, store lineage,
contiguous insertion generations, final write count and unchanged per-process
control. Only checked store/operation IDs are excluded from cross-process receipt
comparison; every other receipt field and control field must match unchanged.

The probe is `canonical-writer-internal-probe-v2`. Initialization completes actual
startup before writing a versioned fixture marker. Query processes require that
marker and matching seed/count, rebuild through the production path and retain
false-query canonical-file/control/record/vector checks. Its canonical-state hash
does not enumerate the entire receipt history or every store manifest. The small
smoke can lower the minimum throughput-request count; the normal default remains
1000. Smoke durations are not new performance evidence or a release gate.

Historical probe SHA-256 is
`d7c1ca01f28ce3c91cd7411596f5085a83b9ca5e0b3dd75930e4994608503079`; original A5 and
PR18 reports, measured sources and archives remain unchanged. The historical A5
matrix driver now refuses a changed probe before any Docker call/output creation,
requiring the old frozen checkout or a newly authorized/frozen protocol. No old
measurement or promotion claim transfers to this composition.

The offline runner includes tracked Rust modules under src/tests/examples/benches
and verifies explicit Cargo targets; unknown target assets/build scripts fail
closed rather than being silently omitted. Scratch, untracked inputs, ambient
Cargo configuration and secrets remain excluded. Required new example modules
and CLI sources are covered by runner regression checks. Final validation uses
separate source-specific private targets and `cargo clean -p recalld` before the
crate rebuild, preserving dependency caches and executable hashes.

## Frozen validation

Measured source is `90574b9ac5ddd83e5fccb66bca290e2442841200`. Seven runner
checks and four example tests pass. Three fresh processes each ingest 100 records
at d16, retain 100 insertion receipts and reach canonical generation 104. The four
declared fixture/record/result artifacts match; receipt semantic/reference/count
checks and each false-query control check pass.

The probe smoke uses 1,000 records at d384, one warmup, three samples and two
throughput requests, with trace disabled/enabled. Both runs pass their per-process
false-query physical-byte and logical-state checks. Their result/logical-state
digests match; enabled tracing emits 228 events versus zero disabled events.
Physical database hashes differ across startup, as retained in the raw evidence.
This is not physical restart determinism or a measured overhead comparison.

Three failed attempts are retained: the first helper test used single-thread Tokio
with a production adapter requiring a multithread runtime; the 200-record smoke
did not provide 20 eligible results; and a supplemental checker incorrectly
required physical database hashes to match across startup. The final supplemental
check preserves that difference and compares the declared logical projection.
No production assertion, filter or canonical field was weakened.

Full validation passes: **851 library tests, 2 CLI tests, 4 example tests,
7 runner checks, formatting and Clippy**. Clippy emits 459 library warnings,
519 library-test warnings (452 duplicates) and one binary warning. The normally
parallel library run took 169.60s; this is not a performance result.
[Evidence and exact commands](../../benchmarks/canonical-harnesses/REPORT.md)
are retained in this same branch. Source-specific private targets and an
explicit crate clean prevent stale executables across revisions. The final
supplement uses the exact same immutable source and same private target, with
pre/post executable hashes, without a source switch. The source/commands, failure
logs, full receipts and closed synthetic probe stores are retained as evidence;
the stores are not certified A6 snapshots. No new public transport suite or
performance matrix is claimed on this composition.

## Next actual controls

Feynman owns the bounded canonical-vector snapshot backend; it is independent of
whole-query derived readiness. This agent owns the shared publication registry
and completion/barrier integration, consuming Curie's batch/tracker and Pascal's
native owned-core joins. Actual catalog, policy, Observed, batch and content
commits must invalidate readiness; graph/vector/FTS/entity/cache publications
must carry matching canonical GenerationRefs. An effects epoch or metadata-only
lease cannot substitute for those hooks.

Whole-core admission/drain, coherent views, owned clock/ID/RNG/cache freeze and
verified fresh-root reset remain required before deterministic canonical replay.
Long scans need cooperative deadline checks; blocking OS I/O still prevents an
unconditional hard timeout claim. This compatibility join does not complete A6.

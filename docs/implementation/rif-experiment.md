# Query-local RIF operator experiment

Runtime commit: `7f41eae7832a05a398cc0a5431be0e03509bf20c`.
Only parent: Observed consumer join `faa0b0a647322877c5d44e97107c93d3a61b4e32` (58).
This implements the internal adapter in A2 §8. It does not activate public v2,
establish coherent serving readiness, or pass a ranking promotion gate.

`QueryEngine::with_rif_mode(RifMode::{Off, Shadow, Experimental})` is a Rust
operator/harness setting. Off remains the default. V1 retains its existing
configured processor. V2 Shadow and Experimental require the original query to
explicitly use `recordAccess=false`; true intent fails before selection and is
never rewritten. Shadow computes diagnostics while retaining Off scores/order.

The engine freezes the preliminary top-L resolved, Off-ranked anchors. Actual
one-hop typed graph reads return `Result`; unsupported or corrupt reads fail
instead of becoming empty success. Self, structural Supersedes, foreign,
ineligible, ghost, and tombstone pairs do not participate. Canonical stability
and policy-selected retrievability come from the existing pinned strength
context; available stored vectors supply cosine similarity. Missing similarity
is counted explicitly. No second query clock/policy pin is introduced.

Pairs deduplicate by (anchor, target), choosing maximum computed activation and
then edge-type order. Admission sorts by activation descending, then anchor and
target UUID, with signed zero normalized. The limits are **100 admitted unique
targets per anchor and 1,000 admitted pairs per query**. They are not examined-edge
or allocation bounds: `rif_edges` collects adjacency, and the engine collects and
deduplicates all eligible pairs before activation sorting/admission. There is no
hidden scan truncation. `examined_edges`, eligible/admitted counts, duplicate
counts, and cap-hit diagnostics expose that distinction. Dense-graph latency and
memory costs require A5 measurement before promotion.

One query-local `QueryRifContext` limits cumulative target multipliers to
[0.75, 1.0], respects actual stability floors, and never enhances or persists
strength. Independent original score contributions survive supersession and
graph rediscovery. Each contribution is recomputed once with the original
baseline channel formula and temporal boost; terminal scores take the maximum
of own and inherited evidence. Direct/FTS/graph formulas can remain unchanged
despite effective-retrievability suppression. `unchanged_score_channels` counts
unchanged independent contributions non-exclusively, not unique results or
rank identity. Per-ID multipliers are restricted to final returned IDs.

The existing `StageTimings.apply_rif_us` measures the overlay; `rank_us` excludes
that work and includes any final Experimental sort. No timing fields or wire
options were added. Later instrumentation consumers should preserve this
accounting when joining branches.

## Validation

Final immutable allowlisted source SHA-256:
`bc9a690a9c89f396b7c311382954942c62838919531435057da6d5a12363f45b`.
All 237 source files matched before and after execution, and the working source
matched the snapshot before commit. The snapshot was copied, made read-only,
and mounted `/work:ro`; no host source edits occurred during the run.

Docker `recalld-ci:1.94`, `RUSTUP_TOOLCHAIN=1.94.1`, `--network none`,
`--cpus 2 --memory 4g`, `CARGO_BUILD_JOBS=2`, private Cargo and target paths,
no home/credentials/socket mounts. Source switching was followed by
`cargo clean -p recalld` in this task's target (dependencies retained), never a
global or another agent's cache clean. Executed test binary hashes are in the
[manifest](../evidence/rif-experiment/MANIFEST.json).

- `cargo test --locked --offline`: **789 library + 2 CLI passed**; four existing
  doctests ignored. Normal parallel library run: 207.77 seconds.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --offline --all-targets --all-features`: completed with
  warnings; no claim of warning-free compilation.
- Fourteen added regressions cover deduplication, exact cap boundaries,
  cumulative suppression, real S/R/similarity, all retrieval channels,
  inheritance/temporal scoring, eligibility/redaction, errors, signed-zero
  ordering, original access intent, and a real canonical offline snapshot.
  The snapshot fixture uses actual schema/lifecycle migration receipts under
  both lifecycle policies, pins one clock per query, forbids side-effect
  adapters, and checks canonical bytes across queries, task yield, and reopen.

The neutral-overlay graph-rediscovery regression first failed with different
Off/Experimental score bits (`1029939577` versus `1017370379`). Retaining each
independent score contribution fixes that actual failure. Its failed run is
preserved, along with the initial compile error, invalid-query test fixture
failures, formatter partial-file failures, and mounted source hash mismatches.
Those attempts are not counted as successful validation. An earlier immutable
788-test run is retained separately; the final run adds signed-zero cap coverage
and the timing-accounting correction. No failure was hidden by a global clean
or an unrecorded retry.

Raw logs and source patch remain outside Git at
`/tmp/recalld-implementation/rif-experiment/.validation/handoff-evidence`.
The manifest records each file's hash and size. A verified persistent copy is at
`/Users/caleb.evans/work-notes/work/Experiments/Artifacts/recalld/2026-11-14-rif-experiment-7f41eae`;
its `COPY-MANIFEST.json` enumerates the copied evidence. No cache, executable,
or raw archive payload is committed.

## Remaining scope

A6 must run matched Off/legacy/deduplicated variants with sufficient independent
groups and every required channel/current/history stratum. Positive paired
nDCG, per-stratum Recall bounds, and p95 ≤1.10× Off remain unmeasured. Dense-graph
examined work/allocation and actual latency/RSS also remain unmeasured. Leave
Off as default; this proof does not establish full A2 or public v2 readiness.
Persistent RIF and true-mode lifecycle writes remain separate work.

After this correctness slice, the smallest proposed A5 overhead response is
request-level deterministic sampling with explicit sampled diagnostic counts,
while complete benchmark request/latency counters remain unsampled. Any buffered
sink must include emission/drain cost in a new frozen paired comparison against
both an uninstrumented anchor and compiled-disabled instrumentation. The prior
enabled ≤5% gate failed and remains failed; transport, write/fsync, and true-mode
cost cells remain separate missing measurements. No new measurement or default
enablement is claimed here.

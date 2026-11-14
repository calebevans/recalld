# Lifecycle review: deterministic persisted-state regressions

Reviewed `211b1bd409570d4cb7fc594ecc90d4fad56a3293` read-only. An isolated archive of that commit plus [probes.rs](probes.rs) reproduced **three failures**. The subsequent fix `8cbb7344b1c45cc444b32781fa6c52d0c6f88ceb` passed the three named regressions and two additional guard/lock-order tests in an independent rerun. No lifecycle source was edited by the reviewer.

| Finding | Deterministic schedule and baseline observation | Fixed regression |
| --- | --- | --- |
| P1: stale sweep overwrites committed feedback | Scan/plan Full at day 8, commit quality-3 feedback, then apply the saved sweep snapshot. Canonical `(stability, permastore, phase)` becomes `(1.0, 0, Summary)` instead of `(17.70828, 1, Full)`. | `review_stale_sweep_must_not_overwrite_committed_feedback` |
| P1: graph phase trails successful canonical transition | Trigger two sweeps with a fixed maximum timestamp. Both report zero errors; canonical phase is Ghost while graph remains Summary. | `review_successful_sweep_must_publish_final_graph_phase` |
| P2: access regresses the feedback clock | Reinforce with day-8 clock, record access with day-1 clock. Persisted anchor changes from 691200000 to 86400000. | `review_access_must_not_move_feedback_anchor_backward` |

These are contract regressions demonstrated in the reviewed tree; the sweep gaps were not necessarily introduced by that commit. The tests use real storage, graph and adapters with synthetic fixtures, explicit interleaving and injected clocks. They do not use sleeps, provider calls, personal memory, the daemon, or host credentials.

## Evidence and reproduction

- [Baseline raw log](baseline.log): 0 passed, 3 failed, 505 filtered, exit 101 (expected).
- [Fixed raw log](fixed.log): 5 passed, 0 failed, 507 filtered, exit 0. This independently checks the fix-owned test module; it is not a verbatim reapplication of the original module because internal expected-state interfaces changed.
- [Manifest](manifest.json): full revisions, exact original probe hash, source hashes and output hashes.
- [Runner](run.sh): extracts the chosen Git commit into scratch and mounts only that source, the probe, a dependency registry, and a scratch build target. Image must be prepared separately. Every test container uses network none, two CPUs, 4 GiB, two Cargo jobs and Rust 1.94.1. No host home or socket mount.

Run from any checkout containing both commits, passing a prepared Cargo registry and scratch target:

```sh
bash docs/research/2026-11-recalld/evidence/lifecycle-review/run.sh baseline /tmp/cargo/registry /tmp/lifecycle-target
bash docs/research/2026-11-recalld/evidence/lifecycle-review/run.sh fixed /tmp/cargo/registry /tmp/lifecycle-target
```

The retained baseline log is the second reproduction (0.20 s test execution); the original reproduction also failed all three (0.21 s). The fixed rerun executed five tests in 2.30 s. These timings are diagnostic, not performance measurements.

## Other review findings and limits

The review also identified Ghost feedback mapped to HTTP 500 instead of a typed expected error, namespace creation accepting initial stability that the new resolver rejects, and partial-write cache/publication gaps. The lifecycle author reports fixes and 512 passing tests in `8cbb734`; see `docs/implementation/lifecycle-v1.md` at that revision. The independent run here covers only the five listed regression tests. HTTP route integration and full multiresource atomicity are not established by this evidence.

The fix adds expected-state checks under the final storage write guard, publishes graph state under the graph-before-storage lock order, clamps access time, validates creation policy, maps typed conflict errors, and invalidates caches after attempted split writes. The original feedback/history and sweep phase/content commits still require the A1 canonical transaction/effects boundary; no claim that these separate writes are atomic. Full lifecycle schema v2, removal ledger, public receipts/batches and derived readiness remain dependent work.

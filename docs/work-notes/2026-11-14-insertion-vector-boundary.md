# 2026-11-14 — insertion vector correction

Review found that the insertion boundary in `bfb4c85` checked finite values and
dimensions but accepted zero norm. Actual engine probes for caller-supplied and
generated vectors both failed the no-mutation requirement (2 failures, 0 passes).
This is a reproduced defect, not a hypothetical risk or a provider experiment.

Source `7fa19f8927a9c1c6d46005044b37cae5eebca113` adds one shared f64 normalizer,
retains original request bits, stages normalized copies after replay/final
namespace checks, and distinguishes bad provider output from bad caller input.
Working advisory vectors and canonical vectors each transform once from raw
bytes. Huygens coordinated the shared helper; native profile work consumes it
without inferring provenance. Feynman's tombstone child consumes the correction
and preserves generic committed-receipt errors. No insertion PR number assigned.

Final offline Docker results: 645 library + 1 CLI passed; four doctests ignored;
fmt passed; clippy completed with warnings. The evidence bundle retains before
failures, reproducible source/commands, compact after results and hash manifest:
[vector evidence](../evidence/insertion-vector-boundary/README.md).

Main owns publication. No push/deployment, personal memory store, daemon,
provider credentials or networked runtime tests were used. The correction does
not enable native v2, whole-store CAS or claim power-loss durability. Future
integration must retain Observed consumers (`3b590fb`) and typed committed errors;
new unavailable daemon code -32008 must not replace namespace mismatch -32007.

Next A1 planning remains durable namespace purge and catalog manifests before
claiming a transitive all-writer barrier. Halley's freeze requirements are action
admission closure, children registered before parents complete, a fixed-point
work drain and a lease retained through coherent capture. Unresolved payload-free
batch items cannot be invented or reported as zero pending work.

Bounded next-writer proposal: [namespace purge, batch manifests and barrier prerequisites](../implementation/namespace-purge-next.md). This is planning only.

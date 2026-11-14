# Online derived completion (child of canonical effects)

Parent: `impl/canonical-effects` at `3f77ba3` (draft PR33). This child closes
that slice's hard-delete/Ghost-sweep/namespace-purge online availability gap.
It does not implement whole-store CAS, a generation-consistent read view, or
all-writer receipts/readiness.

## Publication and failure behavior

`DerivedCompletion::complete` rebuilds graph, vector, FTS, and entity indexes
from canonical metadata/edges, clears the record cache, then acknowledges the
existing durable effects. It does not allocate a new operation or generation.
A rebuild error retains the outbox and returns `CommittedEffectsPending` with
an operation ID; queries remain unavailable. Retry the coordinator, a manual
sweep, or the next scheduled sweep. `Recalld::complete_pending_effects` is an
explicit in-process repair entry. A poisoned/uncertain canonical commit still
requires reopen; this code never clears that sticky admission state.

Actual `Recalld` background sweeps attach all derived handles. The HTTP manual
sweep adapter is wired through `with_indexes` in `main`; missing handles fail
closed if a sweep leaves effects. MCP namespace purge completes publication
on both success and partial-destruction paths. Startup recovery remains the
fallback after process termination.

Lock order is shared store-publication gate (exclusive for completion), graph,
vector, FTS, entity, then storage. No storage guard crosses an await. The public
HTTP single/batch and MCP store pipelines hold the shared gate through their
last derived write; completion drains them before rebuilding. Existing graph
publishers and lifecycle writers retain graph -> storage order. Completion
holds storage through the final acknowledgement, so it cannot acknowledge a
later canonical effect that its rebuild did not include.

The current implementation scans all canonical records/edges and rebuilds all
views under these guards. It has O(records + edges) work and blocks graph/store
publication while rebuilding. This is a correctness baseline, not a throughput
optimization or an incremental per-channel watermark implementation. Mixed
vector dimensions unsupported by the current flat index fail closed.

## Consumer admission and cache correctness

MCP get/search/similar/list/stats/duplicate scan and HTTP corresponding content
reads check the effects stamp before and after their whole consumer body.
Advisory duplicate detection/hydration is fallible; pending effects never mean
"no duplicates". HTTP unavailable search errors map to 503. Cached health
reports are tied to the effects stamp, not just their TTL.

Record-cache handles used by production assembly/adapters bind weakly to their
canonical engine. Hits and read-through loads are validated against canonical
metadata before use. This rejects an old load/feedback operation that refills
cache after completion's invalidation. It costs a metadata read per hit;
replace it with the later shared versioned read view when all writers participate.
Standalone unbound test caches retain their existing in-memory behavior.

The effect stamp is an internal epoch, not a public canonical generation.
Entry/exit checks reject a crossed effect publication; they are not read leases
or a snapshot spanning arbitrary legacy metadata mutations. Direct graph/index
APIs and newly introduced consumer methods still require integration admission.
The store-publication lease is specifically a drain for derived rebuild, not
whole-store CAS or a universal mutation gate.

## Integration requirements

- Preserve latest lifecycle `745ca69` / atomic `e0187fc`: sampled-clock/policy
  projection, expected-state checks and graph-derived threshold revalidation
  remain inside graph -> storage. Keep main's private bridge plan and
  `delete_memory_with_edges` inside that same final guard. This child only adds
  completion after the sweep; it must not replace the guarded transition helper.
- Main's newer query methods `search_with_semantics`,
  `get_memory_with_access`, and `similar_with_access` need the same whole-body
  admission in both modes. Forward HTTP query options and scoped namespace
  arguments. Keep protected scope assertions inside final storage guards.
- Preserve metadata/control/effects/ledger validation and compaction's raw
  constructor-only recovery reads. This child changes no durable schema or
  outbox format. The removal-ledger hook belongs in the same canonical effects
  transaction, using that transaction's receipt.
- New public cache consumers must use the bound cache and whole-body admission;
  new store/index consumers must participate in the publication drain. A method
  absent from this parent is not certified by this branch's test results.

## Validation

All runtime checks use temporary stores in `recalld-ci:1.94`, toolchain 1.94.1,
`--network none --cpus 2 --memory 4g`, `CARGO_BUILD_JOBS=2`, and this worktree's
separate scratch Cargo/target. No home, credentials, daemon, or socket mounts.

Regression checks cover actual automatic Ghost sweep and manual sweep,
namespace purge returning with admission open, real vector-rebuild failure
retaining the original receipt followed by successful retry, pipeline drain
lock order, cached MCP get/similar admission and recovery, stale late cache
refills, unavailable duplicate advisories, and a child process exiting after
all online views publish but before acknowledgement followed by reopen.
These process-crash checks do **not** establish power-loss durability.

Validated source commit: `ad7b8f8d65e650bdfa0b57f757b2dd9b8e9cc555`. `cargo test --offline`: **550 passed**, no failures; **4 ignored doctests**. `cargo fmt --all -- --check`: passed. `cargo clippy --offline --all-targets`: passed with warnings (277 for lib-test, including inherited large-error/style warnings); this is not a warning-free claim. Logs remain in this worktree's untracked `.validation/tests.log` and `.validation/clippy.log`. No source changes followed these checks; this final validation entry is documentation only.

Independent lifecycle review: original graph-race synthetic probe fails on
`862ff28`; the return-type-adapted probe passes on atomic `e0187fc` (1 passed,
558 filtered), after source review of parent fix `745ca69` and its atomic helper
integration. It proves this intervening saving-edge case, not all-writer graph
atomicity. Evidence is being preserved separately in the research worktree.

Full A1/A3 storage/lifecycle contracts remain incomplete.

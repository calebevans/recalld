# Internal pipeline performance instrumentation

Source parent: frozen Observed PR58 `3b590fb`. The actual outer
prepare → hydrate → exposure → final admission interface is required to measure
true-mode service through publication and retain committed error classification.
No broad integration, namespace creation, insertion, or later maintenance join
is needed. Existing public `StageTimings` fields and response shapes are unchanged.

Opt in with the tracing target `recalld::performance=trace`; it is disabled by
default. Counters/timers follow the actual engine, canonical namespace batch,
vector dot products, FTS rows, graph diagnostics, storage blocking workers and
shared locks. Request spans cover engine search and REST/MCP search service;
root elapsed time includes exposure and final admission. Consecutive pipeline
intervals provide a residual and exact wall-time accounting; nested adapters and
locks are not additive to those intervals. Ordered JSON span stacks contain
internal parent/child trace IDs. This is not a public/A6 readiness envelope.

Canonical counts use the existing namespace read transaction and encoded values
actually consumed; they do not serialize records again or read foreign namespaces.
Vector counts include entries examined and actual dot products. FTS logical rows
are counted; SQLite-internal work is explicitly unknown. Existing v2 graph counts
are used; legacy graph examination counts remain explicitly unknown. Graph
expansion lock waits/holds are measured, while per-origin supersession lock splits
remain inside parent wall time to avoid per-candidate trace amplification.
No cache eligibility shortcuts, ranking, admission, clock policy, durability or
writer semantics change. No labels/text/memory IDs enter added events. Actual
committed receipt identity is an isolated trace field, never a metric label.

Disabled timers do not sample clocks or allocate, and Worker context capture
and queue timing use one enable decision. Errors at outer request boundaries
are explicit; a dropped inner timer reports `cancelled_error_or_unwound` rather
than inventing a more precise cause. The writer-I/O regression proves ordinary
sink I/O errors do not replace committed-not-ready classification or receipts.
Arbitrary subscriber panics are not caught or covered, including panic during
Timer/Timeline Drop while already unwinding.

## Validation and measurements

See [PROTOCOL.md](PROTOCOL.md) for the preregistered workload, absolute budgets,
paired seed orders, sink semantics and host-contention rule. Three actual-engine
regressions cover enabled/disabled result and canonical byte parity, exact counts,
redaction, nested trace IDs, disabled collection, cancellation/error outcomes,
true exposure/publication, and actual committed receipts under writer I/O failure.
Final candidate validation: 745 library tests and one CLI test passed in normal
parallel execution; four doctests were ignored. Offline fmt and clippy passed
(existing repository warnings remain), as did the final release probe build.
Enabled/disabled functional smokes agree on logical canonical state and result
hashes; each query interval preserves canonical file bytes. Raw build/test and
probe smoke failures remain in the worktree's private `.validation` directory.

The smoke probe is functional validation only. The completed frozen study is in
[REPORT.md](REPORT.md), [RESULTS.json](RESULTS.json), and the
[raw archive manifest](archives/MANIFEST.json). Enabled emission did not pass the
5% overhead gate; shared-host limitations and later integration gaps remain
explicit. No serialization/send/provider queue, write-fsync detail, compaction,
RIF pair-count or transitive readiness completion is claimed by this slice.

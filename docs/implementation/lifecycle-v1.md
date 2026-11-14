# Current-layout lifecycle foundation

This slice shares an injectable clock, namespace policy resolution and feedback
planning across the existing HTTP/MCP adapters and decay sweep. It retains the
v1 anchor and defaults. It does not introduce lifecycle v2, receipts or a schema.

Sweep writes re-read the record under the final storage write lock. Changes to
created/access timestamps, stability, phase, permastore or retained-summary
presence cause the plan to be skipped until a later sweep. This applies both to
strength materialization and destructive transitions, including Ghost deletion.
Graph publication holds the graph lock before acquiring the storage write lock
and publishes the canonical phase after a successful phase write. Cache entries
are invalidated even when a later transition write reports failure.

HTTP access timestamps are clamped to the record's creation/access timestamps.
Terminal feedback errors become HTTP 409 CONFLICT; missing records remain 404.
The MCP bridge retains its existing NotFound mapping for unavailable content.
HTTP and MCP namespace creation adapters validate supplied lifecycle settings
through the same resolver before creating a namespace. Existing invalid persisted
policies fail closed; no automatic migration is supplied.

The follow-up [atomic legacy lifecycle slice](atomic-lifecycle-v1.md) couples
feedback state/history and sweep phase/strength/content in one metadata transaction.
It supersedes the original split-write limitation for these operations. The lock
and snapshot check prevent the reproduced in-process stale-plan overwrite; they do
not provide public CAS, versioned plans, durable lifecycle receipts or retry safety.
Hard deletion still does not
clean up external vector, FTS and entity indexes; canonical hydration drops their
orphan candidates. No whole-store readiness or convergence guarantee is made.

Tests use fixed clocks and real isolated temporary storage. The reviewer regressions
insert feedback between actual sweep planning and application without sleeps,
check the graph after two destructive sweeps, and expose a backward HTTP clock.
Additional tests reject each stale snapshot field across Full/Summary/Ghost,
reject invalid creation policies before persistence, and exercise HTTP error
mapping using real adapter errors (not an HTTP server integration test).

## Lock-order audit

Audited the existing writer call sites in the lifecycle parent: HTTP create/delete/
access/namespace adapters, MCP store/delete/batch-delete/namespace-purge adapters,
autolink and explicit links, index cleanup, search access recording, startup and
shutdown, and benchmark storage/graph updates. Their storage guards are scoped to
blocking closures or synchronous blocks and are released before graph acquisition;
autolink and explicit links also release their graph guard before persistence.
Startup constructs a local unshared graph under a storage read lock. None of these
inspected writers waits for the shared graph while holding storage. Search/RIF
graph readers acquire no storage guard while they hold the graph.

For the new sweep nested locking, the order is graph then storage; feedback takes
storage only. The canonical-effects agent has been notified and confirmed the
same graph-first acquisition order, with durable mutation before graph publication.
The purge overlap regression deliberately holds graph, observes canonical purge
completion and a released storage lock, completes feedback, then releases graph
and verifies purge and stale sweep finish. This is bounded coverage of those
paths, not a proof of deadlock freedom or publication atomicity for all operations.

## Published graph revalidation (P1)

The independent 4.5-day/S=1 reproducer failed on this original lifecycle parent,
not only the later atomic slice: a committed and published weight-1 associative
edge raised effective strength from 0.6974858 to 0.7172529 across the 0.7 threshold,
but the old transition still stripped content. The regression now passes.

Each evaluation retains its sampled time, resolved policy and activation settings.
Under graph -> storage locks, both strength-only writes and destructive transitions
recheck the record/namespace and recompute projection from the held graph using
those original inputs. Transition eligibility is rechecked before Full -> Summary,
Summary -> Ghost, or Ghost deletion. A newly saving bonus skips the transition;
a strength-only evaluation that newly crosses downward materializes current
strength and leaves destructive reconsideration to the next sweep. No clock is
sampled again. Tests also cover Summary/Ghost rescue, subsequent loss of the
saving neighbor strength, and strength-only bonus growth/shrink with nondefault
multiplier and cap. This covers current published graph state, not a promise that
all separate canonical-edge/publication writers form one snapshot; that boundary
remains owned by canonical effects integration.

Offline Docker validation of this parent: 515 tests passed, four existing doctests
ignored; formatting passed and Clippy completed with warnings. The original
split-write limitation above remains on this parent until the atomic child lands.

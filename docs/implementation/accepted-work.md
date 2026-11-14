# Accepted work and metadata capture

Status: implementation in progress on `impl/accepted-work`, based on the frozen
purge runtime `f6e4d9263fe2d96fca16282eed47b1251ad5398b`. No validation result is
claimed yet. The A5 exclusive measurement hold prohibits Docker, build, test and
cache-copy work until explicit release. Previous public/evidence branches remain
unchanged, including PR64 tail `0dd9954d0b454726c333f7635300e5547790dd45`.

## Implemented boundary, pending validation

`MetadataStore` owns one `WorkTracker`. Clones refer to that same owned core;
matching store UUIDs do not authorize a ticket from another core. Tickets are
non-cloneable, non-serializable, and identify in-process accepted work rather than
canonical operations or generations. Register roots before enqueue/staging and
register children while their parent is active. An accepted child can continue
and create descendants after its parent finishes. A finished parent cannot admit
another child. Dropping an unfinished ticket retains unresolved work until owner
reopen; it never manufactures successful drain from cancellation.

`close_and_drain` closes root admission and waits for the transitive pending set
to empty. Accepted children remain admissible during closing. Timeout reopens
admission without deleting pending/unresolved work. A concurrent failed closer
cannot reopen another closer's lease. A successful lease keeps admission closed
until dropped. This is an in-process admission primitive, not CONTRACTS readiness
or a generation-targeted barrier.

Every existing metadata mutation guard registers before waiting for publication.
Normal known returns release publication before finishing that ticket. Panic or
an unavailable owner retains unresolved work. Insertion, tombstone, and each purge
step hold the same guard through synchronous canonical effects and their durable
acknowledgement. The acknowledgement requires a guard belonging to the same core.
Recovery and purge may bypass only their existing narrow availability conditions;
they cannot bypass closed root admission.

The actual `DerivedCompletion` insertion and forget paths accept a root before
their first await/enqueue. Worker tickets and derived completion carry explicit
parentage. Completion passes its ticket through purge resume and derived
acknowledgement. Async cancellation leaves unresolved work; blocking workers that
continue after cancellation still retain their own ticket. Known execution
failure can finish a ticket only after the worker has returned; durable pending
effects remain a separate obstacle to capture. Existing committed-error receipt
wrappers are retained.

`freeze_metadata` consumes a drain lease, tries the metadata publication lock,
checks handle availability and validates durable effects/purge state, then pins
the actual canonical `GenerationRef`. It rejects foreign-core leases, pending
manifests, stale dirty flags with nonempty durable effects, and active purge rows.
It never substitutes `effects_epoch` for a generation. Its deadline is checked
before and after validation: late success is rejected, but validation scans and
blocking OS I/O are **not bounded** by those checks. No hard 30-second completion
claim is made.

## Source review and test plan

Draft tests cover accepted children/grandchildren during closing; a parent dropped
while its child runs; a consumed parent rejected as a new child's authority;
same-UUID foreign cores; timeout followed by concurrent reclose; spurious wakes,
expired deadlines and sequence overflow; actual mutation/recovery/purge gates;
panic/unknown completion; durable pending state despite stale volatile flags;
and metadata publication contention.

Actual coordinator regressions hold the derived publication or graph lock, queue
insertion/purge completion, close admission, then release the lock. The expected
result is completion through child tickets, durable acknowledgements, zero
pending work and successful metadata capture. A cancelled queued forget must
leave unresolved work without a canonical mutation. These tests have not run.

After release: isolated Rust 1.94.1 formatting, focused tracker/coordinator tests,
then the full library/CLI suite and Clippy using a fresh explicit source snapshot
and a private copied cache. Use network-none Docker, two CPUs, 4 GiB RAM, and no
home/credential/socket mounts. Preserve failures and exact revised-source hashes.

## Required joins and remaining A6 work

This does not establish whole-store readiness or permit a snapshot/reset. Remaining
writer coverage includes public queue entry points outside this coordinator,
maintenance/lifecycle tasks, graph/autolink work, file preparation/compaction,
native-v2 writers and ordered batches. Feynman's raw-writer restrictions and
maintenance joins, Curie's batch manifests/pending-item API, and Pascal's native
owned-core paths must consume this tracker rather than create competing owners.
Pending batches requiring the original resubmitted payload must remain nonzero;
they cannot be auto-executed from payload-free receipts.

Before exposing A1/A6 readiness, all derived views need real generation publication
and every accepted descendant must be registered. The target must reject foreign
lineage. Add cooperative monotonic deadline checks or bounded already-validated
pending-state queries for long loops without weakening startup validation. Blocking
OS I/O still prevents an unconditional hard wall-time guarantee.

Whole-state freeze then requires stopped action admission, complete transitive
drain, canonical/policy consistency, frozen A3 clock/config/RNG and behavioral
caches, plus validated files and logical hashes. Reset must stop old workers and
construct a fresh isolated root from a verified manifest, with actual restored
clock/config/RNG/index state. A copied unchecked directory is not a snapshot.

The ID-source audit is a separate reset prerequisite. `MemoryId::new` calls
`Uuid::now_v7`; canonical insertion calls it when the request has no ID.
`control::initialize` generates a store UUID, `control::record_receipt` generates
operation UUIDs, and purge reserves child UUIDs. These currently use actual
randomness/time independently of A3's clock. Pascal confirmed the native writer
uses these same owned-core paths. ManualClock alone therefore cannot establish
deterministic replay. Coordinate an owned, scoped ID/RNG source with A1/A3, or
prove an explicit bijection that preserves references, receipts and tie-sensitive
ordering. Snapshot/reset must capture its actual state; no process-global RNG
override or private fixed-UUID ingestion escape is acceptable. Repeated equal
score fixtures must exercise the actual ID-order-sensitive path.

Following these controls, A6 still requires validated versioned manifests/schemas,
offline replay misses that fail closed, source-dataset adapter and scorer golden
contracts, and scripted chronological fixtures. Native/public-v2, coherent read
views, public schema faults and repeated controlled replay remain actual
prerequisites. Synthetic vectors do not establish semantic quality. No external
models, paid calls or training are authorized; human-judge calibration remains an
external gate. Neither this slice nor earlier nine-episode conformance proves
full A6 completion, a release gate, or performance acceptance.

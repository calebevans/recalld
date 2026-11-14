# Next A6 control boundary

This is a prerequisite and interface plan, not an implemented barrier or snapshot.
The consumer join is a legacy-v1 receipt/error correctness prerequisite. Repeating
its public episodes does not complete the controls below. A5 may plan against the
frozen consumer interfaces while validation runs; no runtime instrumentation or
public-v2 feature is activated by this plan.

## Minimal dependencies and order

1. Use the validated consumer join with published by-ID `74c9bfb` as the consumer
   parent. Keep the minimal ownership guard. Do not import the integration branch.
2. Join exact frozen A1/A3 native-writer children from Curie/Pascal only when their
   receipt, pending-work, schema and publication interfaces are ready. Every writer
   reachable by actor actions must participate. Preserve #45 pending-effect
   preflight and later restored-origin/catalog receipt validation in their actual
   owners. Their final hashes are not available here and must not be invented.
3. Consume A1's coherent snapshot/manifest/inspection implementation. A physical
   copy or offline read-only open alone is insufficient. If manifest/catalog
   children are required by those concrete interfaces, include those actual
   minimal parents explicitly before implementation.
4. Add the isolated evaluator control adapter, then public schema/fault fixtures.
   Missing native writer, clock injection, RNG ownership or transitive drain
   invalidates the controlled run at preflight; it does not select a weaker
   comparison-only fallback.

Native lifecycle capture/commit remains on MetadataStore's guarded primitives.
A standalone offline CLI may own a thin session wrapper; a live engine must reuse
its already-owned core, never open a second session per query. Capture-generation
metadata cannot be relabeled as whole-query readGeneration without an actual A1
coherent read view.

## Controls and proof obligations

The actor continues to use production MCP/HTTP parsing and handlers. An inherited
private control descriptor belongs only to the supervisor and isolated runtime;
none of its commands appear in actor tool discovery. No personal paths, external
provider access, ordinary production restore or alternate ingestion path.

- `barrier(GenerationRef, deadline)`: A1 owns the work tracker and readiness shape.
  Register accepted work before scheduling; register children before completing
  their parents. Drain the transitive closure through the target generation,
  including graph/index/lifecycle publication. Reject foreign lineage. Use a
  30-second monotonic deadline. Return actual committed/derived generations,
  pending work and readiness. The current `effects_stamp` is an admission epoch,
  not a canonical generation or a whole-store barrier.
- `snapshot()`: stop action admission, drain all accepted work, freeze the shared
  manual clock and require zero pending work. Under coherent A1 ownership capture
  records, text, vectors, edges, namespaces, policies, receipts/manifests, removal
  ledger, index versions/rebuild inputs, RNG state and behavioral cache state or
  an explicitly enforced cache bypass. Return an immutable manifest and logical
  hash at the captured generation. Never serialize an opaque executor queue.
- `reset(snapshot)`: terminate prior workers and close ownership; verify manifest
  and all inputs, create a fresh inaccessible synthetic root, restore clock,
  configuration and RNG, then rebuild/barrier/hash-check before admitting actions.
  No overlay of a live root. Preserve fixture lineage only in this isolated retry
  domain; ordinary production restore retains its different lineage rules.
- `advance(delta_ms)`: one shared A3 ManualClock, initially
  `2026-01-01T00:00:00Z`; reject negative/overflow input. All lifecycle consumers
  use that instance. Advancing time schedules no implicit background work.
- `sweep()`: invoke the real A3 planner and commit path, then barrier. Pin policy
  revisions and sweep order, including one destructive phase per explicit sweep.
- `inspect()`: evaluator-only canonical logical state and readiness. Stable key
  order covers policies, lifecycle, receipts and ledger as well as content. An
  explicit UUID bijection may normalize identities, never erase references,
  receipt identity or tie-sensitive order. RNG/ID generation that can alter ranks
  must be controllable; a seed declaration alone is not evidence.

A frozen manifest must identify source/build/container/lock hashes, independent
API/schema/retrieval/lifecycle versions, policy revisions, actual GenerationRefs,
clock and sweep schedule, every RNG source/state, cache mode, snapshot hashes,
embedding profile/input hashes and trace/scorer versions. Refuse absent or unknown
required controls before starting a scored episode.

## Internal trace boundary for A5

Keep one optional internal request span across preparation, hydration, exposure
and final admission. Preserve original requested recordAccess. Child spans may
measure actual queue wait, lock wait/hold, canonical read/decode/filter, graph,
FTS, rank/supersession, commit and publication. Use monotonic durations and explicit
parent/child identity so nested durations are not double-counted. Attribute
committed storeId/operationId/generation only from the real WriteReceipt. Unknown
readGeneration/readiness stays absent or explicitly unknown. The internal
Observed<T> retains receipts through final admission independently of tracing;
telemetry failure cannot change payload, mutation, retry or receipt semantics.
A no-op sink/default-off instrumentation and its overhead evidence belong to A5.

## Required controlled fixtures

Run three fresh reset/restart replays from one verified snapshot. Assert identical
normalized logical hashes and declared deterministic ranks. Delayed child work
must block barrier; timeout, postcommit publication failure and unknown completion
invalidate the episode. Compare canonical records/history/receipts/graph/ledger/
policy and pending work before and after repeated false-mode reads, including
warm-cache controls. Do not replace this check with a sleep or physical-file hash.

The public schema/fault matrix must traverse real serializers and handlers:
unsupported schema/features; schema/lifecycle mismatch; original true intent on
an unsupported writer; malformed quality/dimension/scope; missing/foreign targets;
terminal UUID reuse; keyed replay/conflict across restart; partial/unknown batch
outcomes; deletion/correction after rebuild; postcommit not-ready receipts. Test
policy changes between preparation and the final transaction. Preserve every
failed raw response and resulting canonical state. No public-v2 claim until the
actual native serving path and every dependent writer are ready.

# Awaited legacy observation consumers

## Parent and scope

Branch `impl/awaited-observation` starts at PR43 `8b013e7`, adds standalone
representable-timestamp fix `2eed80b`, and merges PR24 `5177642` at `4297427`.
It does not inherit the combined integration or core-join branch. PR43's durable
metadata transactions remain the authority; this slice connects actual legacy
query, MCP, daemon and HTTP callers. No explicit-feedback-v2 writer, keyed public
transport, generation-consistent read view, or public whole-store CAS is enabled.

## Actual consumers

`decay::observation::record_exposures(storage, cache, accesses, sampled_at)` returns
ordered receipts. It resolves canonical namespaces under the storage write lock,
deduplicates UUIDs in first occurrence order, and lets DirectRetrieval dominate an
additional AssociativeRetrieval occurrence. It commits one existing A1 transaction
per namespace in ascending namespace-ID order. Empty results do not create a
receipt or advance generation. A failure in one transaction rolls back that
namespace; an earlier namespace's commit remains committed and is reported.

`StorageAccessRecorder` calls this helper with the instance clock and real cache.
QueryEngine direct get, search and similar now await their final recorder. There
is no detached exposure task. `record_exposures` and `record_similar_accesses` are
shared hooks for consumers that finish shaping results outside the core.

HTTP get hydrates text before observation. HTTP search defers core recording,
retains result access classification, hydrates text and then records the final
set. Similarity uses `similar_filtered_with_access(id, k, min_score, record_access)`
(shared signature coordinated with Halley): source/score/visibility filtering
precedes recording. REST invokes selection with recording deferred, hydrates
actual canonical fields (including difficulty and raw decay strength) plus
fallible full text, and calls `record_similar_accesses` once. MCP similarity has
no further filtering after the shared helper.

MCP recall defers core recording and hydrates read-only, then records the union
of final hits and eligible extra neighbors once. Foreign namespace and terminal
neighbors are excluded. `SearchInput.include_neighbors` defaults true for direct
and daemon search callers; the compact tool parser sets false because compact
responses omit graph neighbors. Thus compact calls do not expose or observe those
omitted neighbors. MCP/daemon get default/true now records one durable exposure;
false bypasses the transaction. Tombstone diagnostic responses remain nonobserving.
Tool hints reflect the mutating default. Original request flags survive the
internal read-only selection: only the original true flag invokes the final hook.

Both HTTP and MCP feedback call the shared legacy adapter. It keeps existing
quality/content/policy validation, uses `record_legacy_feedback` for the single
canonical/history/receipt commit, invalidates the record cache, and returns current
canonical numerical fields. No result text or full record is copied into receipts.
Successful legacy response envelopes stay unchanged; receipts are durable internally.

## Failure and publication boundaries

The commit and cache invalidation execute on the same blocking worker under
`storage.write`; cancellation of the request does not cancel a running worker
between those steps. No graph lock is acquired or awaited while holding storage.
The established graph -> storage -> metadata order is preserved. Cache invalidation
normally cannot fail. The tested synthetic panic path marks storage unavailable
and returns committed receipts with `ready=false` and `INDEX_NOT_READY`; reopen
rebuilds the in-memory view. This is not a power-loss experiment.

`ObservationError` carries `commitState`, `ready=false`, all completed receipts,
an uncertain operation ID when supplied by A1, and the failure. Unknown worker
completion is explicitly unknown, not a promise that retrying is safe. HTTP
returns 503 with the structured observation error and no result payload; MCP and
daemon preserve its JSON detail through existing Storage/PartiallyApplied errors.
A later namespace failure never returns an ordinary success or retained result
text. Invalidating preceding commits still occurs before returning that failure.
Unkeyed legacy retries remain new operations; this does not claim exactly-once
transport delivery or full contract batch envelopes.

This minimal parent only publishes affected record-cache invalidations. It does
not include PR39's complete derived admission, late-cache-fill validation, effects
stamps, or whole-pipeline barrier. A newer independent mutation between selection
and observation can still change what is visible. Final transaction validation
checks identity, namespace and terminal state, but this is not a coherent read
snapshot. Joining a post-exposure admission stamp failure must retain the committed
receipts rather than replacing it with a generic retryable error. This remaining
join work is not claimed completed here.

## Required source adaptations in the real join

These are actual glue requirements, not pure merge declarations:

- Retain main's V2 `search_with_semantics` wrappers, scoped get/similar assertions,
  original recordAccess intent, and online cache/admission guards. A final scope
  assertion belongs under the write/publication guard; the minimal parent has no
  optional namespace assertion API to implement here.
- Hume's agreed API is
  `prepare_similar_filtered(id, k, min_score, requested_access) -> Result<Vec<PipelineSearchResult>>`.
  Its shared admitted core receives original intent separately from
  `defer_recording=true`. Replace this parent's internal-false selection with that
  prepared path; REST passes the original flag, hydrates, then records once. MCP
  recall and HTTP search need the analogous prepared-search path. Never interpret
  internal deferred recording as permission to bypass policy admission.
- After joining schema, `metadata::observation::read_namespace` must use
  `NamespaceConfig::decode`, not plain serde decoding. Reject
  `ExplicitFeedbackV2` under the final metadata transaction/publication guard in
  both observation writers until the correct policy-specific writer exists.
  Checking only `ctx.contract_version`, or a preflight check outside that guard,
  is insufficient. Preserve all graph/text/schema/catalog receipt variants in
  control validation when reconciling the minimal-parent exhaustive match.
- Retain Halley's canonical source/result visibility, namespace resolution,
  embedding validation and filtered-similarity implementation. Preserve the final
  recorder hook and original requested mode; no duplicate core/outer events.
- Preserve lifecycle's final graph/storage expected-state and policy checks,
  transaction removal-ledger hook, selected-text disposable startup validation,
  and namespace-filtered entity postings. None of these independent core-join
  changes are reimplemented or removed by this branch.

## Validation

Runtime tests use temp stores only in offline `recalld-ci:1.94`, toolchain 1.94.1,
2 build jobs, 2 CPUs and 4 GiB, with dedicated `.validation/cargo` and target.
No home, credentials, daemon sockets or personal data are mounted. Validation
outputs and caches are untracked and excluded from the commits.

Focused additions cover namespace ordering/generation once per batch; duplicate
classification; empty no-op; atomic same-namespace failure; prior receipts on later
namespace failure; cache invalidation; actual feedback receipt persistence after
reopen; synthetic post-commit publication failure and fail-closed reopening;
actual compact/full MCP exposed unions; filtered REST/MCP similarity; false-mode
immutability; and actual transport errors with no success payload. Existing socket
daemon tests exercise true/default and forced-false aliases with the new consumers.

Final validation: `cargo test --offline --all-features`: **583 passed, 0 failed**,
four ignored doctests. `cargo fmt --all -- --check` passed. Offline
`cargo clippy --all-features --all-targets` finished successfully with warnings
(292 lib-test warnings, including existing large-error/style warnings).
Logs: `.validation/consumer-tests.log` and `.validation/consumer-clippy.log`.
Standalone timestamp follow-up evidence remains in `.validation/timestamp-*.log`.
The tested source commit is pinned in the documentation-only handoff below.

Validated source: `357c780907e7b2302b969feabf86d053849266a2`.
The following handoff commit changes this document only; source/tests are unchanged.

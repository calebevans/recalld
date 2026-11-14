# Canonical batches and accepted work

This join consumes canonical batches `e51cb67ad5442c99c4b7d9c585dfed91d7448ba1`
and published accepted-work PR73 `05cd2542d8478ed395e062a87ec9d3d83f4e8fcc`.
The separate validated batch/PR68 admission correction `9d8b295` (runtime
`98dbef6`, actual PR68 `b2923d1`) is also consumed. Its shared validator remains
inside the work-aware reserved/single adapter, before canonical conversion and
provider work. Historical caller hashes and receipt replay are preserved.

The actual MCP/daemon batch runner accepts one owned-core root before its first
await, reservation, provider work or child operation. Reservation, pre-provider
child replay lookup, insertion/tombstone workers, runtime rejection bookkeeping,
and derived completion carry descendants of that root. Optional associative,
entity and temporal link publication also carry the parent through canonical
edge effects, graph/cache publication and durable acknowledgement. Existing
standalone signatures remain wrappers without a batch ancestor. The shared lock
order remains graph -> storage -> metadata publication; draining never holds an
outer storage guard.

Known returns may finish execution tickets. Cancellation, panic and an unknown
commit on an unavailable owner retain unresolved tickets; a detached blocking
worker has its own accepted ticket. Finishing execution does not finalize a
payload-free reserved batch item. A provider failure may end execution while
leaving canonical `NotAttempted` items that require matching caller resubmission.

`freeze_metadata` checks that durable pending count while holding metadata
publication and the admission lease. Reopen resets volatile tickets but preserves
the canonical obstacle; it cannot invent caller input or clear an unresolved
slot. Completed and final-error manifests do not block capture. Canonical batch
startup validation is unchanged: partial/unknown schema, lineage, receipt,
reserved-child identity and ordered-state proofs still run before writable open.

The pending scan is O(lifetime manifests + their items), with no second index.
`pending_batch_items_checked` invokes a supplied check before reading, between
rows and after scanning. Capture uses its existing owned-core deadline check.
This is cooperative cancellation between manifests, not preemption within redb,
decode or OS I/O. Other existing control/effects/purge validation still has the
outer before/after deadline checks. No hard OS or 30-second completion bound.

The native effect tail keeps the actual `&MutationGuard` and final optional
`BatchChild`; Pascal's native path passes `None` at its later join. This code
adds no native serving activation, public whole-store CAS, contract2, shared
readiness, generation-targeted barrier, whole-core snapshot or frozen clock/RNG
claim. A4 batch result version 1 only negotiates the existing envelope. Feynman
owns the separate ancestry-aware offline preservation consumer, now frozen at
`3b0aded`; it is not an ancestor of this tracker slice.

Final combined validation passed **43 batch + 14 tracker/metadata + 9 coordinator
focused checks, 744 library tests and 1 CLI test**. Four doctests were ignored;
format passed and Clippy completed with warnings. Exact hashes and raw logs are
in the [evidence package](../evidence/batch-accepted-work/README.md).
The initial pre-correction-parent focused join passed 39 tests, including provider/graph and tombstone
completion during closing, runtime rejection bookkeeping, cancellation/reopen,
unknown receipt identities, actual framed daemon rejection, and durable pending
checks. Two development failures were incorrect test assertions: safe cache
misses were initially disallowed, then outgoing counts were mistaken for
incident counts. Neither is presented as a production failure or power-loss test.

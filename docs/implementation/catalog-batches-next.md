# Next slice: actual durable insertion/forget batches

Plan following frozen purge runtime `f6e4d92`. Implement as a separate child;
this document does not claim batch support exists. Minimal parent is purge
(`dad8465` + `d4f756c` underneath), which already contains actual insertion,
tombstone, catalog receipts and the reserved receipt primitive. Add a further
parent only for a consumed API: native/profile or broad integration ancestry is
not required for the legacy batch writer. Feynman owns offline compatibility;
Halley owns accepted-work/freeze tracking. No competing journal or tracker.

## Actual consumer change

Today `mcp/tools.rs::handle_store_memories` parses/executes a loop of single
`store_memory` calls. The daemon sees separate writes and cannot persist one
batch identity. `McpStorageAdapter::delete_memories` returns flags or a partial
error string; its RPC likewise has no durable whole-batch result.

Introduce one typed bridge batch call and daemon request/reply for the complete
ordered insertion or forget request. The actual MCP batch tools delegate once.
Retain legacy single methods and compatibility flag projections where required;
never use those lossy projections for the new batch result. Do not change
Observed read wrappers, native-v2 policy or independent profile semantics.
Legacy optional lineage/key fields opt into retry without advertising v2.

Proposed internal boundary:

```rust
reserve_batch(catalog_context, normalized_item_intents) -> BatchProgress
resume_batch(catalog_context, matching_full_request, publisher) -> BatchOutcome
lookup_batch(store_id, batch_operation_id) -> BatchProgress
```

`BatchProgress` is payload-free: manifest version, immutable parent receipt,
ordered item hashes, frozen resolved scopes/default-policy identity, reserved
child IDs, and indexed states. `BatchOutcome` carries every original index,
resolved scope where known, `ok|error|notAttempted|unknown`, receipt or structured
error, readiness and retryability. No aggregate committed generation.

## One canonical receipt mechanism

Use the existing `canonical_control`, `mutation_receipts`, `mutation_keys` and
`record_reserved_receipt` transaction primitive. Add only versioned canonical
batch manifest/progress rows; do not add another receipt journal. Reservation
commits the catalog-scoped parent receipt and all ordered child IDs once.
Children remain namespace-scoped. A private reservation token identifies the
manifest/slot/hash/scope/child ID; its final transaction rechecks all of them.
No public arbitrary operation-ID override.

Extend the existing insertion/tombstone transaction tail with that private
reservation and same-transaction manifest outcome hook. Metadata, removal
ledger/effects, child receipt and successful item state commit together. Current
provider/staging boundary remains: replay/reconciliation before provider work
where possible and before UUID/vector/text allocation; shape/profile checks
and final canonical validation still precede staging/commit. Publish all views
before reporting ready. Keep current-child committed/unknown errors unchanged.

## Request identity, replay and partial progress

Outer array/type/size/key/lineage errors fail before reservation. Item validation
failures become durable indexed errors without cancelling later valid items.
Persist bounded error codes/field paths, not rejected text/vector payloads.

Compare the complete ordered normalized request against the existing manifest
**before** CAS or fresh namespace/default resolution. Fixed defaults expand by
contract version. Mutable policy defaults and resolved namespace IDs freeze at
reservation with their policy identity; retry validates the caller's original
intent and those saved expansions, never today's changed defaults. Explicit
absence versus supplied values retains its insertion meaning; supplied vector
bits remain exact before normalization, including signed zero. Generated UUIDs,
sampled time and provider output are excluded. Invalid-item fingerprints also
retain request identity through deterministic canonical encoding, without
storing payloads. Changed payload, order, scope intent, explicit values or batch
kind conflicts across the catalog key even when some children already committed.

A matching replay returns finalized success/error outcomes unchanged. Reconcile
an unknown slot by its reserved child ID before any attempt. If committed, retain
its original UUID/receipt and retry publication; if absent, only a matching full
caller resubmission can supply the pending payload. Never invent inputs from a
hash. `notAttempted`/`unknown` remain unresolved; finalized errors require a new
key to attempt again. Check the batch's initial expected generation once at
reservation, not after each of its own children; keyed replay precedes that CAS.
No public whole-store CAS claim follows while other writers are incomplete.

Process children in input order. Independent final errors do not stop later
items; service-wide failures leave the rest `notAttempted`. Counts satisfy
`total=succeeded+failed+unresolved`; committed but unready children still count as
succeeded and retain `ready=false`. MCP sets isError for every non-all-ready
outcome. Preserve structured statuses through daemon encoding/decoding and
connection failure; no blanket safe retry or string-only loss of receipts.

## Required tests and joins

Actual MCP tool and real daemon RPC tests must cover mixed valid/invalid items,
all-final errors, first/later committed-but-unready failures, stop with
notAttempted remainder, duplicate IDs and input order. Inject faults before/after
manifest and each child commit, after publication and before response; reopen
and resubmit to prove no duplicate UUID/slot/text/history/ledger mutation. Assert
exact child identity on committed/unknown errors and all earlier receipts in
partial outcomes. Matching replay must precede stale CAS; changed payload/order/
raw vector bits/scope/default intent must fail without mutation or provider work.
Concurrent identical retries must share manifest/children; cross-kind catalog
keys conflict. No payload text/embeddings in any canonical receipt/manifest row.

New tables require strict startup validators and explicit offline preserve or
reject handling, coordinated with Feynman before wiring export/restore. An
unfinished payload-free batch is reported pending to Halley's transitive drain,
not auto-completed during freeze. Register root work before enqueue and reserved
children before parent completion when the actual tracker parent is consumed.
No native-v2 serving, complete writer participation or A6 readiness claim.

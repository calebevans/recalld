# Awaited observations through final transport admission

This joins awaited consumers #47 (`9ebe082`) with Hume's by-ID source
`6f29a74`. The latter already contains the required #45 core, #46 strength,
#30 namespace batching and #51 similarity parents. It does not import the
full integration branch or catalog/restore #48/#50. The user-authorized minimal
#15 ownership fix `2788645` is also a parent; OfflineSnapshot retains its new
owned lock guard at the existing field position.

The internal recorder returns the original `Vec<WriteReceipt>`. Private
`Observed<T>` retains those receipts until the outermost effects-stamp check
has passed. A post-commit admission error or changed stamp returns the existing
observation error with `commitState=committed`, `ready=false`, `INDEX_NOT_READY`
and the committed receipts. It contains no selected payload. Pre-commit errors,
unknown commit outcomes and preceding namespace receipts retain their original
classification. Successful legacy response envelopes are unchanged.

HTTP and MCP search prepare with the original requested `recordAccess` value;
preparation defers writing rather than pretending the request was false. HTTP
hydrates canonical metadata and text before exposure. MCP records its hydrated
hit/eligible-neighbor union, excluding neighbors omitted by compact responses.
Similarity filters its source, threshold and limit before exposure. Framed daemon
RPC consumes the same MCP adapters and preserves the failure detail.

Final observation transactions decode namespace schemas, assert the prepared
namespace and by-ID source, and check terminal UUID evidence under the metadata
publication guard. Feedback also checks its namespace in the transaction.
Historical keyed replay remains before fresh namespace policy and clock checks.
Explicit-policy writes are rejected. Other schema-bearing records/namespaces are
also rejected by this legacy writer: it cannot update native lifecycle fields
and must not serialize inconsistent legacy anchors into a schema-v2 record.
Native writers and ordinary schema-v2 serving remain separate dependencies.

Instance-local test probes run after a real canonical exposure commit and cache
publication. They either close admission or change the effects epoch. Actual
HTTP routers, MCP tool dispatch and Unix-socket framed daemon calls cover get,
search and similarity, false-mode controls, empty similarity and reopen. Each
error's receipt is compared with persisted operation lookup, and each returned
UUID has exactly one event. Separate regressions cover schema decode/policy
rejection, final source/scope assertions and pre-exposure text hydration errors.
The probes exist only under `cfg(test)`; there is no production fault switch.

## Integration seams

Preserve #45 pending-effect canonical preflight. When joining later #48/#50,
preserve their permitted-origin/restored-ancestry and catalog-scope receipt
validation. This branch cannot validate those absent variants and does not add
a blanket historical-receipt rejection policy.

## Validation and limits

Validation is in progress; exact frozen source, commands, image and artifacts
will be recorded after the full run. Development failed-request logs are retained.
The public replay remains legacy-v1 diagnostic evidence, not an A1 coherent
snapshot, barrier guarantee, release gate or completed A6. Receipts retained
internally do not introduce a public keyed query/replay protocol. Real wall-clock
public episodes do not establish controlled-clock determinism.

# Explicit read-only retrieval in v1

`recordAccess: false` is supported by MCP `recall_memories`, `get_memory` and
`find_similar_memories`, HTTP `POST /search`, `POST /similar/{id}`, and HTTP
`GET /memories/{id}?recordAccess=false`. Omission defaults to true. JSON values
must be booleans; the HTTP GET query parameter accepts `true` or `false`.

False mode reads canonical records without using or filling behavioral record
caches. Candidate batches, graph expansion and supersedes hydration carry the
same option. It schedules no access recorder, invokes no reinforcement, persists
no RIF effects and mutates no retrieval index. Existing RIF calculations still
adjust the response in memory; diagnostic metrics may change. The system clock
is also injected into the search access recorder for controlled tests.

## Compatibility

True/omitted retains each path's v1 behavior:

| Path | Existing true/omitted behavior |
| --- | --- |
| MCP/daemon get | Nonobserving canonical read |
| HTTP get | Access recorded before response; no feedback reinforcement |
| Recall, QueryEngine get/similar, MCP/daemon similarity | Async exposure recording; no synchronous completion guarantee |

This is an additive false-mode contract, not v2 lifecycle parity. Existing ranking,
filters and true-mode timestamps/strength behavior are otherwise retained. There
is no v2 default, durable receipt, global completion barrier or whole-store
readiness claim. Concurrent writers/background sweeps can still change state.

For remote MCP, false selects private daemon methods `search_read_only`,
`get_memory_read_only` and `find_similar_read_only`. These force false even if the
payload says true. An older daemon rejects these unknown methods instead of
silently ignoring a boolean on a legacy mutating method. The client never falls
back to the legacy method. True requests keep the existing method names. The
read-only aliases are retry-safe; legacy observing search/similarity are not.

The existing HTTP adapter does not support vector-only pipeline queries: both
HTTP embedding-only search and similarity currently fail with EmptyQuery. This
slice propagates false and bypasses the similarity source cache even on that
error path; it does not claim successful HTTP vector retrieval. Text HTTP search,
HTTP get and MCP/daemon similarity have successful real-adapter tests.

## Validation scope

Tests use real temporary storage, indexes, graph and adapters, fixed embedding
vectors and clocks, MCP tool dispatch, axum requests, and isolated Unix sockets
with the actual daemon connection handler. No personal daemon lifecycle runs.
A separate old-handler simulator checks the exact six-method sequence (three
rejected read-only aliases, three legacy methods), detecting silent fallback;
it is not an old daemon binary. True-mode tests await a test recorder notification
for that operation, not a production barrier. False-mode checks compare records,
graph nodes, vectors, FTS results and persisted edges, plus a bounded quiet check
for accidentally queued access. Graph/supersedes tests poison caches to verify
canonical hydration. These checks do not establish crash recovery or global
concurrent snapshot isolation.

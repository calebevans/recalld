# A4 — Agent-facing tools, API contracts, and namespace ergonomics

Status: proposed implementation specification; no behavior below is claimed implemented.
Owner: A4. Date: 2026-11-13. Baseline: `5aa4692c7d326d2ef10712939eb01484a520e891`.
This document follows [CONTRACTS.md](CONTRACTS.md) and [PLAN.md](../PLAN.md). Shared decisions are fixed in CONTRACTS revision 1; only implementation conformance and empirical release gates remain.

## 1. Problem and evidence

- **Demonstrated in isolated real handler/storage probes:** malformed reinforcement quality `-1`, `1.5`, `"3"`, `true`, `null`, `[]`, `{}` all succeeded and mutated synthetic records. The probes invoke `dispatch_tool` and real adapters over temporary `Fixture` databases, not a bridge stub or personal memory service.
- The same probes persisted invalid namespace dimensions: `65536 → 0`, `65537 → 1`, and `u64::MAX → 65535`. Values `0`, `8193`, `65535` were also accepted. Wrong types inherited fixture dimension 4. Controls `1` and `8192` persisted correctly. The proposed common range **1–8192** follows the existing HTTP validation, not the storage integer width.
- [Baseline report](../experiments/baseline/REPORT.md) records 487 baseline passes, three passing and three failing added validation tests across 32 cases. Its separate failing supersession probe belongs to A2. These results establish narrow synthetic handler/storage failures; they do not test HTTP transport, external MCP clients, or personal-store mutation. The completed report records seven added tests overall: three passes and four failures; these do not change the 487-pass baseline count.
- [Tool exploration](../evidence/exploration-tools.md) establishes misleading annotations, JSON encoded only as text, inconsistent batch decoding, configured defaults described as literal `default`, and duplicate scans capped at 200 without coverage metadata.
- Additional source inspection: `scan_duplicates` in `src/mcp/bridge_adapters.rs` uses ten nearest neighbors per sampled memory and only joins sampled IDs. Enumerating more IDs alone would not establish exhaustive pair coverage.
- [Retrieval evidence](../evidence/exploration-retrieval.md) shows asynchronous access recording and candidate limits; [storage evidence](../evidence/exploration-storage.md) identifies independent namespace identities at insertion and derived-index ordering risks. These are source-supported risks unless separately demonstrated in their owning specs.
- HTTP already has structured error categories and fixture controls. Internal `src/bench/locomo.rs` paths bypass MCP parsing/decoding; their scores cannot establish agent-facing contract parity.

## 2. Scope and ownership

A4 owns wire schemas, strict parsing, response/error adaptation, namespace resolution, embedding compatibility checks, discovery, pagination semantics, and retry contracts. Reuse existing Rust/Serde helpers and adapters; no new service or schema framework is required.
A1 owns durable receipts, generations, canonical namespace/profile storage, migration and recovery. A2 owns retrieval eligibility/ranking. A3 owns access/feedback semantics. A5 owns performance instrumentation and tuning. A6 owns isolated conformance episodes and traces.
Namespaces are organizational partitions, **not authentication or authorization boundaries**. This work adds no ACLs, credentials, implicit cross-namespace search, provider router, ANN index, or production-only fixture bypass.

## 3. Versioning and strict input contract

Retain existing tool names and HTTP paths. Introduce application `contractVersion: 2` as an optional MCP argument on every tool; HTTP selects the same contract with `Recalld-Contract-Version: 2`. Absence selects legacy v1. This is distinct from MCP transport protocol negotiation. Reject unsupported application versions before executing operations.
Publish both response variants in each tool's output schema. The version selector is visible in input schemas and discovery, never inferred from client identity. Legacy defaults and success payload shapes remain available during migration. Contract v1 selects retrieval semantics 1, contract v2 selects semantics 2; neither changes namespace lifecycleVersion. CONTRACTS defines separate disk, receipt and harness version names.

All versions immediately receive the demonstrated validation fixes:

- Arguments must be objects. Required fields must exist with their declared type; arrays validate every element. No string-to-number conversion, clamping, lossy narrowing, or invalid-value fallback.
- `quality`: omitted means 3; supplied values must be JSON integers 1–4. Explicit `null` is invalid. Parse and range-check before converting to `u8` or calling any bridge method.
- `embeddingDim`: omission selects the active profile dimension; supplied values must be JSON integers 1–8192. Explicit `null` is invalid. Check range before checked conversion, then check profile compatibility before namespace creation.
- Optional namespace omission resolves a default; an array, number, boolean, object, or empty string never does. Preserve v1's existing optional-null-as-absence behavior for fields other than the two corrected numeric controls. In v2 optional null is allowed only where the published schema explicitly says nullable; `namespace: null` remains omission-compatible.
- Reuse byte limits and collection bounds from `src/model/constants.rs` and `validate_memory_input`. Schema `maxLength` is a loose character bound; descriptions and server validation state UTF-8 byte limits.
- Validate finite floats, UUIDs, enum values, ordered time ranges, and mode-specific combinations before work. Retain existing numeric bounds except the explicitly unified dimension range.
- V2 objects reject unknown fields (`additionalProperties: false`); v1 retains unknown-field tolerance for compatibility. The server and published schemas must agree on integer representation and nullability.
- In v2 `find_similar_memories` uses mode-discriminated schemas. `single` rejects scan-only `threshold`, `cursor`, and `pairBudget`; `scan` rejects `id`, `sameNamespace`, `limit`, and `minScore`. V1 retains ignored-field behavior but describes it accurately.

No invalid single mutation may change canonical state, indexes, access history, or enqueue lifecycle work. Batch item validation has the distinct partial-outcome contract below. Validation errors must identify the field, not be substituted by a later storage failure.

## 4. Namespace resolution and defaults

Resolve scope once, before embedding, lookup, mutation, or receipt reservation. Carry immutable namespace identity through every adapter; storage asserts that the record namespace equals the vector-store namespace (A1).

| Operation | Resolution rule |
| --- | --- |
| Store, recall, list, tags, duplicate scan | Explicit namespace; otherwise configured connection/session default; otherwise `default` |
| Get, reinforce, forget by UUID | Source record's namespace; optional explicit namespace is an assertion and mismatch is `CONFLICT/NAMESPACE_MISMATCH` |
| Similarity `single` | Source record's namespace by default; v2 explicit namespace asserts source scope |
| Create/delete namespace | Explicit target name; never infer a destructive namespace target |
| Namespace catalog | Explicit catalog operation may enumerate names; enumeration does not authorize implicit all-namespace recall |

For stdio, freeze the existing loaded per-directory/default configuration when constructing the bridge. For HTTP, use the configured server default for requests without an explicit namespace; do not infer a client's working directory. No mutable process-global “current namespace” tool is added. A config change affects new bridge/session contexts, not an in-flight request.
V1 retains its established HTTP omission behavior if it differs from MCP. V2 uses the rule above on both surfaces. Harness requests specify namespace explicitly, and separately test omission behavior against the discovered context.
Remove literal `default` schema defaults wherever the value is contextual. Every v2 result carries resolved scope, including canonical namespace name and ID; UUID mutations resolve by record rather than accidentally targeting a session namespace.
Preserve explicit legacy `sameNamespace: false` as an opt-in cross-namespace similarity operation, never as a default. Compare only compatible embedding profiles and report searched/excluded namespace IDs with reasons. V2 requires explicit `namespaceScope: "allCompatible"` for this behavior and rejects conflicting selectors. A2 applies scope to every candidate source and supersession replacement.
Retain existing namespace name validation and case-insensitive uniqueness; return the stored canonical name. Do not silently create a missing default namespace during a memory write.

## 5. Structured outputs and errors

Add optional `outputSchema` to `ToolInfo` and `structuredContent` to `ToolCallResult`. Define shared serializable domain envelopes used by HTTP and MCP adapters; do not independently reconstruct result semantics in both handlers.
V2 MCP returns the envelope in `structuredContent` and identical JSON in its text block. HTTP v2 returns that envelope directly. V1 retains existing JSON-text/prose decoding and field shapes; structured additions are advertised only where supported by the repository's negotiated transport. Test both transport eras already present in `server.rs`.

```json
{
  "contractVersion": 2,
  "status": "ok",
  "scope": {"namespaceId": 7, "namespace": "project"},
  "data": {"id": "<memory-uuid>"},
  "receipt": {"storeId": "<store-uuid>", "operationId": "<operation-uuid>", "committedGeneration": "42", "receiptVersion": 1, "outcome": "applied", "replayed": false},
  "warnings": []
}
```

Envelope rules: `status` is `ok|partial|error`; `data` is operation-specific and absent only on whole-operation failure; `receipt` appears only for committed writes. Reads include `readGeneration: {storeId, generation: "42"}` where consistency/pagination requires it. All generation/counter values follow CONTRACTS decimal-string encoding and range. Access-recording reads include both the readGeneration and a distinct write receipt. `warnings` is always an array. `scope` may contain an explicit namespace-ID list for opted-in multi-namespace reads. Catalog operations use `{ "kind": "catalog" }`.
Errors use `{category, code, message, field?, retryable, commitState, operationId?}`; committed errors additionally carry the receipt and shared readiness object. Retryable means reconcile/retry the same keyed operation, never assume rollback. `field` is a JSON Pointer such as `/memories/2/quality`. `commitState` is `notCommitted|committed|unknown`; absence of a response is never evidence of rollback. Messages exclude credentials and raw memory content.

| Category | Example stable code | HTTP v2 |
| --- | --- | --- |
| invalid_input | `INVALID_TYPE`, `OUT_OF_RANGE`, `INVALID_COMBINATION`, `RESPONSE_TOO_LARGE` | 400; semantic vector/profile input failures 422 |
| not_found | `MEMORY_NOT_FOUND`, `NAMESPACE_NOT_FOUND` | 404 |
| conflict | `IDEMPOTENCY_CONFLICT`, `STORE_LINEAGE_MISMATCH`, `NAMESPACE_MISMATCH`, `CURSOR_STALE`, `EMBEDDING_PROFILE_MISMATCH` | 409 |
| unavailable | `EMBEDDING_UNAVAILABLE`, `INDEX_NOT_READY`, `STORAGE_UNAVAILABLE` | 503 |
| partial | `BATCH_PARTIALLY_APPLIED` | 207 |
| internal | `INTERNAL_ERROR` | 500 |

MCP execution errors use `isError: true`; JSON-RPC protocol/method errors remain protocol errors. V2 partial and all-failed batches also set `isError: true` and retain machine-readable per-item data. A client must inspect outcomes, not infer success from a JSON-RPC result. V1 preserves existing flags during migration; discovery explicitly warns about legacy batch flags.
After a commit, enrichment failure (for example near-duplicate reporting) returns committed success plus a warning, never an apparent failed write. A2 adds budget/truncation and ranking explanation fields to recall `data`; A4 serializes them without claiming exhaustive recall.

## 6. Writes, batch outcomes, and idempotency

Every successful mutation yields an A1 operation ID and committed generation, including access-recording recall. One canonical item write targets exactly one resolved namespace. Multi-item batches are explicitly **not atomic**.
`idempotencyKey` is an optional 1–128-byte printable ASCII string; HTTP also accepts `Idempotency-Key`. If body and header both supply it, require equality. A4 normalizes input; A1 atomically stores the receipt and canonical mutation.
Receipt scope is `(storeId, namespaceId, key)` across operation kinds/transports. Operation kind is part of payload identity, not key scope. Namespace creation and batch manifests use distinguished catalog scope with normalized target/ordered request in their payload hashes; deletion receipts survive namespace deletion. Keyed requests require explicit storeId; stale lineage conflicts. Optional expectedGeneration is a decimal-string store-wide CAS paired with storeId. A namespace-ID selector permits receipt replay after deletion without accidentally targeting a recreated name.
Payload identity includes contract version, resolved scope, supplied content/IDs, ordered arrays, and expanded semantic defaults; exclude transport IDs and the key itself. A1 requestHashVersion 1 hashes the length-delimited canonical request with SHA-256; canonicalize object keys and preserve array order. Receipts retain hashes/IDs/numeric outcomes, not copies of content. A5 cache equality remains a separate exact-input requirement.
A matching key/payload returns the original outcome, IDs, and generation; it does not reinforce twice, rerun duplicate enrichment, or allocate another record. A different payload returns `IDEMPOTENCY_CONFLICT` with no write. Concurrent matching requests serialize at A1's receipt boundary.
Retention is the lifetime of the store lineage, including deleted namespaces and finalized batch outcomes; no idempotencyExpiresAt or silent key reuse. Backup/migration preserve receipts. Production restore creates a new storeId, preserves available old receipts historically, and rejects old-lineage retries. A1 owns persistence/removal-ledger reconciliation; A5 measures growth.

V2 batch request: `{memories: [...], storeId?, idempotencyKey?}` or `{ids: [...], storeId?, idempotencyKey?}` (storeId required with a key) using existing collection limits. Reject an invalid outer object, empty/oversized array, or conflicting envelope fields before any item. Parse items individually so a malformed item can have an indexed failure without hiding later valid items.
Return `data: {total, succeeded, failed, unresolved, outcomes}`; `outcomes` has exactly one entry per input position in original order. Entries contain `index`, `status: "ok"|"error"|"notAttempted"|"unknown"`, resolved scope when known, and either data/receipt or error. Counts satisfy `total = succeeded + failed + unresolved`; notAttempted and unknown count as unresolved. Committed items count as succeeded even when readiness is false; their receipt and warning remain visible. Never omit failed indices. Empty batches are invalid, not successful no-ops.
Process in input order initially. Independent failures do not cancel later items; a service-wide failure may mark remaining entries `notAttempted`. Each item commit and receipt are atomic; batch completion is not.
A keyed batch reserves a durable manifest over the whole normalized ordered request, derives internal item receipt identities from manifest ID plus index, and resumes unfinished items after a crash. Finalized outcomes replay unchanged, including failures; retry a finalized failed item with a new key. Pending notAttempted/unknown entries resume only after reconciliation by child operation ID. Persist hashes and child IDs, not deleted payload copies; the matching resubmitted request supplies unfinished item inputs. Manifest bookkeeping advances generation but does not make the batch atomic. Manifest lookup/reservation is catalog-scoped; individual mutation receipts remain namespace-scoped. No top-level committed generation implies batch atomicity; return item receipts and a separate `batchOperationId`.
V2 all-ready-success HTTP batches return 200; mixed or committed-but-not-ready batches 207; all-final-failed batches 422; zero-success batches with unresolved work 503. Always include full outcomes/retryable flags, and set MCP isError=true except for all-ready success. Unkeyed mutations remain allowed but have no exactly-once retry promise. A1 must reconcile uncertain commit outcomes before reporting an item as safely retryable.
Ready success requires consistent publication. A committed publication failure returns unavailable/INDEX_NOT_READY with commitState=committed, receipt and CONTRACTS readiness; point reads can use canonical state after pending effects replay. Affected search channels block/rebuild or report unavailable, with A2 degraded coverage only for independent ready channels. There is no mandatory canonical FTS fallback. Receipt lookup/readiness survive restart.

## 7. Truthful duplicate scans and pagination

Ship an immediate v1 correction: remove “entire namespace” wording, retain existing cluster payloads, and add coverage metadata without claiming exactness. Report sampled count, eligible count, skipped-vector count, sample cap, neighbor cap, algorithm, and `complete: false` unless exhaustive pair coverage was actually established. Never infer completeness merely from fewer than 200 returned IDs.
V2 `find_similar_memories(mode="scan")` and HTTP duplicates use one exact paged comparator. Inputs are resolved namespace, `threshold` (default 0.85, range 0–1), optional `cursor`, and `pairBudget` (default 10,000; range 1–100,000). These are provisional workload limits, not measured optima.
Freeze eligible IDs in ascending canonical UUID order at store GenerationRef G: live, non-tombstoned records with valid profile-compatible vectors, including ghosts when their vectors remain valid. Report phase counts and records excluded because vectors are missing/invalid; exclusions prevent a claim of complete namespace coverage.
Enumerate unordered pairs lexicographically `(i,j), i<j`, including pairs whose members fall across any record-page boundary. Compare exact cosine using A5's exact vector oracle; each page advances at most `pairBudget` pairs. No nearest-neighbor cap or approximate search is allowed in an exact scan.
Return `pairs: [{leftId,rightId,similarity}]`, `coverage: {algorithm:"exactPairs", readGeneration, eligibleCount, excludedCount, totalPairs, comparedThisPage, comparedSoFar, complete}`, and `nextCursor`. Use checked arithmetic for `n*(n-1)/2`. V2 returns pair edges, not misleading page-local global clusters; consumers can union all edges after completion. Describe threshold-connected components rather than implying every pair in a cluster exceeds threshold.
The opaque cursor binds contract version, storeId, namespace ID, decimal-string generation, profile ID, threshold, eligibility policy version, and next pair position. Server validation rejects malformed tokens, changed request bindings, and invalid positions. Cursors are continuation state, not authorization tokens.
Read generation and vectors consistently for each page; if generation changes during the page, discard it and return `CURSOR_STALE`. Later pages also reject changed generation. This avoids long-lived snapshots and unbounded server-side scan jobs; frequent writes may require a quiet audit window. Use A1's conservative store-wide generation; unrelated writes can invalidate a cursor. Do not add an independent namespace counter.
`complete` is true only on the terminal page when all pairs were compared and no eligible namespace records were excluded; the statement applies to the aggregate scan, never the current page's pairs alone. A terminal scan with exclusions has `nextCursor:null`, `complete:false`, and an explicit warning. Missing vectors are not silently treated as dissimilar. Empty/one-record scopes terminate correctly.
Use the same generation-bound UUID cursor for v2 `list_memories`; retain v1 offset semantics and document concurrent-write instability. Bind filters to cursors. Tag/namespace catalogs must expose truncation and continuation when bounded; never present a cap as an exhaustive vocabulary.

## 8. Access recording and truthful annotations

Add explicit `recordAccess` to recall/search and direct/batch fetch on both surfaces. Integrated compatibility default: v1 and v2 both default to `true` initially; harness/evaluation requests always pass `false`. Changing that default requires an A3-versioned transition and measured retention/utility evidence, not an A4 silent change.
`recordAccess:false` performs no lifecycle, history, reinforcement, persistent RIF, or delayed access writes. Telemetry may run only if it cannot affect future retrieval. A3 owns the exact observation event and clock semantics; A4 propagates the flag through `SearchInput` to the pipeline and verifies the absence of queued work.
For `true`, A3/A1 must commit the access event before returning a write receipt; an asynchronous untracked write cannot satisfy v2 retry guarantees. A failed recording step must state whether retrieval data is available and whether observation committed; never turn a successful retrieval into a false claim of durable observation.
Tool-wide annotations describe the most mutating permitted call: recall `readOnlyHint:false`, `idempotentHint:false`; reinforce/store/create and their batches also `false/false` because keys are optional. Destructive flags are true for forget/delete and operations capable of superseding or weakening existing memory. Get/direct/batch fetch also use false/false hints while access recording defaults true. List/stats/similarity are read-only only after no-write tests confirm it; graph-context fetches explicitly propagate recordAccess. Set open-world hints according to external-provider use; do not mark embedding-dependent operations closed-world by default.
Keep logical forget, namespace deletion, supersession, and decay distinct. Forget removes logical content according to A1/A3, not forensic copies in backups; discovery links to retention policy instead of promising erasure.

## 9. Embedding profile compatibility

Keep one active embedding provider configuration. Persist namespace `embeddingProfileId` plus immutable identity: provider kind, model identifier/revision where available, dimension, document/query preprocessing versions and prefixes, and normalization convention. A1 owns serialization/migration; A4 owns validation and agent-visible diagnostics. Do not include credentials in profile identity or discovery.
Dimensions must be 1–8192 and equal both namespace dimension and active provider dimension. Equal dimensions alone do not establish compatibility: reject a different profile identity even at the same dimension. Namespace creation cannot request an arbitrary inactive profile. The parser accepts boundary controls 1 and 8192 as valid ranges, but end-to-end creation additionally requires a matching active profile.
Validate generated and HTTP precomputed vectors before any allocation: exact length, finite elements, nonzero norm, and declared profile. Apply the existing unit-normalization convention at the shared boundary; reject zero/nonfinite vectors. Provider-response shape failures are unavailable errors; caller-supplied vector failures are invalid input. Batch provider outputs must match input count/order and validate each vector before committing its item.
HTTP v2 precomputed embeddings require `embeddingProfileId`. Keep fixture-only timestamp/stability controls on the existing HTTP setup path; do not expand MCP solely for benchmarks. A6 uses an isolated deterministic provider profile and the same vector validator.
Legacy namespaces with unknown profile identity must not be silently labeled compatible by dimension alone. Migration permits an explicit operator attestation against known configuration/provenance; otherwise semantic writes/search remain blocked pending re-embedding. Metadata reads/export remain available. Existing v1 precomputed-vector requests may use the active profile only for an already bound compatible namespace and emit an unverified-provenance warning.
Cache keys include profile identity, document/query mode, and transformed text. Provider hot changes require rebuilding profile-bound caches and validation; never reuse old vectors under a new model name. Multi-provider routing remains deferred until separate workload evidence justifies it.

## 10. Discovery and brief agent guidance

Extend existing discovery instructions and add `recalld://session` plus HTTP `GET /capabilities`; reuse `resources.rs` and existing transport discovery, not a new tool family. Return supported application versions, effective default namespace/source, active storeId, profile ID/dimension, limits, access default, lifetime receipt retention, scan semantics, all supported version names and fixture controls. Never return config paths, secrets, or memory content. Session-specific resources must not inherit public catalog caching from `server/discover`.
Keep stable public discovery text short; put resolved mutable context in the session resource. Namespace catalog entries expose profile compatibility and blocked reasons. Schemas, limits, and typed parsers share constants; conformance tests prevent drift.
Suggested agent instructions (under 150 words):
> Read session scope first. Omitted namespace uses that context for store/search; by-ID operations use the record namespace. Namespaces organize data; they are not access controls. Use explicit namespace in reproducible workflows. Pass `recordAccess:false` for evaluation or inspection. Reinforce only deliberate usefulness feedback. Reuse an idempotency key only for the identical intended write and with its original storeId; receipts last for that lineage. Inspect every batch outcome; partial batches are not atomic. Follow duplicate-scan cursors and check coverage before concluding there are no duplicates. Similarity is evidence, not proof of equivalence; do not automatically delete memories because they are close. Forget is logical removal, not backup erasure.

## 11. Concrete implementation touchpoints and dependencies

| Files/interfaces | Required change / dependency |
| --- | --- |
| `src/mcp/args.rs`, `src/model/validation.rs`, `src/model/constants.rs` | Strict quality/dimension parsers, checked conversions, shared bounds, mode/unknown-field validation |
| `src/mcp/tools.rs`, `src/mcp/protocol.rs` | Version selector, schemas/envelopes, annotations, batch outcome mapping, accurate descriptions |
| `src/mcp/bridge.rs`, `src/mcp/bridge_adapters.rs` | Resolved scope, typed errors/receipts, access flag, duplicate page contract; remove capped-neighbor completeness assumption |
| `src/api/models.rs`, `handlers.rs`, `errors.rs`, `routes.rs`, `state.rs` | Same validators/envelopes, HTTP version/idempotency adapters, capability route and fixture parity |
| `src/mcp/server.rs`, `resources.rs`, `transport.rs`, `http_transport.rs` | Discovery/version compatibility, session resource, output serialization and caching checks |
| `src/config/loader.rs`, `src/config/types.rs`, `src/model/namespace.rs` | Frozen default provenance, embedding identity; A1 migration/storage owns persistence |
| `src/embedding/mod.rs`, `provider.rs`, `prefix.rs`, `cache.rs` | Shared profile/vector validation and cache identity, no provider router |
| `src/search/pipeline.rs`, `src/search/adapters.rs`, `src/api/adapters.rs` | A2 scope/eligibility and search readiness; A3 no-write observation mode |
| `src/storage/engine.rs`, `src/storage/metadata.rs` | A1 namespace invariant, generations, atomic receipts/manifests and recovery; A4 defines wire needs only |
| `src/bench/locomo.rs`, existing MCP/API tests, A6 harness | Label internal-only benchmarks; add actual transport contract episodes with synthetic fixtures |

Order: (1) strict validation and truthful descriptions/annotations; (2) shared typed contracts and scope; (3) A1 receipts/generations/profile migration plus A3 access semantics; (4) v2 writes, exact pagination and discovery; (5) A6 conformance rollout. A2 eligibility and A5 exact comparisons are prerequisites for claiming scan coverage, not for fixing malformed writes.

## 12. Acceptance tests and release gates

These are future tests, not tests executed by this spec author. A6 uses temporary data, no credentials/personal corpus, deterministic embeddings and an injected A3 clock.

1. Promote all 32 validation probe cases to regressions. Every invalid case returns the field-specific error and preserves canonical records, history, namespaces and queued work; valid omitted/1–4 quality and dimension range controls still pass their appropriate parser/profile stages. Explicitly cover wrapping values and null.
2. Run equivalent requests through real MCP `dispatch_tool`, stdio, HTTP MCP, and REST. Compare normalized v2 envelopes, error codes and committed state; do not substitute stubs for storage mutation assertions. Validate outputs against published schemas and verify text/structured equivalence.
3. Two sessions with different defaults never cross scope. Explicit scope wins; wrong type never falls back. By-ID assertions detect mismatches. Single similarity defaults locally; explicit cross-scope calls exclude incompatible profiles. Verify all A2 candidate sources/replacements honor scope.
4. With a fixed clock, repeat `recordAccess:false`, drain background tasks, restart, and compare persistent lifecycle/history/generation and later ranking. Require no changes. `true` and reinforcement must produce the A3-defined change exactly once under a matching key.
5. Mixed and all-failed batches preserve indices/counts and error flags. Inject faults before/after each item commit and before response; keyed replay after restart neither duplicates successful items nor drops failures. Conflicting payloads, concurrent retries, lifetime retention and stale lineage follow the documented receipt policy.
6. Scan 201+ synthetic records with a duplicate pair spanning pages, dense neighborhoods exceeding ten neighbors, tied scores and disconnected components. Aggregate exact pair edges must equal an exhaustive oracle; each pair is compared once. Test budgets 1 and maximum, zero/one record, excluded vectors, changed generation, malformed/rebound cursors and restart.
7. Test equal dimensions with different model/prefix identities, wrong generated vector length, provider batch count mismatch, NaN/infinity through internal adapters, zero norm, and precomputed-vector provenance. Fail before mutation. Confirm unknown legacy profiles cannot be silently blessed.
8. After ready success, immediate MCP/REST reads see committed state. Inject publication failure: writes expose committed receipts/unavailable; canonical point reads work after effects replay, and affected search never returns false empty success. Verify restore-lineage conflicts, historical receipts and stale cursors.
9. Snapshot v1 tool schemas/results and representative legacy JSON-text consumers; allow only documented correctness fixes and additive coverage fields. Test v2 unknown-field rejection, mode unions, null rules, discovery scope caching, and unsupported versions.
10. A5 measures parser/envelope overhead and exact-scan page latency at 200/2,000/20,000 records with representative dimensions. Initial release gate: no correctness regressions and at most 10% p95 overhead for ordinary calls against the same fixture/provider baseline. The default pairBudget remains 10,000 until a versioned budget update; if p95 pages exceed one second, exact-scan promotion is blocked pending measured tuning. A lower experimental page budget must never change accumulated exact results. These thresholds are proposed gates, not measurements.

## 13. Rollout, rollback, and integration decisions

Release strict quality/dimension fixes first on both versions, with release notes for previously accepted invalid/null inputs. Add truthful scan metadata/annotations and legacy description fixes next. Do not roll validation back to silent coercion.
Ship v2 opt-in while A6 exercises both versions; retain v1 for at least two releases. Default migration requires passing conformance, named legacy-client compatibility checks, zero unexplained lost/duplicate writes in injected-fault tests, and A5 latency gates. Publish the migration date only after these pass; do not infer client readiness from the existing 487 passing tests.
A failed v2 rollout disables its advertisement and selection while keeping v1 operational against the new shared validators. Preserve durable receipts/profile metadata; never downgrade on-disk state by deleting unknown fields. Before an old binary is used, A1 must certify format compatibility or restore a verified pre-migration backup, with explicit reconciliation of intervening writes. Existing receipt commitments must not disappear on protocol rollback.
**Decided here:** common 1–8192 dimension range; missing-only quality/dimension defaults; namespaces are organization; no implicit all-scope retrieval; one provider; v2 opt-in; explicit partial outcomes; exact pair pagination; no-write evaluation mode; no automatic duplicate deletion.
**Closed integration decisions:** CONTRACTS fixes lifetime namespace/catalog receipt scope, batch unresolved states, restore lineage, synchronous default-true observation, profile attestation and separate duplicate-scan eligibility (including valid Ghost vectors) versus recall eligibility. Advertise contract v2 only on negotiated transports whose schema/result types support outputSchema and structuredContent; unsupported legacy transports remain v1 and reject explicit v2 before work. Actual client conformance is a release test, not an undecided wire design.
**Measurement gates, not blockers to this draft:** A5 page budgets/overhead thresholds, A6 actual client compatibility, and future retention evidence for changing observation defaults. No multi-provider routing, alternative storage choice, or new lifecycle policy is justified by these validation probes alone.

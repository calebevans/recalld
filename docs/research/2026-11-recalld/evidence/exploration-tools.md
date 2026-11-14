# MCP tools exploration

Date: 2026-11-13. Scope: static inspection of MCP descriptions, namespace ergonomics, API consistency, and harness readiness. No experiments, test runs, external research, production edits, or recalld MCP calls were performed. Findings describe the inspected source, not observed runtime failures. No on-disk AGENTS.md was found in the repository or parent paths; supplied conversation instructions applied.

Source links below are relative to this evidence directory. Line numbers identify the inspected revision and may drift.

## Prioritized findings

### P1: Argument validation can silently change write intent

**Source-proven contract:** Reinforcement obtains `quality` with `as_u64().unwrap_or(3)`. Strings, negative numbers, and fractional numbers therefore take the default quality 3 instead of producing a type error. Valid unsigned values outside 1–4 are rejected. Namespace creation also extracts optional numeric arguments through permissive conversions; `embeddingDim` is cast from u64 to u16 without a prior bounds check in the handler. This is inconsistent with the strict helpers used by storage, recall, and listing.

Evidence: [MCP handlers](../../../../src/mcp/tools.rs), `handle_reinforce_memory` around line 738 and `handle_create_namespace` around line 1242; [strict argument helpers](../../../../src/mcp/args.rs).

**Untested failure hypothesis:** A malformed caller request could reinforce a memory with the opposite intended meaning, or submit a wrapped embedding dimension to downstream validation. Downstream acceptance of wrapped dimensions was not established.

**Minimal remedy:** Reuse strict typed and bounded argument helpers for the remaining write handlers.

### P1: Behavioral hints understate mutation and retry effects

**Source-proven contract:** `reinforce_memory` advertises `idempotentHint: true`; its adapter recalculates and persists decay state and records access on invocation. `recall_memories` advertises `readOnlyHint: true`, while the search pipeline schedules asynchronous retrieval access recording. The latter can finish after a search response.

Evidence: [tool annotations](../../../../src/mcp/tools.rs), reinforcement around line 712 and recall around line 545; [reinforcement adapter](../../../../src/mcp/bridge_adapters.rs), around line 1372; [search access recording](../../../../src/search/pipeline.rs), `record_accesses_async` around line 1210.

**Untested failure hypothesis:** Automatic retries or repeated harness evaluations could alter access history and later retrieval or decay behavior. Magnitude and timing sensitivity were not measured.

**Minimal remedy:** Align annotations with side effects and document retrieval access recording and retry semantics.

### P1: Duplicate scans do not report incomplete coverage

**Source-proven contract:** `find_similar_memories` describes scanning an entire namespace. The scan handler fixes its sample limit at 200; the adapter selects at most that many IDs with `take(max_memories)`. The result includes namespace, threshold, clusters, and cluster count, but no sample size, total eligible count, truncation indicator, or continuation mechanism.

Evidence: [similarity tool definition and handler](../../../../src/mcp/tools.rs), lines 988–1170; [scan adapter](../../../../src/mcp/bridge_adapters.rs), around lines 380–403.

**Untested failure hypothesis:** A caller could interpret no returned clusters as proof that a larger namespace has no duplicates. Missed-duplicate frequency and sample representativeness were not evaluated.

**Minimal remedy:** State the cap in the description and return explicit coverage metadata; add continuation only if complete audits are required.

### P2: Namespace defaults and mode scope disagree with descriptions

**Source-proven contract:** Several schemas advertise a literal namespace default of `default`, but omitted namespaces are resolved through `bridge.default_namespace()`, which can come from per-directory configuration. Similarity single-mode parsing ignores `namespace`; scope follows the source ID and `sameNamespace`. The similarity schema describes namespace as required for scan mode, but scan parsing accepts omission and resolves the session default.

Evidence: [tool schemas](../../../../src/mcp/tools.rs), store around line 177 and similarity around line 1011; [argument parsers](../../../../src/mcp/args.rs), `namespace_or` around line 349 and similarity parsers around lines 857–889; [configuration loader](../../../../src/config/loader.rs), `LoadedConfig` and per-directory configuration.

**Untested failure hypothesis:** A harness that materializes schema defaults could target a different namespace from a caller that omits the field. A caller could also believe an ignored single-mode namespace restricts scope.

**Minimal remedy:** Describe the configured session default accurately, expose resolved scope where useful, and reject or clearly document irrelevant mode arguments.

### P2: Output and error contracts require tool-specific decoding

**Source-proven contract:** `ToolInfo` has an input schema but no output schema. `ToolCallResult::json` serializes JSON into a text block; ordinary errors are prose with `isError: true`. Batch storage returns an object with `results`, `total`, `stored`, and `errors`, despite the description saying it returns an array. Per-item failures are returned through the ordinary success constructor, including when every item fails.

Evidence: [protocol types](../../../../src/mcp/protocol.rs), lines 322–406; [batch storage handler](../../../../src/mcp/tools.rs), lines 384–450.

**Untested failure hypothesis:** A generic harness that checks only JSON-RPC success or outer `isError` could count failed batches as successes. No client behavior was exercised.

**Minimal remedy:** Correct the batch description, document partial-success semantics, and provide stable structured outputs and machine-readable error fields where harness consumers need them.

### P2: Internal benchmarks do not establish MCP readiness

**Source-proven contract:** Benchmark ingestion uses internal storage and indexing paths; benchmark retrieval calls the query engine directly. These paths bypass MCP schema interpretation, argument parsing, transport, and tool-result decoding. This finding does not imply there are no MCP tests: the repository contains protocol, transport, parser, and handler tests, and CI invokes `cargo test --all-features`.

Evidence: [benchmark implementation](../../../../src/bench/locomo.rs), `store_single_memory` around line 1233 and direct query around line 1654; [MCP modules](../../../../src/mcp/); [CI configuration](../../../../.github/workflows/ci.yml), around line 48.

**Untested failure hypothesis:** Good internal benchmark scores could coexist with agent-facing contract failures. Existing test coverage and client compatibility were not validated by running tests.

**Minimal remedy:** In a later authorized experiment, use an isolated MCP contract harness that exercises discovery, representative calls, malformed arguments, partial batches, namespace scope, and result decoding.

### P3: MCP lacks HTTP fixture controls

**Source-proven contract:** HTTP memory creation exposes `createdAt`, precomputed embeddings, and initial stability. The MCP storage input schema does not expose those fields.

Evidence: [HTTP creation model](../../../../src/api/models.rs), lines 42–77; [MCP storage schema](../../../../src/mcp/tools.rs), starting around line 133.

**Untested failure hypothesis:** Backdated or deterministic fixtures may require a separate setup interface, reducing fidelity between fixture preparation and agent-facing operations. This is a harness limitation, not necessarily a missing end-user feature.

**Minimal remedy:** Decide whether fixtures should use HTTP setup or whether a narrowly scoped MCP capability is warranted; avoid adding production controls solely for speculative testing needs.

## Assessment boundary

The strict parser helpers, behavioral annotations, and per-item batch outcomes are useful existing foundations. Prioritize validation correctness, mutation/retry semantics, and duplicate-scan coverage before experiments. Runtime correctness, protocol conformance, performance, concurrency effects, and compatibility with particular clients remain unverified.

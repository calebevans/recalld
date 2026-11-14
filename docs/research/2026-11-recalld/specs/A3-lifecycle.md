# A3 — Memory lifecycle and feedback semantics

Status: **integrated proposal; implementation and empirical release gates remain**. Written 2026-11-13 against research baseline `5aa4692c7d326d2ef10712939eb01484a520e891` and the inspected working tree. Nothing in this document claims these changes are implemented. This task performed source/evidence review only; no tests, experiments, or personal memory tools were run.

A3 owns lifecycle semantics. [CONTRACTS.md](CONTRACTS.md) governs shared interfaces; [PLAN.md](../PLAN.md) governs evidence. A1 owns physical schema, transactions, receipts, migration, and recovery. This document specifies the logical fields and invariants A1 must preserve, without selecting a new backend.

## 1. Problem and evidence

| Finding | Evidence and confidence | Required response |
|---|---|---|
| Returned results asynchronously update `last_accessed_at` without FSRS stability growth; sweeps use that timestamp as their decay anchor. Exposure therefore changes retention without usefulness feedback. | [Retrieval exploration](../evidence/exploration-retrieval.md), “Recall refreshes the decay clock”; `SearchPipeline::record_accesses_async`, `MetadataStore::update_access`, `DecaySweepRunner::evaluate_record`. Source-supported behavior; production impact unmeasured. | Separate retrieval observation, explicit feedback, and decay anchor. Version any changed retention semantics. |
| Explicit reinforcement reads elapsed time from last access, constructs a default `DecayConfig`, updates decay state and access separately, and keeps permastore sticky. | `src/mcp/bridge_adapters.rs` reinforcement implementation; matching HTTP implementation in `src/api/adapters.rs`. | One shared transition function, one resolved policy, one atomic canonical mutation. |
| Search uses cached strength; sweep state and current-clock strength can diverge. Sweep transitions carry scan-time stability into later writes. | [Retrieval exploration](../evidence/exploration-retrieval.md); `src/search/pipeline.rs`, `src/decay/sweep.rs`. | Pure, clock-controlled projection and generation-checked sweep commits. |
| Tool hints overstate read-only/idempotent behavior; malformed reinforcement quality can silently become 3. | [Tool exploration](../evidence/exploration-tools.md). | A4 strict parsing and accurate annotations; A1 receipts, not hints, provide retry safety. |
| Phase/content writes are separate; phase indexes and deleted vector slots require stronger storage guarantees. | [Storage exploration](../evidence/exploration-storage.md); sweep transition methods and `src/index_cleanup.rs`. These are source-supported risks, not reproduced production failures. | Couple phase and content loss atomically; publish derived state through A1's generation contract. |
| RIF integration repeats neighbor suppression and differs from engine intent. | [Retrieval exploration](../evidence/exploration-retrieval.md); [existing probe source](../experiments/ablations/probes.py) and [results](../experiments/ablations/results.json). Probes explicitly model algorithms in Python, not the Rust engine. | Preserve configured legacy RIF in contract v1 and nonpersistent behavior in both contracts; opt-in contract v2 uses RIF off per A2. Models do not establish a quality benefit for either mode. |

[External memory research](../evidence/external-memory-research.md) motivates deterministic chronological evaluation and deletion hard gates. It does not validate FSRS usefulness ratings for agent memories. No new efficacy claim follows from this spec. New experiment results may revise priorities and gated defaults, but not terminal deletion or read-only guarantees.

## 2. Decisions and policy versions

Introduce semantic policy versions independently of disk format:

- `legacy_access_v1`: default for migrated namespaces and initially for newly created namespaces. Returned results with access recording reset the decay anchor but do not increase stability, preserving existing exposure-driven retention. Explicit feedback retains the existing FSRS quality formula and sticky permastore rule.
- `explicit_feedback_v2`: opt-in per namespace. Retrieval only records exposure; only creation, a controlled policy migration, or explicit feedback sets the decay anchor. It uses current-clock strength projection. New-namespace default promotion is gated by §10; existing namespaces never switch automatically.

Both versions enforce strict validation, transactional feedback, terminal deletion, no reconstruction of discarded content, and genuine read-only queries. These correctness changes are not optional compatibility bugs. Legacy mode preserves retention policy, not asynchronous lost writes or malformed-input acceptance.

Keep initial stability 3.7145 days, difficulty 5.0, thresholds 0.70/0.30/0.05, permastore threshold 1500 days, connection bonus cap 0.15, and existing FSRS constants/formulas. RIF is independent of lifecycle policy: contract v1 retains configured query-local defaults, contract v2 defaults off, and A2 owns operator-only shadow/experimental modes. Keep existing numeric constants for the legacy comparator. Do not add automatic reinforcement from clicks, graph exposure, cache hits, or an answer model's unsupported confidence.

## 3. Logical schema and clock

A1 persists these logical additions alongside stable UUID, namespace, content, stability, difficulty, phase, and permastore flag. Wire names are camelCase; internal names below are snake_case.

| Field | Meaning and invariants |
|---|---|
| `observed_at: i64` | UTC epoch milliseconds when the source observation happened; defaults to creation time. Immutable through retrieval/feedback. It is provenance, not a retention anchor. A4 may accept an explicit value on store/import; it must be a valid representable timestamp. |
| `retrieved_at: Option<i64>` | Latest committed exposure of this memory as a returned result or direct fetch. Null means no known exposure. Candidate discovery alone does not set it. |
| `reinforced_at: Option<i64>` | Latest committed explicit usefulness rating, including quality 1. Null means unknown/no feedback; it does not imply never retrieved. |
| `decay_anchor_at: i64` | Last retention-clock reset. Necessary because legacy timestamps cannot prove feedback and version migration must not invent it. Initialized to creation time for new records. |
| `lifecycle_version` | `legacy_access_v1` or `explicit_feedback_v2`; must match the namespace's committed policy version. |
| `lifecycle_updated_at: i64` | Last committed lifecycle mutation time; prevents timestamp regression after a backward wall-clock step. |
| `timestamp_origin` | `native` or `legacy_inferred`. Signals incomplete historical attribution after migration; never fabricate feedback history. |
| `phase_changed_at: i64` | Time content-retention phase last changed; initialized to creation time, or migration time when historical time is unknown. |
| `policy_revision` | Namespace policy revision used for the last lifecycle mutation. Query projections separately report their current policy revision. |
| `record_generation` | A1 per-record decimal-string concurrency token used to reject stale transition plans. This is distinct from the database committed generation. |

`strength` and `decay_strength` remain persisted compatibility/materialization fields, not authoritative independent clocks. A projection returns raw R, effective R, `evaluatedAt`, policy version/revision, committed phase, and proposed next phase. A2 owns how these fields enter scores. Permastore is retention exemption, not a claim that semantic relevance equals 1.

Add `Clock::now_millis()` in `src/time/mod.rs`, with a production system clock and A6 manual clock. Inject the same clock through `MemorySystem` into creation, feedback, retrieval, and sweeps. Pure functions receive `now_millis` explicitly. Capture once per operation/sweep snapshot; do not call `Utc::now()` midway through a transition. Use monotonic `Instant` only for performance measurement and timers, never retention math.

For a write, `effective_now = max(clock_now, lifecycle_updated_at)`; compute elapsed time with saturating subtraction and clamp at zero. Read-only projection never changes timestamps. Manual clock advances are confined to the isolated A6 instance; there is no public production time-travel endpoint. Reject backwards manual advances except during full snapshot reset. An explicit read-only `as_of` option, if A6 needs it, is internal and never authorizes a production write at that time.

## 4. Pure lifecycle computation and namespace policy

Use the existing FSRS implementation, with three small shared entry points in `src/decay/`:

```text
resolve_policy(namespace, global_config) -> EffectiveLifecyclePolicy
project(record, policy, now_ms, connection_bonus) -> LifecycleProjection
plan_feedback(record, policy, now_ms, quality) -> LifecycleMutation
```

The mutation is a value containing expected record/policy generations, timestamps, stability, strength, phase, and permastore changes. It performs no I/O. A1 applies it and a receipt atomically. HTTP, MCP, daemon, and embedded paths call the same planner; adapters must not reproduce the formula.

For v2, elapsed days are `max(0, now - decay_anchor_at) / 86_400_000`. Raw R uses existing `FsrsEngine::retrievability(elapsed, S, multiplier)`; effective R adds the existing capped connection bonus. Keep the existing multiplier convention: **2 means half as old/slower decay**, 0 disables automatic decay, and 1 is normal. Do not invert this misleadingly named setting. Retrieval timestamps never enter this formula in v2.

Resolve namespace override before global value before built-in default. Existing concrete namespace values for stability, difficulty, phase thresholds, and permastore threshold remain explicit overrides during migration. Add optional namespace overrides only for currently global decay parameters needed by the shared policy (connection cap and policy version if absent). Namespace initial stability applies to new memories only; record difficulty remains authoritative for feedback. Existing `decay_rate_multiplier: None` continues to inherit global configuration. Freeze resolved global inputs and namespace revision for each operation; a config reload is a revision change, not an in-flight mutation of policy.

Reject NaN/infinity for every float, nonpositive stability, difficulty outside [1,10], negative multiplier, nonpositive permastore threshold, bonus outside [0,1], and thresholds not satisfying `0 < delete < ghost < summary < 1`. Preserve existing FSRS stability floor/ceiling. Invalid inherited configuration fails load/update without partially applying it.

A multiplier change applies to the entire elapsed interval at subsequent evaluation; this preserves the current formula and avoids a new historical-rate accumulator. Disabling decay does not pause or reset the anchor: re-enabling can make old memories eligible immediately. Configuration updates return a preview of affected phase counts before activation via the admin workflow; no ordinary query commits that preview. `disable_sweep` stops background scheduling only, not read-only projection or explicit feedback. Zero multiplier returns R=1 and prevents automatic transitions but does not block forget or feedback.

For lifecycle v1, keep canonical materialized-strength retrieval behavior initially so the compatibility release does not silently change ranking. Expose the pure current projection in diagnostics/evaluation. Sweeps and feedback still use the resolved shared policy. A2 must not silently replace the legacy ranking signal with the projection. In lifecycle v1, use that same canonical materialized strength for hard minimum-strength eligibility and ranking; current projection is diagnostic only. In lifecycle v2, query eligibility, ranking strength and diagnostics all use the projection consistently across candidate sources, without content stripping on read. API/retrieval v2 does not itself select lifecycle v2. Projection caches must include query clock and policy revision as well as generation.

## 5. Query observation and evaluation API

Extend search and direct/batch fetch requests with strict `recordAccess: boolean`. Omitted defaults to **true** for existing retrieval/fetch operations in both versions. Listing, statistics, diagnostics, and administrative inspection default to false and remain non-observing. A6 always sends `recordAccess=false`; it must not depend on defaults.

`recordAccess=false` means no canonical writes, receipts, access history, stability changes, graph weight changes, phase materialization, or deferred lifecycle tasks attributable to that request. Query-local RIF calculations may run for comparable ranking but cannot persist. Telemetry/caches are allowed only if they cannot alter later ranking, eligibility, graph bonuses, or lifecycle transitions. Any access-sensitive cache used for those functions must be frozen/bypassed in evaluation. A6 disables autonomous sweeps and drains earlier writes before comparing state hashes; a false-mode query is not a global pause command for a live daemon.

For true mode, collect the final eligible returned UUIDs, deduplicate within the request, and record one exposure per UUID. Direct and associative retrieval remain history classifications but neither invokes FSRS stability growth. Capture access time once. Persist before acknowledging success, replacing fire-and-forget writes. Cross-namespace batch fetch follows CONTRACTS per-namespace exposure receipts and partial outcomes; duplicate UUIDs share one exposure outcome. In v1 update `retrieved_at`, access history, and `decay_anchor_at`; in v2 update only `retrieved_at` and exposure history. Do not change `reinforced_at`, phase, stability, or permastore.

The access write revalidates namespace, record generation, and terminal state. On a concurrent transition, rerun eligibility against canonical state before acknowledging; never copy a stale record back. If persistence is unavailable, return a structured unavailable outcome rather than pretending access was recorded. A1/A4 supply the operation receipt for the access transaction, separately from the pre-exposure readGeneration. Keyed exposure replay returns the original mutation outcome and recomputes any requested content read-only under current visibility; it never returns a saved copy of now-deleted text. Empty results require no access write. Reads without writes expose the snapshot generation rather than inventing a committed write generation.

A2 owns the query snapshot/visibility boundary: after forget is acknowledged, a newly started read cannot return removed content. A query linearized before a concurrent forget may have already returned content; this is not a promise to retract prior responses. A4 must describe recall as potentially mutating and set `readOnlyHint=false` while true remains the default. A read-only evaluation caller obtains the guarantee from the explicit flag, not tool annotations.

## 6. Feedback, idempotency, and ordering

Keep `reinforce_memory` and the HTTP reinforcement route. Proposed request extension:

```json
{"contractVersion":2,"id":"<uuid>","namespace":"<resolved-name>","quality":3,"storeId":"<store-uuid>","idempotencyKey":"<caller-key>","expectedGeneration":"42"}
```

By-ID lookup resolves the record namespace; an explicit namespace asserts that scope and mismatch returns conflict/NAMESPACE_MISMATCH, per A4. An unknown UUID is not found. Quality omission defaults to 3 for compatibility; present values must be integers 1–4. Strings, null, booleans, fractions, negatives, and out-of-range values are invalid. An optional expected generation is A1 store-wide optimistic concurrency control paired with storeId; internal record/policy checks are separate; mismatches return conflict with no feedback applied.

Ratings retain their existing numerical meaning: 1 means not useful/incorrect (lapse formula); 2 limited usefulness (hard); 3 useful (good); 4 very useful (easy). This is a proposed mapping to existing FSRS mechanics, not validated psychological calibration. Quality 1 does not mean delete or supersede; corrections use store-with-supersedes and removal uses forget. Keep existing `review_stability` calculation, including quality multipliers and bounds, until measured changes are authorized.

Under a write transaction/serialized canonical mutation:

1. Resolve namespace and validate request. Check the receipt key/payload before checking caller expectedGeneration or current record/policy generations so a retry can return its original result.
2. For a new operation, read current canonical record and policy; reject Tombstone, hard-deleted UUID, or Ghost feedback (`conflict: content_unavailable`). Summary feedback is allowed for retained summary content. Missing IDs are not found. Never implicitly follow a supersedes link: feedback targets exactly the UUID shown to the caller.
3. Compute elapsed time from `decay_anchor_at` (v1 access may reset it; v2 retrieval cannot). Calculate new stability using record difficulty and effective namespace settings. Set `reinforced_at`, `decay_anchor_at`, and `lifecycle_updated_at` to effective now. Set raw/effective strength to 1, retain the actual content phase, and append one explicit feedback event.
4. Promote to permastore when new stability reaches the configured threshold. Preserve existing true membership for all quality ratings and policy versions. Negative feedback may reduce stability without automatically revoking permastore. No promotion restores missing payloads.
5. Commit updated record, feedback event, operation receipt, and generation atomically; publish derived views before ready success. Publication failure returns committed/unavailable with receipt; canonical point reads remain possible under A1's barrier contract.

Response uses the A4 envelope with receipt `{storeId, operationId, committedGeneration: "43", receiptVersion: 1, outcome, replayed}` and data fields `lifecycleVersion`, `policyRevision`, `reinforcedAt`. Generation/counter fields use the shared decimal-string encoding. Preserve legacy text decoding per A4. Receipts contain numeric lifecycle results and identifiers, never summary/full text.

An explicit idempotency key yields exactly one committed feedback mutation for a matching normalized payload, including target, resolved namespace, quality, and caller-supplied expected generation. Reuse with another payload conflicts. Clock/config changes after the first commit do not change replay. Requests without keys are distinct events; tools must not advertise unconditional idempotence and clients must not auto-retry unkeyed feedback after ambiguous transport failure.

A1/A4 retain receipts for the lineage lifetime under `(storeId, namespaceId, key)` across operation kinds. There is no expiry or key reuse. Keyed requests require storeId; restore creates a new lineage and old-lineage requests conflict. Historical receipts survive as outcomes without recreating content. CONTRACTS fixes catalog exceptions and partial-batch behavior.

Replay of a pre-delete feedback receipt may return its original numerical result, explicitly marked replayed and historical; it never mutates the now-terminal record. New feedback after forget fails. Simultaneous sweep/feedback/forget uses generation checks: if feedback commits first, a stale sweep replans; if forget commits first, new feedback fails. A stale access task cannot recreate a missing record. Retry transient generation conflicts at most three times internally, then return conflict; do not overwrite newer state.

## 7. Retention phases, permastore, and deletion

Phase describes retained content, not current strength. Sweeps advance at most **one destructive phase per record per sweep** (compatibility default), even after a large clock jump. Repeated explicit sweeps may advance further. Projected phase is advisory until committed; reads never remove content. Boundaries use `<=` as in the current engine.

| Current state | Automatic sweep at/below threshold | Explicit feedback | Forget |
|---|---|---|---|
| Full | At 0.70: atomically remove full-text pointer/payload visibility and set Summary. Require a nonempty retained summary; if absent, skip with `missing_summary` diagnostic rather than destroy the only text. | Recompute stability, remain Full. | Terminal Tombstone. |
| Summary | At 0.30: strip summary and content-bearing tags/entity projections, set Ghost. | Recompute stability, remain Summary even if R=1. | Terminal Tombstone. |
| Ghost | At 0.05: A1 hard deletion of payload/vector/edges and record; retain only minimal deletion identity/receipt bookkeeping. | Conflict; it has no reviewable text. | Terminal Tombstone or equivalent terminal identity if already hard-deleted. |
| Tombstone/deleted identity | No decay/reinforcement/access transitions. | Conflict or not found for a purged identity. | Idempotent terminal outcome; never free a reused vector slot. |

Permastore and multiplier zero skip automatic transitions, including for an already degraded record. Permastore is sticky until a separately specified explicit administrative unpin operation; A3 does not add that operation. Raising thresholds/config changes do not clear existing membership. Lowering the permastore threshold does not bulk-promote on a read; promotion occurs on subsequent feedback. Forget overrides all exemptions.

No read, feedback, policy switch, backup replay, index rebuild, cache warm, or graph operation may promote Summary to Full or Ghost to Summary by inventing/reloading discarded content. A newly supplied observation may be stored under a **new UUID**; UUID-targeted store/import cannot reuse a terminal UUID. This prevents system resurrection, not a promise to recognize and reject every paraphrase a user explicitly stores later.

Forget removes full text, summary, tags/entities, embedding ownership, and search eligibility. A content-free graph identity/structural edges may remain for A2's supersession traversal; no deleted text or content-bearing edge metadata may be exposed through that route. Contract v2 forbids tombstone/ghost associative activation relay in both current and history modes. Content-free tombstone identities can be intermediate Supersedes-chain nodes only, never seeds/results or inherited-score origins. V1 retains labeled legacy structure behavior, never deleted payloads. Hard deletion removes that graph structure too. Supersession alone changes neither retention phase nor deletion status; A2 owns historical-result eligibility.

A1 records terminal identity or equivalent anti-replay protection for deleted UUIDs even when the content record is purged. All feedback/access/sweep workers are update-only and cannot upsert absent records. Forget is logical removal, not forensic erasure; historical logs and backups have A1-documented retention. Retained-state recovery merges all available compatible removal-ledger entries before serving, preserving terminal identities and removed-field masks and rejecting stale queued writes. An unreadable target ledger or unknown ancestry fails closed. A1 isolated historical restore is fresh-destination-only, read-only and excluded from ordinary serving; coverage is only through the snapshot/supplied ledger. If all newer deletion state is lost, later deletions cannot be inferred. Separate explicit promotion then acknowledges possible reappearance of later-deleted content; this is data-loss recovery, not normal rollback or an anti-resurrection guarantee.

## 8. Migration and compatibility

A1 assigns the physical format version; the existing `DiskRecord` decoder recognizes only v1, so do not append fields while claiming v1. Migration is offline or uses A1's exclusive canonical migration boundary, with validated backup, atomic manifest/version publication, and index rebuild.

For each legacy record preserve UUID, namespace, content presence, stability, difficulty, phase, permastore, and old `last_accessed_at`. Set `decay_anchor_at` to old last access and policy to v1. Set `observed_at=created_at`, `timestamp_origin=legacy_inferred`, `lifecycle_updated_at=max(created_at,last_accessed_at)`, and `phase_changed_at=migration_now` where historical time is unavailable. Derive retrieved/reinforced timestamps only from matching typed events in available bounded access history; if no such evidence exists, leave null. History truncation is not proof that feedback never happened. Keep the old access value as a compatibility field; never label it confirmed reinforcement.

V1 writes maintain legacy `lastAccessedAt` as the latest exposure/feedback time. V2 compatibility responses use `max(created_at,retrieved_at,reinforced_at)` for that deprecated field; lifecycle code cannot read it as an anchor. New fields are additive on the wire, with A4 version negotiation for strict consumers.

Switching a namespace to v2 is an explicit, receipt-backed admin migration, not an incidental query. Under an exclusive namespace mutation boundary, retain every record's current `decay_anchor_at`, stability, and retained phase; change the policy version/revision atomically for the namespace and records. Do not backdate the anchor to an inferred reinforcement event or reset it to migration time. This avoids a one-time artificial aging or rejuvenation. Preview current-time eligible phase counts and verify no payload change; resume sweeps only after derived generations are ready. New records then follow v2. Unknown policy/schema versions fail closed for writes/sweeps.

Rollback uses a schema-aware binary with v1 policy support and an explicit namespace switch; preserve anchors, timestamps, terminal identities, and content already lost. It cannot restore discarded content. Never open migrated data with the old v1-only binary. If a physical-schema rollback requires an older snapshot, A1 must first reconcile post-snapshot committed writes/deletions; otherwise keep it isolated. Turning off sweeps is the immediate reversible containment action for unexpected degradation.

## 9. Concrete implementation touchpoints and dependencies

| Touchpoints | Required change and owner boundary |
|---|---|
| `src/time/mod.rs`, `src/system.rs`, `src/test_support.rs` | A3 clock injection; A6 manual clock/reset. Keep display timezone helpers. |
| `src/model/memory.rs`, `src/model/record.rs`, `src/model/decay.rs`, `src/model/namespace.rs`, `src/model/constants.rs` | Logical lifecycle fields and phase invariants; A1 owns encoding/version migration. |
| `src/decay/mod.rs`, `config.rs`, `fsrs.rs`, `storage_adapter.rs`, `sweep.rs` | Shared effective policy, pure projection/feedback plan, explicit anchor, generation-checked atomic phase transitions. Replace scan-time stale writeback. |
| `src/config/types.rs`, `src/config/loader.rs` | Validated policy versions and overrides, global config revision snapshot. |
| `src/storage/metadata.rs`, `src/storage/engine.rs`, `src/storage/vectors.rs`, `src/index_cleanup.rs` | A1 applies lifecycle mutation + receipt atomically; safe slot ownership and canonical-to-index publication. No second A3 storage subsystem. |
| `src/search/query.rs`, `pipeline.rs`, `adapters.rs`, `response.rs` | A2 propagates recordAccess through search/direct/batch paths, uses A3 projections, awaits exposure writes, returns snapshot/policy diagnostics. |
| `src/mcp/args.rs`, `tools.rs`, `bridge.rs`, `bridge_adapters.rs` | A4 strict requests, feedback extensions, accurate hints, shared planner calls. |
| `src/api/models.rs`, `state.rs`, `handlers.rs`, `adapters.rs`; `src/daemon/protocol.rs`, `server.rs`, `client.rs`, `bridge_adapters.rs` | Carry identical semantics and keyed retry rules end to end; remove duplicate feedback math. |
| `src/rif/engine.rs`, `src/search/adapters.rs`, `src/rif/config.rs` | Keep RIF request-local; A2 owns ranking fixes, A5/A6 evidence gate default changes. |
| `src/backup/manifest.rs`, `restore.rs`; `src/cache/`, `src/graph/` | A1 versions/restores lifecycle data and terminal identities; A2/A5 prevent stale projections/caches from exposing discarded payloads. |
| `src/bench/locomo.rs`, new isolated lifecycle integration fixtures | A6 explicitly disables access recording and background sweeps for fixed-state runs; chronological runs advance time and invoke declared actions. |

Dependency order: implement the agreed A1/A4 receipt and schema interfaces → land A3 pure clock/policy/transition code and A1 persistence safeguards → route every transport and query branch through it → A6 invariant fixtures and migration/restore replay → opt-in v2 canary → measured default gate. A5 may instrument in parallel but cannot substitute performance results for correctness. A2 ranking work can use a fake pure projection until A3 lands; it must not invent feedback semantics.

## 10. Acceptance tests and measurement gates

These are required future tests, **not executed results**. Use synthetic temporary stores and a controlled clock; exercise the actual Rust paths and at least MCP plus HTTP parity. No personal corpus or credentials.

| Test | Required observable result |
|---|---|
| Exposure vs feedback | At day 0 create; at day 7 retrieve; at day 8 rate 3. V2 rating produces the same stability as a no-retrieval clone rated at day 8. V1 retains its day-7 anchor behavior. Retrieved/reinforced timestamps remain distinct. |
| Strict read-only | Search, ID fetch, batch fetch, graph expansion, and replacement paths with false leave canonical records, events, receipts, graph weights, and pending-write count unchanged before/after a drain barrier. Repeating at the same clock/snapshot yields identical ranked results. |
| Observation completeness | True mode records each returned UUID once, no nonreturned candidate, and has no pending access work at successful acknowledgement. Zero results commit no access event. |
| Clock determinism | Repeat the same action/time trace from the same snapshot through each transport; logical state hashes match. Clock regression cannot move lifecycle timestamps backward; a large jump remains finite and nonnegative. |
| Namespace overrides | Same initial state under multipliers 0/1/2 produces R=1/normal/slower respectively; null inherits global. Custom thresholds/difficulty/permastore settings affect sweep and feedback identically. Invalid floats/order fail with zero writes. |
| Phase boundaries | Test exactly at and just above each configured threshold, with and without connection bonus. One sweep advances at most one phase; no automatic upgrade. Missing summary skips Full degradation with a counted error. |
| No resurrection | Reinforce Summary to R=1; full text remains absent through read, restart, cache warming, index rebuild, and backup round trip. Ghost feedback fails and tags/text remain absent. |
| Permastore | Threshold crossing sets flag once; quality 1 can lower stability but keeps exemption; forget still removes content. Config change and read-only queries do not silently promote/demote. |
| Retry/crash | Two simultaneous identical keyed ratings, transport retry, and restart replay apply one event and return one operation/generation. Inject interruption before/after commit: either neither mutation nor receipt exists, or both exist. Different payload conflicts. Lifetime keys never become new feedback; stale restore lineage conflicts. |
| Deletion races | Race feedback/access/sweep against forget at controlled barriers. Later writes cannot recreate content or reallocate/free another memory's slot. Retry old receipt is historical only; new feedback fails. |
| Stale sweep | Pause after scan, reinforce or change namespace policy, resume. Generation mismatch forces replan and cannot overwrite stability/phase or apply old thresholds. |
| Migration | Mixed Full/Summary/Ghost/Tombstone, truncated history, and null overrides survive round trip with no invented feedback. Opt-in v2 preserves anchors and payload availability. Unsupported version fails closed. |
| Restore and rollback | Retained-state restore merges later terminal identities/removal masks before serving. With newer state absent, historical restore stays isolated and reports unknown later coverage; promotion requires explicit data-loss acknowledgement. Schema-aware rollback preserves all available deletion and irreversible phase loss. |
| RIF boundary | With either recordAccess setting, all RIF modes make zero persistent stability/phase writes. Pin legacy config under contract v1; assert off/shadow rank identity under contract v2. |
| Protocol validation | Missing quality is 3; malformed present values fail. Wrong namespace, stale generation, and terminal target have declared errors with no hidden writes. |

Correctness release gate: every invariant above passes, including A1 fault/recovery checks; no content resurrection, cross-namespace mutations, or unacknowledged partial feedback. An existing unit suite alone is insufficient.

V2 default-promotion gate: A6 runs paired chronological episodes comparing v1, v2, no decay, and a simple age/budget policy with matched retriever/answerer and storage/context budgets. Report evidence retention, stale-answer rate, answer accuracy, deletion violations, bytes retained, explicit feedback volume, and per-action latency. Proposed decision rule: zero hard-constraint failures; lower bound of grouped paired 95% interval for v2-minus-v1 answer accuracy at least -1 percentage point; plus either at least 10% fewer retained bytes at matched accuracy or at least 2 percentage points lower stale-answer rate at matched bytes. A5 checks p95 query/feedback latency regression <=10% under its declared workload/SLO; absolute SLO ownership stays with A5. Freeze dataset, groups, minimum sample plan, and budgets before collection. Inconclusive intervals mean keep v1 default, not success. No such result is asserted here.

Enabling RIF for contract v2 follows A2's separate ranking gate and A6 sample plan; v2 remains off until it passes. A broader future RIF default change also requires actual-engine off/current/deduplicated-and-capped runs with results stratified by direct/entity/graph/metadata origin, cumulative suppression and useful-evidence recall, then paired chronological utility evaluation. Any proposal to persist RIF stability changes requires a new lifecycle version, explicit receipt/event semantics, and the same deletion/read-only tests; it is out of scope for this implementation. Do not enable persistence merely to make the engine match an academic description.

## 11. Rollout and empirical gates

Ship first the schema-aware compatibility release: read-only control, shared clock/planner, strict feedback validation, receipts, atomic terminal/content transitions, and diagnostics, with lifecycle v1 default, legacy RIF under contract v1 and RIF off under opt-in contract v2. Validate migration on isolated copies with A1's recovery process. Enable v2 only for explicitly selected test/canary namespaces, review policy preview and retention counters, then apply the default gate above. Emit counters for exposure writes, feedback applied/replayed/conflicted, phase transitions/skips, stale-sweep retries, clock regressions, and policy versions; never put memory text into metric labels.

For an unexpected degradation spike, stop sweeps immediately, keep serving canonical retained content, and disable new v2 enrollment. Switch affected namespaces back to v1 with the schema-aware admin operation if needed. Do not reverse content removal or restore a pre-delete backup into service as a quick rollback.

CONTRACTS revision 1 settles lifetime receipt scope, physical/schema version names, removal-ledger restore coverage, default-true exposure, policy-specific strength, and independent API/RIF defaults. A1 persists the agreed A3 fields; A4 propagates them through all transports; A2 consumes the selected strength source. No later integrator choice is required.

Remaining release gates are actual migration/restore/race/read-only conformance and A5/A6 measurements under the frozen grouped protocol. A6 supplies at least 100 independent groups, 20 query events each and its declared stratum coverage; A5 records hardware/workload/absolute budgets before candidate measurements. Inconclusive utility or latency results keep lifecycle v1 as default and contract v2 RIF off.

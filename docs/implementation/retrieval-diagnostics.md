# Internal channel observations and returned provenance

Child of corrected FTS merge `f2a5ba5`, which includes `57e0196` and the independently validated reciprocal-edge fix `3b28898`. The original diagnostics branch was fast-forwarded before source edits. This slice adds observations to internal semantics2; it does not expose a transport v2 or certify A1 coherence/readiness.

## Channel observations

`SemanticsDiagnostics.channels` is `Some(map)` for internal v2 and `None` for v1. The map always names Vector, Fts, Entity, Metadata and Graph. `ChannelParticipation` distinguishes `NotRequested` from `Completed`; completed means the operation returned, not that A1 certified publication. V2 source errors continue to fail the query. V1's existing FTS fallback/diagnostic stays separate and unchanged.

`offered` counts distinct IDs returned by a source interface before union/deduplication. `admitted` counts its eligible members in the candidate pool after the existing channel allowance, before downstream fusion/minScore/version collapse and final result truncation. Membership overlaps: an already-present vector/FTS candidate can also be observed by the entity or graph channel. Entity admission retains the parent's existing cap on novel entity-only candidates; instrumentation does not redefine that policy or its ranking effect.

`returned_own` counts returned UUIDs observed directly in that channel. `returned_inherited` counts returned UUIDs with resolved predecessor evidence from that channel, including evidence below the winning inherited floor. These counts overlap both across channels and with each other; their sum is not `totalMatches` or a corpus-wide hit count. A zero returned count does not erase earlier admission.

`examined`, `eligible`, `exhausted`, `candidate_limited` and `budget_exhausted` are optional observations. None means unknown/unmeasured, never zero or exhaustive:

| Channel | Known source coverage | Intentionally unknown |
| --- | --- | --- |
| Vector | Interface output and admitted/returned counts | Exact scan examinations/eligible-vector count, exhaustion and cap/work status; returning B does not prove exhaustion |
| FTS | Existing refill coverage, including charged lookahead and the 12,800-row budget | No new inference beyond the parent's coverage |
| Entity | Interface output and admitted/returned counts | Total postings examined, eligible source matches and source exhaustion/cap status |
| Metadata | Full eligible enumeration, ranked-B admission, completed source enumeration, candidate cap, no examination ceiling | Count of canonical rows examined/rejected before eligibility |
| Graph | Expansion output, admitted/returned members, existing state/edge budget hits | Record examination/exhaustion; edge counts remain explicitly in `diagnostics.graph`, not mislabeled as candidate counts |

Metadata source exhaustion can coexist with candidate limiting: the full eligible set was enumerated but only B ranked candidates entered version resolution. Graph candidate limiting is marked true when work limits fire; absence of those limits does not certify exhaustive retrieval. No extra source reads, source-trait changes, generation counters, or timing fields are introduced. Observational work is included in existing stage timings and requires its own performance measurement.

## Returned provenance

`SemanticsDiagnostics.provenance` is a v2-only map whose keys are exactly the returned UUIDs. Each `ResultProvenance` separates:

- own observed channel set and own raw vector/FTS/fused relevance;
- own graph activation and observed path depth;
- own baseline score including boosts before inheritance;
- inherited channel set and maximum inherited score floor;
- final score.

An injected current terminal has no own raw score or own baseline score unless independently admitted evidence exists. Predecessor cosine/FTS/activation is never relabeled as the terminal's own evidence. History mode records no inheritance. Graph observations overlapping a direct candidate are reported but do not activate the graph-ranking formula for that candidate: the existing direct score stays in force. Thus channel membership means observed evidence, not an additive scoring contribution.

The inherited floor includes the predecessor's already-computed score; boosts are not reapplied. Tests reconstruct the final score as `max(own_score or 0, inherited_floor or 0)`, including injected, weaker-own and stronger-own terminals, graph-derived replacements, and repeated resolution around graph expansion. Own graph depth stays separate from the existing minimum contributing depth, which may come through inheritance.

No predecessor/path IDs, rejected IDs, labels or foreign namespace identifiers enter these structures. Only aggregate counts survive for candidates that fail eligibility/version resolution or final truncation. This is bounded returned provenance, not the full A2 explain envelope: minScore rejection detail, factor-by-factor score decomposition, predecessor details, coherent read generations, lifecycle-policy provenance and all-source examination coverage remain pending.

## Integration and validation

Graph traversal, ranking formulas, source selection/caps, cache/access policy, metadata adapters, query fields and public response serialization are unchanged. The reciprocal-edge correction belongs to the semantics parent and was independently rereviewed; this branch contains no graph traversal edits. The separately published entity-postings branch still needs its namespace-argument fixture integration; this branch adds no index calls. Preserve the read-only branch's shared access guard when merging pipeline changes. Evaluators must treat None as unknown and channel counts as overlapping.

Validation runs in `recalld-ci:1.94`, Rust 1.94.1, `--network none --cpus 2 --memory 4g`, `CARGO_BUILD_JOBS=2`, and private cargo/target scratch, with no home, credentials, daemon socket or personal data mounts. The corrected FTS parent was separately validated at **535 passed** before diagnostics tests. Build outputs remain untracked in `.validation`.

- `cargo test --locked --offline --all-targets --features bench --quiet`: **540 passed**, zero failed (five added tests); both binary targets compiled with zero tests.
- Changed-file rustfmt checks passed. `cargo clippy --locked --offline --all-targets --features bench` completed with repository warnings; no warnings-as-errors clean claim.

New regressions exercise real vector/entity/FTS/graph adapters: overlapping direct channels before deduplication; observed graph overlap without score changes; own versus inherited evidence through current/history and repeated resolution; graph-derived replacement provenance; final truncation; exclusion of filtered/foreign/unresolved IDs; unknown vector exhaustion at a full cap; and measured metadata exhaustion plus candidate limiting. Full A1 readiness/coherence and performance/quality gates are not established by these tests.

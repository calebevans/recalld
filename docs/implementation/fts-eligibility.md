# Internal eligible FTS refill

Child of `35291e0` (`impl/retrieval-semantics`, itself based on PR #17). This is the FTS slice of A2, not full A2 completion or a transport-v2 rollout. CONTRACTS and INTEGRATION-REVIEW remain authoritative. V1 remains the default.

## Behavior

Internal `search_with_semantics(..., V2(...))` now calls `FtsIndexRegistry::search_eligible`. The production adapter holds its existing FTS mutex across one SQLite statement, consuming a stable score-descending/UUID-bytes-ascending cursor in logical pages of `B=min(1600,max(100,8*L))`. SQLite's native cursor retains its read view across pages; there are no shifting OFFSET queries or rounded-f32 score cursors. Canonical eligibility membership is applied before admitting at most B matches. The schema guarantees unique UUID rows, so rejected rows and repeated upserts cannot spend the admission allowance twice.

The SQL stream and application consumption are capped at 12,800 rows. Sorting/scoring matching documents inside SQLite can still inspect more rows: this is an examined-candidate bound, not a bound on SQLite VM instructions, total CPU or latency. The cursor avoids repeated rescoring across refill rounds. No ranking weights, tokenization, query escaping, BM25 parameters, or fusion formulas change.

`SemanticsDiagnostics.fts` exposes internal operation availability (`NotRequested`, `Available`, `Unavailable`) and optional `FtsCoverage`:

- `examined`: distinct source rows consumed, including one possible lookahead after the admission cap;
- `eligible`: consumed rows passing canonical membership, including an eligible lookahead;
- `admitted`: eligible rows retained for fusion, at most B;
- `pages`: logical page reads started, including an empty final page needed to prove EOF;
- `exhausted`: true only after observing end-of-stream;
- `candidate_limited`: admission/work stopped collection without proving source exhaustion; conservative when remaining eligibility is unknown;
- `budget_exhausted`: the 12,800-row examination allowance was consumed without proving exhaustion.

The lookahead is charged to the same examination budget. At exactly 12,800 matches, coverage conservatively reports budget exhaustion rather than performing an uncharged extra EOF read. With fewer matches, EOF distinguishes exhausted underfill from budget-limited underfill. Filling the requested final result limit never establishes exhaustive retrieval. The existing `result_limit_applied` separately reports final truncation. FTS counters precede fusion/minScore/version collapse; they are not corpus-wide hit counts or unique counts across channels. Counts expose no rejected IDs.

Both FTS adapter methods propagate SQLite failures. V2 fails the query on FTS failure, including when other candidates exist; optional degraded partial serving is deferred until A1 can certify independent ready views. Unsupported eligible-refill adapters fail explicitly rather than silently reusing legacy top-K. Corrupt UUIDs and nonfinite scores are errors. Existing adapters implementing only legacy `search` continue to compile via the new method's error default.

V1 deliberately retains top-B-before-filter collection and its existing fallback. The fallback is now at the pipeline boundary, with a warning and internal `Unavailable` diagnostic. A successful legacy call reports `Available` with coverage absent, because top-K cannot prove exhaustion or examined counts. Metadata-only queries report FTS `NotRequested`. Availability describes the operation outcome, not generation readiness. Internal diagnostics remain non-serde, and public response/query fields do not change.

## Integration

- Parent semantics branch stays stable; this branch changes FTS collection and its internal diagnostics only.
- Read-only recall retains ownership of cache/access guards. The shared access call and query fields are unchanged here.
- `impl/namespace-read-batch` retains ownership of `namespace_records`, list/get-batch adapters, and the initial canonical call. Refill consumes the resulting eligible UUID set without changing enumeration.
- The explicit-vector branch's stage-2 embedding changes are independent; FTS still chooses `ftsQuery` before `text`.
- No StageTimings fields are added. `ftsSearchUs` now includes refill. Evaluators can read `diagnostics.fts.coverage` through the internal entry point; `None` means unknown/not-collected coverage, not zero.
- `graph/autolink.rs`, entity postings, entity counts, lifecycle, transports, and writer/publication paths are untouched.

## Validation and remaining work

Validation uses Rust 1.94.1 in `recalld-ci:1.94`, `--network none --cpus 2 --memory 4g`, `CARGO_BUILD_JOBS=2`, dedicated copied cargo/target scratch, and no home, credential or socket mounts. Build outputs remain untracked in `.validation`.

- `cargo test --locked --offline --all-targets --features bench --quiet`: **533 passed**, zero failed (7 added tests); both binary targets compiled with zero tests.
- Rustfmt checks passed for every changed Rust file.
- `cargo clippy --locked --offline --all-targets --features bench`: completed with repository warnings; no warnings-as-errors clean claim.

Regression coverage uses real SQLite FTS5 and the engine's production FTS adapter: selective filters (missing canonical records, foreign scope, tombstones, ghosts, strength and required tags), explicit FTS text, current/history parity, v1 compatibility, stable ties and upserts, source/candidate/work boundaries, real SQLite errors through both adapter and engine, metadata-only nonparticipation, and concurrent atomic namespace swaps while refilling a pinned cursor. The concurrent fixture validates each query against a whole SQLite view; it does not establish cross-store coherence.

A1 still must provide a coherent canonical/FTS/vector/graph generation and publication/readiness barriers. The canonical membership set is not pinned with this SQLite snapshot. No transport v2, generation guarantees, full channel coverage/explanations, or performance promotion follows from these tests. A5 must measure refill latency/RSS and the independent canonical batching optimization; existing parent measurements do not pass a promotion gate.

Next reviewable A2 slices: namespace-partitioned entity postings plus idempotent distinct counts and all writer/rebuild callers; remaining per-channel/provenance explanations; integration with A1 readiness and lifecycle policy; transport/oracle conformance. The optional RIF experiment stays gated and off in v2.

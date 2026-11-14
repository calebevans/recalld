# Similarity wire and exposure regression

The original evidence is unchanged: [combined fixture2 report](../combined-aeab9e6/REPORT.md), [exact wire findings](../combined-aeab9e6/wire-parity-findings.json), and both original public-runtime archives. On frozen `aeab9e68e749e4e178576c2283a6173c11611a31`, REST requested `minScore: 0.1` and returned copper 0.8, birch 0.0 and olive 0.0. There were 12 offending REST responses with 21 below-threshold hits. MCP returned copper 0.800000011920929 with `namespace: ""` in 24 responses. Actual UUIDs, request steps and raw responses remain in those archives; no failure was rewritten.

The contract is not redefined. `FindSimilarRequest.min_score`, `docs/mcp.md` and `docs/guide.md` specify minimum similarity for single-mode similarity. `/search` uses a separate `minStrength`. The old REST adapter field named `min_score` actually feeds strength eligibility, while pipeline score minimum is zero; passing the similarity request through that field was wrong. Globally remapping that field would break search strength filtering.

## Child and shared fix

Runtime fix `c9e7d3dc9cddd2f9d3dc08c8169df3942225c82b`, branch `impl/public-similarity-parity`, is a direct child of the exact measured combined parent aeab9e6. That parent supplies the scope assertions, explicit-vector validation, read-only behavior and effects admission exercised by this suite. No evaluation branch was merged into the runtime. The frozen checkout was not edited. Harness/evidence branch `study/public-similarity-parity` is a separate child of `525389d`; its source-root runner builds the runtime child explicitly.

Both transports now use QueryEngine similarity: canonical source namespace, validated vector, source/score/limit selection before access, canonical response namespace, and exposure of final returned IDs only. REST no longer exposes `limit+1` ordinary-search candidates then removes the source; MCP no longer post-filters already-observed neighbors. Search strength semantics stay unchanged. REST similarity now follows cosine ordering and associative access consistently with MCP, rather than the general-search ranking path.

## Declared fixture3, same input in red and corrected replay

Fixture3 adds one foreign-scope neighbor to fixture2 and six true-mode cases per episode: threshold, limit and intended-empty response, each for default and foreign source scope. By-ID namespace is omitted in these new cases to test canonical scope rather than the connection default. Existing false-mode vector/similarity, malformed-input, feedback, deletion, restart and strict wrong-quality checks remain. The added REST strength-filter probes require both source and copper at `minStrength: 0.5`, and no results at 1.1 (the existing REST interface accepts this above-maximum cutoff). A canonical Rust regression separately uses strength 0.01 with cosine 0.8 to prove score and strength are independent.

True-mode public observations use the actual clock, a declared 1.1-second pre-action wait, at most five seconds polling for returned-record changes, and a 200ms quiet observation. They compare all public records, preserve file hashes and raw requests/responses, and restart again after exposure. Canonical event counts are checked separately by the real Rust adapters/handlers with an injected clock. Public GET does not expose the full canonical event/receipt store; public anchor observations are not presented as exact event counts.

### Baseline replay

Fresh isolated scratch `/tmp/recalld-similarity-red-rlnm2khv/output` reused the binary built and validated in the original fixture2 run, SHA-256 `10fb59f1562cb8ac2a56b0bb5fe389db77abcd63fb5e447e0857f35e69e4d063`. No new baseline compilation is claimed. All nine episodes completed, three each for stdio MCP, HTTP MCP and REST, with `conformancePassed: false`. The strict checker exits 1 on these results.

| Exposure observation | MCP stdio, 3 episodes | HTTP MCP, 3 episodes | REST, 3 episodes |
| --- | ---: | ---: | ---: |
| True-mode cases | 18 | 18 | 18 |
| Changed IDs differ from expected returned IDs | 9 | 9 | 18 |
| Intended-empty cases write durable state | 6 | 6 | 6 |

For default-scope threshold after deletion, only copper should be exposed. MCP changed copper and zero-score olive; REST changed source amber, copper and olive. With limit 1, REST still exposed amber as well as the returned copper. With an intended-empty threshold 0.99, MCP returned no hits but exposed neighbors; REST returned below-threshold neighbors and exposed its source too. Foreign source checks reproduced the source/empty-response problems in that scope. Exact UUIDs and state differences are in each `exposure-window.json`, with full wire traffic in `trace.jsonl`.

### Corrected replay and validation

Corrected scratch `/tmp/recalld-public-conformance-iezeyage/output` completed all nine episodes with `conformancePassed: true` and zero violations. All 54 true-mode cases passed; only the expected returned record changed, and all 18 empty-result cases preserved durable state. Both default and foreign response namespaces were correct. All 36 false-mode similarity calls respected minScore and canonical namespace, with unchanged public records and durable hashes; 12 embedding-only REST queries and 12 independent REST minStrength probes passed. All nine wrong-type quality requests rejected with identical public records and **all** captured file hashes. Each episode passed its ordinary restart and subsequent exposure restart.

Actual isolated validation passed **673 library tests + 1 CLI integration test + 2 example tests + 7 Python runner tests**, fmt check, all-target/all-feature Clippy and production debug binary build. Clippy retained existing warnings (323 library; 331 library-test including duplicates; one binary). No doctest claim. The new real-adapter regression checks exact canonical access-history increments for both handlers; false and empty modes emit no event, and excluded records remain byte-identical. The separate strict evidence checker exited **1 on baseline, 0 on corrected**.

Both runs used identical fixture3 and driver bytes: fixture SHA-256 `74845b1e71ac6afd191e272d00cc43b3a0b331fef287a839749f2a68fd1384ee`; driver `1734cf73ec961c1020bf75284534ca7afd05babce8d0ce7ffb54c61692ec6c12`. Each produced 2,238 wire events, 54 accepted fixture records and 144 provider requests (135 exact fixture hits plus nine deliberate fail-closed misses). These are synthetic contract probes, not semantic quality measurements. All 147 source input hashes per run and all output hashes (153 baseline, 164 corrected) were independently verified. Prior tracked evidence bytes were compared against `525389d` and matched exactly.

Both used image `sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`, Rust 1.94.1, Python 3.11.2, network-none Docker, two CPUs, 4GiB, read-only container root, temporary HOME and no personal/socket/credential mounts. Build caches were copied into private scratch. `CARGO_BUILD_JOBS=2`; Rust test target linking used `-j 1`. Exact Docker commands, resource observations, source manifests, target inventory, raw logs, wire traces and script inputs are archived. A preliminary compile error from using the wrong existing response-type alias is retained under the corrected archive's `development/`; the final full suite validates the corrected alias.

[Baseline archive](baseline-aeab9e6.tgz) and [corrected archive](corrected-c9e7d3d.tgz) include bounded source snapshots, outputs and checker logs. Archive sizes and SHA-256 values are in [artifacts.json](artifacts.json). No binary, Cargo registry, target cache or personal data is committed.

## Reproduction and limits

Use the repository Docker recipe and the pinned image reported in each manifest. A fresh full run of either runtime is:

```sh
python3 scripts/public-conformance.py --fixture-version 3 \
  --source-root RUNTIME_CHECKOUT --source-commit EXACT_RUNTIME_SHA \
  --source-input tests/text_migration_cli.rs \
  --source-input examples/offline_eval.rs \
  --cache ISOLATED_CARGO_CACHE --build-cache ISOLATED_TARGET_CACHE
python3 scripts/check-public-similarity.py PRINTED_SCRATCH/output
```

Run the checker inside the same isolated Docker image with only the synthetic scratch mounted. It verifies output hashes and all nine episodes, checks raw expected/returned/changed IDs and exposure restart equality, and fails on any recorded violation. The characterization runner retains failures rather than silently excluding episodes; the checker supplies the regression exit status.

No performance matrix or latency improvement claim. No personal paths, credentials, external model calls or outbound network. This is legacy API v1 with actual clock and bounded async observations, not full A6 or API-v2 conformance. AccessRecorder remains asynchronous and lacks an A1 receipt; failure logging, query/exposure admission races and concurrent deletion need the separately coordinated atomic lifecycle/storage path. Curie confirmed no overlapping similarity work; Hume was notified of the shared-helper change and his pending explicit-lifecycle write guard. No receipt, full-state barrier/snapshot/reset, lifecycle-v2 serving, removal-ledger or online completion claim.

## Changed files and follow-up

Runtime child changes `src/api/{state,adapters,handlers}.rs`, `src/mcp/bridge_adapters.rs`, `src/search/{pipeline,adapters}.rs`, `src/daemon/read_only_tests.rs`, and `docs/benchmarks/similarity-transport-fix.md`. Evaluation child changes `scripts/public-conformance.py`, adds `scripts/check-public-similarity.py`, `fixture-v3.json`, this report and the evidence archives/index. Earlier evidence remains unchanged.

The existing [handoff plan](../HANDOFF.md) remains the follow-up for versioned manifest/trace schemas and A1 barrier/snapshot/reset plus A3 clock controls. True-mode receipt completion must include participating writers and a fail-closed explicit-lifecycle guard; integrate Hume's guarded projection work separately. Preserve source/score/limit selection before any exposure when doing so. No additional performance study is proposed.

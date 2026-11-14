# Minimal-parent similarity revision and canonical REST metadata

This is fresh bounded validation, not a relabeling of [b2's combined-parent evidence](../similarity-parity/REPORT.md). Runtime `c9e7d3d`, evidence `b2dad0f`, their raw archives, and frozen runtime aeab9e6 remain unchanged.

## Dependency plan and actual ancestry

Runtime `98179a4458b6c3c232df1d997ba31afe29551b31`, branch `impl/public-similarity-minimal`, uses only these necessary heads and their ancestors:

| Parent | Exact commit | Required behavior |
| --- | --- | --- |
| PR37 | `89e0cf3bd75c263b6e246733800fc3057e9f9735` | Canonical scope assertions combined with explicit read-only behavior |
| PR28 | `f02212764ce83d129bbc4461195e9b986d72f080` | Validated explicit vectors and successful REST vector/similarity path |
| PR39 | `cdee36e51520a9dde72984bec9a99be06a71435f` | Read admission, stale-cache rejection and online derived-effects completion |

Join commits are `fe48bdb` (37+28) and `6bf71fe` (+39); `1d08bc4` transplants c9's filtering fix; `f7cc353` corrects metadata/error handling, adds regressions, and finishes formatted parent/test adaptations. `98179a4` aligns the second inherited scoped-HTTP success assertion with PR28. No PR17 API is needed: similarity uses existing metadata point/batch reads and the older three-argument namespace vector search. [Machine-checked ancestry](ancestry.json) proves all three required heads are ancestors and PR17, namespace batching, combined aeab9e6, compaction, entities, removal ledger, schema migration, transactional-observation, c9 and b2 are not runtime ancestors.

Parent conflict resolution retains supplied vectors and access/scope arguments, both clock injection and completion handles, lifecycle expected-state guards, and effects-backed Ghost deletion before graph publication. PR39 tests were adapted to scope/access parameters and a valid source vector; the old HTTP-similarity-error test now requires PR28's successful read-only result. These are explicit join adaptations, not imports from unrelated feature branches.

## Metadata and failure correction

REST now uses `MemoryResponse::from_cached` on each actual canonical record. It preserves nondefault difficulty, separate raw strength/decay strength, timestamps, tags/derived tag metadata, stability, phase, permastore and edge counts. Requested embeddings and real full text remain present. It does not construct difficulty 5.0 or use a possibly adjusted search projection as canonical decay metadata.

REST similarity prepares selection without exposure, hydrates every result with fallible full-text reads, rechecks its original admission stamp, then calls the shared `record_similar_accesses(&[MemoryId])` hook once. The shared hook deduplicates and awaits the existing associative batch recorder. Core/MCP similarity uses the same hook after final source/score/limit selection. No second transport recorder is added. Hydration errors return an error and cause no exposure, including original true-mode requests; absent text and unreadable text are distinct.

The new canonical fixture uses difficulty **7.25**, strength **0.23**, decay strength **0.67**, stability12.5, actual stored full text and requested embeddings, with a deliberately stale cache. It compares the complete REST memory JSON against the canonical helper serialized through the real JSON encoder. The corrupt-text fixture preserves readable metadata/vector but points outside the text file, and requires failure with unchanged state/events in false and true modes.

On the transplanted implementation, both tests failed: difficulty was5.0 rather than7.25, and an unreadable text reference returned a successful missing-text payload. Exact failing logs and source bytes are retained. An initial parent-compilation attempt needed older test call signatures adapted; another test attempt used serde_json::to_value, which promotes f32 to f64 rather than matching the HTTP encoder's shortest decimal. The assertion was corrected to serialize/parse the expected helper response through the actual wire JSON encoding, retaining independent numeric assertions. None of these attempts is counted passing. The first full f7 run passed598/599 library tests: the sole failure was a remaining PR37 assertion expecting HTTP400 before PR28 vector support, while the actual valid response was200. `98179a4` changes only that expectation; the full failure log remains in a separate archive.

## Fresh bounded validation

Final scratch `/tmp/recalld-public-conformance-u9n54dm9/output` completed on exact runtime `98179a4458b6c3c232df1d997ba31afe29551b31`:

- **599 library tests passed**, including canonical metadata, full-text failure in both modes, returned-only exposure, scope/daemon and online completion tests. Both binary test targets completed with zero tests. This minimal package has three Cargo targets: one library and two binaries; no unrelated CLI34/example-test coverage is claimed.
- **Seven runner tests passed**, fmt check, all-target/all-feature Clippy and production debug binary build passed. Clippy retained existing warnings; no doctest claim.
- **Nine public episodes passed**, three each over stdio MCP, HTTP MCP and REST, with zero conformance violations and two successful restarts per episode.
- **All54 true-mode cases passed**, including both namespaces, source/threshold/limit exclusion and18empty-result cases with no durable writes. All36false-mode similarity calls respected minScore and canonical namespace;12vector-only REST calls and12independent strength-filter probes passed. All9wrong-type quality requests rejected with exact public fields and all captured file hashes unchanged.
- The unchanged strict checker passed in offline Docker. There were2,232wire events,54accepted synthetic records,144provider requests including9deliberate fail-closed misses. These counts are diagnostics, not semantic retrieval or performance claims.

All tests/replays used Rust1.94.1 aarch64, Python3.11.2, image `sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`, Docker network none,2CPUs/4GiB, private copied Cargo/target,2build jobs with test-target linking `-j 1`, read-only root and temporary HOME. No personal/socket/credential mounts. All136source input hashes and164output hashes were verified, including the packaged archive. Driver SHA-256 remains `1734cf73ec961c1020bf75284534ca7afd05babce8d0ce7ffb54c61692ec6c12`; fixture3 remains `74845b1e71ac6afd191e272d00cc43b3a0b331fef287a839749f2a68fd1384ee`.

[Completed validation and raw public replay](validated-98179a4.tgz), [two metadata failures and development attempts](metadata-development.tgz), and [superseded f7 full-run attempt](f7-validation-attempt.tgz) are separate artifacts. [Archive hashes/sizes](artifacts.json) pin each. Exact baseline and corrected source bytes are included without Cargo/target caches. The original b2 tracked evidence and harness inputs were byte-compared against that commit and remain unchanged.

## Reproduction, scope and handoff

The same fixture3 and runner bytes as b2 are used. No performance matrix, model calls or new dataset evaluation. Exact source manifest, raw requests/responses, isolated resource limits, hashes, test/build logs and checker output are preserved in the new archive. Prior evidence is not overwritten.

```sh
python3 scripts/public-conformance.py --fixture-version 3 \
  --source-root /tmp/recalld-implementation/public-similarity-minimal \
  --source-commit 98179a4458b6c3c232df1d997ba31afe29551b31 \
  --cache /tmp/recalld-unit-tests-l__ja1yo/cargo \
  --build-cache /tmp/recalld-public-conformance-crunmxd1/target
```

This minimal checkout does not declare or contain the unrelated CLI34 test `tests/text_migration_cli.rs` or evaluation example `examples/offline_eval.rs`, so neither is silently copied or claimed tested. The allowlisted snapshot and recorded Cargo target inventory describe the actual minimal package. Full combined-runtime coverage in b2 remains historical evidence for its own pinned runtime.

The shared hook is awaited but the legacy recorder is **not receipt-atomic**. Partial storage failures and read/exposure races require the separate transactional-observation stack; no A1 receipt, barrier/snapshot/reset, fixed-clock or full A6/API-v2 claim. Current legacy canonical REST fields must stay unchanged when integrating Hume's separately guarded explicit-policy projection. That follow-up prepares with original requested access preserved, hydrates before exposure, guards explicit true mode, and overlays projected fields only under the explicit policy. Both owners received the agreed helper/hook signatures. No push, merge to main, deployment or personal recalld access.

## Changed-file inventory

The three-parent joins adapt API/query fields, clocks/completion, read admission and inherited lifecycle tests. The transplanted similarity fix changes API state/adapters/handler, MCP adapter, search namespace adapter/pipeline and daemon transport tests. The metadata revision updates the shared late-exposure hook and REST hydration, adds the two regressions, adapts PR39 test signatures/valid vectors in `src/derived_completion.rs` and `src/system.rs`, formats join resolutions, and updates `docs/benchmarks/similarity-transport-fix.md`. `98179a4` changes one inherited HTTP success expectation only. `git diff 6bf71fe..98179a4` shows the fix plus final join adaptations; the two parent merges are explicit in history.

The separate evidence child adds only this new `minimal-parent-metadata/` directory. No existing driver, fixture, report or archive is modified.

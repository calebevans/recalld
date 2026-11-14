# Canonical-label consumer evidence

Runtime: `7223481456564149834429c3dd52c8e1fce7c997`, branch `impl/canonical-label-consumers`, worktree `/tmp/recalld-implementation/canonical-label-consumers`. Author: Caleb Evans <calebdevans@pm.me>. No push or PR publication by this agent.

## Scope and dependency proof

Exact minimal parents are PR58 `faa0b0a`, PR61 `22093c5`, PR62 `ed242ec`, and PR68 `b2923d1`. `ancestry.json` checks these positives and excludes broad writer `30b2010`, broad test merge `be8cfc1`, and catalog50 `d4f756c`. Owner fixture patch `8a4912b` is cherry-picked as `5781635`; the relevant MCP-only test hunk is `42cbec3`. No broad branch source tree was copied.

The runtime commit carries exact optional structured labels/provenance through cache, pipeline, HTTP and MCP results, keeps legacy arrays and absent-carrier payload shape, and treats canonical empty arrays as authoritative. Hard label predicates compile caller strings once, retain original input, and run before vector/FTS/entity/metadata caps and list totals/pagination. Matching uses whole-label Unicode lowercase without aliases, trimming or Unicode normalization. Canonical-value folding remains a measurement item; no new cache/index or ranking change. The actual moka weigher counts added label allocations. Receipt/error flow and supplied-vector/metadata-only provider independence are retained.

Changed runtime files and SHA-256 digests are in `source-files.json`; [implementation scope](../../implementation/canonical-label-consumers.md) describes the consumer boundaries and limitations.

## Actual validation

| Check | Result |
| --- | --- |
| Focused all-feature label tests | 15 passed, 0 failed, 808 filtered out; 19.13s |
| Full all-feature library suite, normal parallel execution | 823 passed, 0 failed; 232.33s |
| CLI integration tests | lifecycle 1 passed; text migration 1 passed |
| Doctests | 4 ignored |
| cargo fmt check | exit 0 |
| cargo clippy, all features/all targets | exit 0; warnings retained: 430 library, 454 test including 430 duplicates, 1 binary |

Image `recalld-ci:1.94`, exact ID in `manifest.json`, Rust1.94.1, Docker network none, 2 CPUs/4GiB/2 Cargo jobs, private cargo/target. No home, credentials or socket mounts. Nothing heavy launched during the A5 HOLD/measurement interval; validation started only after explicit END. No standalone public-episode/Python-runner or remote CI run is claimed for this child.

```sh
docker run --rm --network none --cpus 2 --memory 4g \
  -e RUSTUP_TOOLCHAIN=1.94.1 -e CARGO_BUILD_JOBS=2 \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target \
  -v /tmp/recalld-implementation/canonical-label-consumers:/work \
  -v /tmp/recalld-implementation/compaction-review-7281918/cargo:/cargo \
  -v /tmp/recalld-implementation/compaction-review-7281918/target:/target \
  -w /work recalld-ci:1.94 sh -c \
  'cargo test --offline --locked --all-features && cargo fmt --all -- --check && cargo clippy --offline --locked --all-features --all-targets'
```

## Preserved failures

`join-compile.log`: the initial prerequisite join had seven test-interface mismatches; corrected in `dfde882`. `join-baseline.log`: that source passed 785 library tests and failed 30. Twenty-six mock-width mismatches, three generated-query/unknown-profile fixtures, and one real legacy label-loss fixture are identified in the implementation report. The width/vector fixture patch retains the original eligibility/projection assertions; PR68 fixes legacy admission rather than preserving ephemeral index state.

`focused-initial-compile.log`: the first new-fixture compile lacked the `redb::ReadableTable` trait import. `reproduce-missing-test-import.patch` removes only that import from the frozen runtime and recreates the compiler failure. It is a reproduction artifact, not an intended source change. The corrected focused and full logs are preserved separately; original failures were not overwritten.

## What the UTF-8 probes prove

The offline fixture starts with real legacy insertion/migration, closes the snapshot, then directly authors label payloads in isolated redb records. Reopening OfflineSnapshot validates their structure, not authenticity or native publication. Actual HTTP/MCP routing and hydration exercise those authored records with separate legacy placeholder subsystems. The list test uses schema-bearing codec rows in an already-open legacy-control test database and is a reader unit probe only. Neither setup proves a native store or ordinary serving operation.

Tests preserve exact arrays, duplicate/order/spelling and both authored provenance values, authoritative emptiness, byte immutability during reads, provider call count zero, and the ordinary serving rejection gate. They do not recover missing historical labels, bless unknown profiles, or activate v2. A future Pascal native-writer join must prove real writer → publication → HTTP/MCP read → reopen with exact provenance and malformed profile/vector-proof rejection, retaining native/legacy format negative guards. Full A4 readiness, public envelopes/discovery and the all-writer barrier remain outside this slice.

# Observed integration with canonical entity rebuild

Frozen source `c425a5d4df158e2659ecfa674ea9b422e5e9dc2b`, branch `validation/observed-entity-integration`, joins completed integration report tip `098a182a88d9b9bafd3ba685b41a9d05fd6656ee` and published entity source `7fb27693dca69866eb616ba2ae27e41293d44cca`. Prior measured525 evidence stays immutable.

Four files changed: `src/search/entity_index.rs`, `src/system.rs`, `src/derived_completion.rs`, and owner documentation `docs/implementation/entity-rebuild-labels.md`. The only conflict removes the obsolete per-record legacy-tag insertion in DerivedCompletion after constructing `EntityIndex::from_records`. Startup and completion now share canonical entity selection; authoritative empty labels remain empty, legacy provenance remains retained-only, and Ghost/Tombstone records are excluded. No entire-file override. Observed/control/adapters/pipeline/strength/fault-test files are byte-identical to525, recorded in seam-audit.json.

Actual exact-source validation passed823library tests(normal parallel,189.44s),7CLI integration,2example,7Pythonrunner,fmt,Clippy anddebugbuild. Clippy passed with warnings. Nine public episodes passed(3each stdioMCP/HTTPMCP/REST),54returned-only true cases including18empty no-write, all9 false public/durable-byte comparisons, wrong-quality rejection and restart checks. Separate raw probes passed18postcommit faults,12hydration controls,2finaltransaction guards and1legacy-policy startup guard. Persisted committed receipts and exactlyone returned-ID exposure survive final admission failure. Strict public/hash checker passed.

The three new entity regressions passed: two canonical helper/codec checks and actual online completion replacement. Startup's existing real reopen test now repeats twice. These checks do not claim ordinary schema-v2 serving; that gate stays closed.

Environment: Docker image `sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`, Rust1.94.1, Python3.11.2,aarch64,networknone,2CPU/4GiB,2buildjobs,testlink-j1,privatecopiedCargo/target;nohome/credentials/socketmounts. Source selected by tracked allowlist. Unchanged harness/evidence3642034,fixture3.

Exact runner command from observed-consumer-evidence:

```sh
python3 scripts/public-conformance.py --fixture-version 3 --source-root /tmp/recalld-implementation/observed-entity-integration --source-commit c425a5d4df158e2659ecfa674ea9b422e5e9dc2b --source-input tests/text_migration_cli.rs --source-input examples/offline_eval.rs --cache /tmp/recalld-unit-tests-l__ja1yo/cargo --build-cache /tmp/recalld-public-conformance-wh0alhy7/target
```

Raw output `/tmp/recalld-public-conformance-7lgc6nhu/output`; siblingtargeted contains complete HTTP/MCP/daemon fault responses and exact pinned container command. Archive `validated-c425a5d.tgz` SHA256 `c17faf190d8e39bf5a8368b43eca849d8044f7f9225b8c22014248a78516c856`. Verified182source+164output+4targeted hashes including archive contents. Work-notes destination `Experiments/Artifacts/recalld/2026-11-14-observed-entities-c425a5d` with COPY-MANIFEST.

Local isolated evidence only; no CI,performance,releasegate or fullA6 claim. Public clock is actualwallclock; durable equality excludes only fts.db-shm coordination bytes, retaining raw hashes. No frozen-reset/transitive-barrier/native-public-v2 proof. No push or broad shippingPR.

Next approved shipping work is existing#58 plus actual#57 current129b701 and#40e7,62fixture and the3dee by-ID overlay, with its own full/public/fault validation. Do not absorb this broad branch, unrelated catalog/export/evaluation ancestry, or entity61 solely for convenience. Preserve both preceding evidence freezes.

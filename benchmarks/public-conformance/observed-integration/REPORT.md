# Isolated Observed integration validation

Measured source: `525558a9e0a45d6ef82f5f8d2b09e9bd3de2e44f`, branch `validation/observed-integration`. This is a validation-only child of integration `75882c3d8e2f00a632ad5c76c6261b4cf1a5b023`, not a broad shipping PR. No push, deployment or original-checkout edits.

## Composition and conflict resolution

- Merge published consumer #58 `3b590fb28c84bb2aa2d8afaac66e34556d7da6a6` as `cbb030e`.
- Apply finalized #57 guard `65edaa36a446789e7918e756c624025b1a91b6a7` as `95263af`, retaining both catalog scope validation and permitted restored ancestry.
- Apply Hume's actual migration fixture `62dbd44df2c097681613900e23fccdbd0dbf14b3` as `980f8a5`.
- Fix integration seams in `3deecca70beaa8b63cee836419f77e6c9abeb11d`: diagnostic example receipt interface, duplicate test record field, unused import, and by-ID timestamp fixture adaptation.
- Merge canonical entity fix #40 `e7fd45573b7ba2de4fd216a208f7981a0a8c8c2d` as `525558a`.

Fourteen conflicted files were resolved:

| Files | Preserved behavior |
| --- | --- |
| `src/api/adapters.rs`, `src/api/handlers.rs` | Prepared selection retains requested mode; hydrate canonical metadata/text before returned-only exposure; retain receipts through final admission. Keep main entity scope and namespace error mapping. |
| `src/mcp/bridge_adapters.rs`, `src/mcp/tools.rs`, `docs/mcp.md` | Awaited observations and correct default-mode annotations; retain main namespace/entity/duplicate-coverage changes. |
| `src/search/pipeline.rs` | One strength clock/context, original observation intent, deferred hydration hook, canonical by-ID projection and receipt-carrying final checks. |
| `src/daemon/server.rs`, `src/daemon/read_only_tests.rs` | Main coverage module plus consumer public, false-mode and scoped daemon checks. |
| `src/decay/lifecycle.rs`, `src/decay/sweep.rs` | Main schema-anchor planning plus observation error propagation and shared clock. No native writer activation. |
| `src/derived_completion.rs`, `src/system.rs` | Main terminal/admission guards and stronger scoped recovery assertions; no whole-file replacement. |
| `src/storage/control.rs`, `src/storage/metadata.rs` | Catalog and restored-ancestry checks, lifecycle proof consumers, observation probe, and final schema guard coexist. |

`seam-audit.json` records identical blobs against758 for canonical-schema validation, lifecycle migration, restored lineage, restoration, removal-ledger and activation implementations. Control adds ExposureScope and the65 schema guard/test. Existing generic outcome matching remains intact. Canonical entity selection is exactly e7's schema-authoritative path; existing consumer lifecycle fields remain present.

The actual strength fixture now inserts legacy inputs, performs schema migration, then receipt-backed namespace lifecycle migration. Independent retrieval/compatibility timestamps are tested using an explicitly synthetic CachedRecord read overlay over that valid snapshot. All original by-ID projection, donor, clock-count, scope and byte assertions remain; no persisted native exposure history is fabricated. Hume's separate pure-projection test remains.

## Exact validation

All checks used image `sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef` (`recalld-ci:1.94`), Rust1.94.1, Python3.11.2, aarch64, Docker network none,2CPUs/4GiB,2Cargo build jobs and test linking `-j1`. Test execution used the normal parallel default. Fresh private Cargo registry/target copies; no home, credentials or Docker socket mounts. Source inputs come from the tracked allowlist, including actual Cargo test/example targets.

| Check | Exact525 result |
| --- | --- |
| Full all-feature library tests | 820 passed,0 failed;192.54s |
| CLI integration tests | 7 passed: lifecycle1, profile/attestation/export5, text migration1 |
| Offline evaluation example tests | 2 passed |
| Python runner tests | 7 passed |
| Formatting, Clippy, debug server build | Passed; Clippy warnings remain (460lib,496lib-test including460duplicates,1bin). No deny-warnings claim. |
| Public fixture3 | 9/9 episodes:3 fresh stdio MCP,3 HTTP MCP,3 REST |
| Returned-only true similarity | 54/54 cases, including18 empty no-write cases |
| False reads and malformed quality | All9 public/durable-byte comparisons unchanged; all9 wrong-quality requests reject unchanged |
| Actual postcommit faults | 18 cases:3 transports x3operations x2faults; committed receipts survive and match reopened storage, exactlyone event per exposed ID |
| Actual hydration failures | 12 get/search cases:3transports x2operations xfalse/true; text errors retained, no exposure |
| Final transaction guards | 2 tests passed: schema/policy decode and final namespace/source assertions |
| Corrupt legacy policy receipt | Guard regression passed; writable startup refused without byte changes |
| Strict checker | Passed9episodes,54truecases, output hashes and restart state |

Run from unchanged harness/evidence `364203421a57fd4a93dd20149d89d7ae997ccafb`:

```sh
python3 scripts/public-conformance.py --fixture-version 3   --source-root /tmp/recalld-implementation/observed-integration   --source-commit 525558a9e0a45d6ef82f5f8d2b09e9bd3de2e44f   --source-input tests/text_migration_cli.rs   --source-input examples/offline_eval.rs   --cache /tmp/recalld-unit-tests-l__ja1yo/cargo   --build-cache /tmp/recalld-public-conformance-xk1eh_my/target
```

Final output `/tmp/recalld-public-conformance-wh0alhy7/output`; raw fault responses and exact container command in sibling `targeted/manifest.json` and logs. Targeted commands run `observed_join_tests`, `final_transaction_`, and `policy_receipt_requires_canonical_schema_before_writable_startup` with `--nocapture`, followed by the strict public checker. Each uses the same frozen source and pinned image.

Verified182 source hashes,164 output hashes and4 targeted log hashes, including archive contents. Final archive `validated-525558a.tgz` SHA256 `b6607c478183bf4b6f0d6b8257891f5c4b42d349e74271aed0dece70f3276398`. Work-notes artifact destination: `Experiments/Artifacts/recalld/2026-11-14-observed-integration-525558a`, with COPY-MANIFEST.json.

## Retained attempts and limits

`failed-980f8a5.tgz` preserves the initial example compile failure against the old AccessRecorder signature. `validated-3deecca.tgz` preserves the prior817lib+7CLI+2example+7runner+9public pass before e7. Original consumer3b/evidence364 and all earlier failed conformance evidence remain unchanged.

This is local isolated proof, not CI, a release gate or a performance comparison. Public episodes use actual wall clock and legacy-v1 behavior. Only fts.db-shm coordination bytes are excluded from durable equality; raw hashes remain. This does not establish a transitive barrier, frozen reset, native exposure writer or public-v2 conformance. No new performance matrix or paid models.

## Minimal shipping follow-up

Do not leave62's fixture adaptation solely on this broad validation branch. Proposed existing#58 update: merge actual#57 `65edaa3` into58 `3b590fb`, then updated#40 `e7fd455`; carry62's fixture plus the by-ID overlay from3dee. A read-only text merge preview of58+57 found only2conflict hunks in `src/storage/control.rs` (basefb7ae7e). Preserve the complete outcome validation and permitted restored ancestry. #57 already has its required restore/provenance ancestry; no unrelated catalog, export, evaluation or broad-integration parent is needed.

That minimal composition has not yet been compiled: run its own all-feature tests, fmt/clippy,9public episodes and real fault probes before claiming shipping proof. The example adapter correction is specific to the final integration that contains offline_eval; retain it when composing main rather than importing evaluation ancestry into the consumer branch. No broad shipping PR is proposed.

The next entity-rebuild parent `7fb2769` is deliberately outside this525 source freeze. Its follow-up must preserve Observed wrappers and replace only entity rebuild construction with the shared canonical helper. Actual A6 barrier/reset/clock controls remain subsequent work.

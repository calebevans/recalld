# Completed batch history in offline consumers

The canonical batch schema now has real offline export/import and restore consumers. Completed manifests retain their original parent, child and bookkeeping receipt IDs, origin lineage, ordered outcomes, request/default hashes and immutable namespace IDs. Restore permits historical manifests only through verified bounded ancestry. The bound covers the newest parent, successful child or final-error receipt, not merely the reservation generation. Historical manifests cannot resume in the new retry domain.

Logical transfer carries both typed batch tables losslessly. Restore captures target manifests with the target receipts and merges them into inaccessible staging; identical parent keys require byte-identical manifest rows. It never merges competing terminal outcomes or recreates child content from receipts. Completed purge history remains integrated: old namespace IDs/defaults survive name reuse, and target hard-delete ledger proof prevents resurrection from older archives.

Unresolved items remain supported by the owning online recovery path, but offline export, migration, snapshot, backup inventory and restore reject them before publication. Unknown/future/partial schemas, orphan parents or child receipts, and foreign/unbounded origins fail closed. No active-purge or unfinished-batch recovery protocol is introduced. Ordinary schema-v2 serving remains gated. This is not atomic batch execution, whole-query ReadView or an all-writer barrier.

## Actual parents and proof

- Consumer implementation: `45ac4dc`, based on PR74 `d03116c` and PR72 `e51cb67`.
- Corrected actual PR72 runtime `98dbef6` adds its real PR68 `b2923d1` dependency. Tested composition: `ce66f75f778396c17ad0f78e483a31d3d1494358`.
- Final PR72 documentation/evidence `9d8b295` is merged by `a88631b`; `src`, `tests` and `examples` are byte-identical to the tested composition.
- Six focused regressions pass. Full corrected composition: **760 library tests + 1 CLI test pass**, four doctests ignored. Formatting and all-target clippy exit 0. Clippy reports 509 library and 538 test warnings (507 duplicates), plus one binary warning; this is not warning-free validation.

Tests use actual parser/reservation and reserved insertion/forget writers, including initial resolution errors and receipt-backed final errors. They cover completed purge/name reincarnation, schema migration, exact logical hashes through export/import, ManifestV2 backup, repeated historical forks, retained-target merges without child payload resurrection, old-origin retry rejection, unresolved rejection, malformed evidence, conflicting valid raw row encodings and child/error ancestry bounds. Original PR74 positive restore and unfinished-purge negatives remain present.

This standalone child does not prove PR70. A separate isolated PR70 + this consumer join must turn PR70's old completed-purge negative into the real positive restore case while preserving unfinished negatives. Native strict-inventory integration must consume this recovery implementation, not just allowlist the table names.

## Reproduction and evidence

Docker image `recalld-ci:1.94`, toolchain `1.94.1`, `--network none --cpus 2 --memory 4g`, `CARGO_BUILD_JOBS=2`, dev/test debug info disabled. Only synthetic source/build paths and the readonly offline registry were mounted; no home credentials, daemon or socket mounts.

```sh
docker run --rm --network none --cpus 2 --memory 4g \
  -e RUSTUP_TOOLCHAIN=1.94.1 -e CARGO_BUILD_JOBS=2 \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target \
  -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 \
  -v /tmp/recalld-implementation/batch-offline-compat:/work \
  -v /tmp/recalld-foundation-validation/target:/target \
  -v /tmp/recalld-unit-tests-l__ja1yo/cargo/registry:/cargo/registry:ro \
  -w /work recalld-ci:1.94 sh -c \
  'cargo fmt --all -- --check && cargo test --offline --all-features && cargo clippy --offline --all-features --all-targets && cargo fmt --all -- --check'
```

Before switching this private target from PR74 source to this child, the agent captured the PR74 binary hashes, then ran `cargo clean --offline -p recalld`; the first focused log records that cleanup. All later batch runs used this same source root. No BEFORE/AFTER switchback or other agent's target cleanup was used.

Compact evidence is in `docs/evidence/batch-offline-compatibility/`: source and five actual executable hashes, exact inner command, compressed original focused/full logs. Full log SHA-256: `995a057c18689a3fc36a2ab8bd68dafebb4371a00777196d707da75a681df3a2`. Manifest SHA-256: `c7ea622a142897bb885992394b158f75d65d7a0c72de4005710dc0e5264546a9`.

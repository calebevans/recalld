# Completed purge offline compatibility

A completed namespace purge persists progress and immutable start/child receipts. Previously logical export and restore rejected these unknown tables. Whole-file preservation alone was insufficient: a restore changes the active store ID, and restoring a pre-purge archive could recreate the retired namespace configuration even when the removal ledger removed its records.

This child uses frozen parent `611aa7c` (PR49 capacity, PR53 ManifestV2 and PR48/55 restore prerequisites) plus the published PR67 parent `c3ac21b` (runtime `f6e4d92`). It does not import the broad integration, guard, tracker, native-writer or batch branches.

The strict purge validator permits completed progress from a verified ancestor only within that origin's generation bound. Start and child receipt identities remain in their original store lineage; no receipt or progress ID is rebound. Active progress must belong to the current lineage. Unknown versions, mismatched chains, missing schema/progress, and orphan receipts still fail closed.

Logical export/import explicitly handles the two validated canonical relations, `namespace_purge_schema` and `namespace_purge_progress`. Progress rows remain byte-exact. Migration keeps the same canonical rows. Export still emits only retained record payloads, not unreachable forgotten text-log bytes.

Restore capture now includes optional raw completed-progress rows, distinguishing an absent schema from a present empty schema. Private staging merges source and target rows with conflict checks, alongside the original receipt/key merge. It removes retired namespace configuration and its private copied vector directory, preserves the maximum namespace-ID reservation, and requires a hard-delete ledger entry with all loss bits for each restored record in a retired namespace. Missing proof rejects the restore. Reconciliation and derived rebuilding finish before journal publication; source archives and the original target are not mutated by staging.

The original positive tombstone/purge restore regression remains. Its purge fixture now uses a non-default namespace, because the real purge contract protects `default`. It asserts the retired namespace and vector directory remain absent, not merely that an old slot is free. Additional real-consumer tests cover repeated restore forks; source and target progress/receipts; schema migration; exact logical export/import hashes, retained payload, f32 bits and legacy label provenance; ManifestV2 restore of an imported store; and recovery at Prepared, OldMoved, NewInstalled and Complete publication boundaries.

Active and Cleanup progress reject snapshot, backup inventory, export, migration and actual restore without changing source/archive bytes or publishing a destination/journal. Unknown batch tables remain explicitly unsupported. Completed batch manifests require a separate actual consumer that reconciles their child receipts and resolved namespace IDs. There is no batch atomicity, all-writer barrier, coherent ReadView or native serving activation in this change.

Frozen runtime `3109baa` passed 730 library tests and 1 CLI test; 4 doctests were ignored. All-target clippy exited 0 with 472 library warnings and 502 test warnings including 472 duplicates; formatting passed. The PR67 parent alignment adds documentation only, with no source/test difference from the tested runtime. Focused compatibility checks (7), the original positive purge restore regression (1), and the expanded active/Cleanup actual restore/migration rejection check passed before the full run.

## Reproduction

The package was cleaned with `cargo clean --offline -p recalld` in this agent's private target before changing source roots. Dependencies were retained. The source stayed fixed throughout focused/final reruns, and executable hashes were captured before any later reuse. Tests used synthetic data, no networking, home/credential/socket mounts, personal daemon or external provider.

```sh
docker run --rm --network none --cpus 2 --memory 4g \
  -e RUSTUP_TOOLCHAIN=1.94.1 -e CARGO_BUILD_JOBS=2 \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target \
  -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 \
  -v /tmp/recalld-implementation/purge-offline-compat:/work \
  -v /tmp/recalld-unit-tests-l__ja1yo/cargo/registry:/cargo/registry:ro \
  -v /tmp/recalld-foundation-validation/target:/target -w /work \
  recalld-ci:1.94 sh -c \
  'cargo test --offline --all-features && cargo clippy --offline --all-features --all-targets && cargo fmt --all -- --check'
```

- `/tmp/recalld-lifecycle-review/purge-offline-full.log` SHA256 `f27035bebc5724a0f8f09b32fecec14d442ed7d2c65bbce9a53d7d5ba5b62f76`.
- `/tmp/recalld-lifecycle-review/purge-offline-source-3109baa-binaries.json` SHA256 `3fa6b1bc90b9130861ecd1f5a03ebce3ff3ee8ee08b81b00c108b5aedaf0f936`.
- `/tmp/recalld-lifecycle-review/purge-offline-focused-final.log` SHA256 `67604e796a6f5cd553e7406b3ce263fbe9a1b9bc4b4ed1e4a25b276cd12f9d4a`.
- `/tmp/recalld-lifecycle-review/purge-offline-active-final.log` SHA256 `7e177d1bef1f1495f403809a31c109a0ddc425f7452d1a53d1c8876dcde63054`.

The first compile stopped on a private backup-archive module imported from a storage test module. The test module was moved under the actual restore consumer without widening production visibility. No runtime behavior was changed to bypass validation.

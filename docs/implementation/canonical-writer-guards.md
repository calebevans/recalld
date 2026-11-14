# Canonical writer restrictions

Minimal runtime prerequisites are maintenance d2e83b3, namespace purge f6e4d92, Observed58 faa0b0a and shared legacy admission b2923d1 (PR68). The final write transaction shares namespace_state::require_namespace_active for insertion, tombstone, lifecycle patch and catalog policy, access, exposure and explicit feedback. Catalog creation checks the same durable global purge state; ordinary record/edge effect completion checks it too. Only the existing private purge chunk path bypasses that state to finish deletion. Missing or changed lifecycle policy still reports stale separately from an exact no-op.

The production Rust StorageEngine trait no longer exposes fixture-only raw insertion, tombstone, phase/strength replacement, content stripping, generic batch decay, edge-count overwrite or vector-slot release. Existing unit fixtures retain these under cfg(test), including the raw metadata record-replacement and batch helpers. External callers must use canonical insertion/tombstone/maintenance and graph-effect writers plus awaited derived publication. Backend writable constructors and mutations are crate-internal (primarily storage-internal); public engine accessors no longer expose those implementation handles. This is an intentional Rust API break.

The actual benchmark ingestion caller uses existing DerivedCompletion insertion with its explicit timestamp, configured initial stability and optional supersedes edge. It no longer creates a raw record then independently patches canonical links. The lifecycle CLI fixture now creates its retained record through canonical insertion; the text migration CLI assembles its historical empty files only inside that test. No public benchmark-loader escape or alternate engine is added. The benchmark still has its existing post-creation link flow and must participate in Halley's accepted-work accounting before any barrier guarantee.

Observed58 supplies the actual feedback consumer and receipt implementation; the obsolete raw feedback method is test-only. The shared guard runs in its final transaction. Scoped forget preserves the namespace check under the same storage write lock and retains the typed namespace-mismatch transport error. Native-v2 serving remains gated. Purge offline export/restore compatibility and ancestry-aware progress merging are a separate required child. These restrictions do not claim a complete all-writer barrier or certify effects_epoch as a canonical generation.

Validation: frozen runtime `12a55a019e7dfac8cb5665f6ae75ea63fca5abf0` passed 848 library tests and 2 CLI tests; 4 doctests were ignored. All-target clippy exited 0 (459 library warnings, 519 test warnings including 452 duplicates, plus the existing binary warning), and `cargo fmt --all -- --check` passed. No warning-free claim. The actual LoCoMo module compiled with all features; this branch has no example targets.

LoCoMo has one caller: ingest_conversation constructs entity/topic/emotion tags from the same extracted labels. Tag::new rejects unsupported alphabets before the provider call, but an empty extracted label becomes a valid prefix-only tag and is dropped by retained structured decoding. PR68's shared legacy input validator now checks the original extracted fields in ingest_conversation, before deriving tags or invoking the embedding provider. The LLM extraction necessarily precedes this check. No duplicate validator is added. Explicit created_at, initial_stability bits and supersedes remain in InsertMemory request identity; the existing insertion writer performs final graph validation and produces its canonical receipt.

Producer trace: `ingest_conversation` receives the LLM's original `summary`, `full_text`, `entities`, `topics` and `emotions`, calls `validate_legacy_memory_input`, then constructs normalized tags. `store_single_memory` validates those tags, requests the embedding, and passes `InsertMemory` to `DerivedCompletion::insert`. Its explicit timestamp, stability and optional supersedes are hashed by `InsertMemory::request_hash`; canonical insertion creates the record and structural edge in one metadata/effects transaction and DerivedCompletion awaits publication. LoCoMo no longer calls raw `StorageEngine::insert_memory` or patches the supersedes edge independently. Its existing associative/entity/temporal linking remains separate canonical work, followed by awaited effects completion; this is not one atomic multi-step ingestion or an A6 barrier claim.

The temporary offline boundary is deliberate: a completed namespace purge has durable progress and historical child receipts that the current logical-export and restore inventories do not understand. Those operations reject the tables instead of omitting them. The guard regression `retained_restore_completed_purge_requires_offline_progress_consumer` uses a real non-default namespace, completes its purge, checks the specific unsupported-table error, compares every target file and archive byte, and verifies no restore journal was published. `retained_restore_terminal_ledger_survives_tombstone` retains the positive restore/no-resurrection/UUID-reuse assertions. The dependent offline consumer must restore the positive completed-purge case with ancestry-aware progress/receipt merging and retired namespace reconciliation. This guard slice does not complete that requirement; PR67 remains draft pending paired compatibility proof.

## Reproduction and evidence

Validation ran after the A5 release, one container at a time, without networking, home directories, credentials or socket mounts. The source checkout and private target were the only writable host mounts.

```sh
docker run --rm --network none --cpus 2 --memory 4g \
  -e RUSTUP_TOOLCHAIN=1.94.1 -e CARGO_BUILD_JOBS=2 \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target \
  -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 \
  -v /tmp/recalld-implementation/canonical-writer-guards:/work \
  -v /tmp/recalld-unit-tests-l__ja1yo/cargo/registry:/cargo/registry:ro \
  -v /tmp/recalld-foundation-validation/target:/target -w /work \
  recalld-ci:1.94 sh -c \
  'cargo test --offline --all-features && cargo clippy --offline --all-features --all-targets && cargo fmt --all -- --check'
```

- `/tmp/recalld-lifecycle-review/guard-focused.log` SHA256 `343f690839c7c8934ba90d7bfbf4063e6165c69d3cbc59b28fbf5138f9e0279a`.
- `/tmp/recalld-lifecycle-review/guard-focused-2.log` SHA256 `889da55a235c4fc6f883f1269eaf5754620d9cca222c4fe5756eaa5b54f7a4b0`.
- `/tmp/recalld-lifecycle-review/guard-full.log` SHA256 `9afc22ae31261a1bd805bc0bf207c9157256ece67052a4362f38f8ca48f3c8f9`.
- `/tmp/recalld-lifecycle-review/guard-reconciliation-focused.log` SHA256 `3e49dad588953773422af2c3396451e6051103eb651221eb4bcd9b2795c59a2c`.
- `/tmp/recalld-lifecycle-review/guard-reconciliation-focused-2.log` SHA256 `e01745c1210b9a6a53fae2e9a487d638b3c95b8dc36efe26d39c6b43b9665925`.
- `/tmp/recalld-lifecycle-review/guard-full-final.log` SHA256 `e7fad6c02c411f916b878daa066fa3af787d51aaa1a00e308d3c8d149bc22573`.

The initial full run was 835 passed / 12 failed: a real double-open of the namespace table in the new policy guard, missing canonical namespace setup in old access fixtures, catalog receipts missing from old count expectations, and an old restore fixture attempting to purge the protected default namespace. The policy guard now reads/closes the namespace table before opening its mutable handle. The corrected fixtures retain replay/CAS, abort, overflow, scope and restoration assertions; the explicit completed-purge compatibility negative is additional coverage of the known dependency.

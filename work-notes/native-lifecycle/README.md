# Native lifecycle: frozen source and validation evidence

Parent `129b701` contains only the policy guard evidence on runtime `65edaa3`,
which fast-forwards PR57's lifecycle migration. This implementation adds explicit
native metadata exposure/feedback and its ownership wrapper. Ordinary schema-2
serving, public keyed queries, insertion/tombstone/profile joins, and import
integration are not enabled by this result. See
[implementation contract](../../docs/implementation/native-lifecycle.md).

## Final results

- `focused.log`: 14 native tests passed, including both lifecycle policies,
  real prepared transaction abandonment, replay/reopen, uncertain commit
  reconciliation, same-key concurrency, source-token and policy conflicts,
  malformed proof, history eviction, rollback/copy, and deletion-aware fork.
- `full-suite.log`: 668 library tests and 2 actual CLI tests passed; 4 existing
  doctests ignored. The original attestation-pointer, publication-generation,
  malformed policy-history, restore-journal, and old-reader-fence tests remain.
- `fmt.log`: empty output, exit 0 from `cargo fmt --check`.
- `clippy.log.gz`: complete compressed raw Clippy output, exit 0. It is not
  warning-free: the library-test summary reports 427 warnings, including 409
  duplicates. Warnings include large error types and style/argument-count
  warnings in the new module; no warning-free baseline claim is made.

Every run used synthetic temporary stores in `recalld-ci:1.94`,
`RUSTUP_TOOLCHAIN=1.94.1`, `CARGO_BUILD_JOBS=2`, Docker
`--network none --cpus 2 --memory 4g`. Only the assigned source and its separate
scratch cargo/target were mounted. No home, credentials, socket, personal data,
personal memory tools, or daemon access.

```sh
cargo test --offline --lib storage::metadata::native_lifecycle
cargo test --offline
cargo fmt --check
cargo clippy --offline --all-targets
```

## Reproduced failures and fixes

`mixed-format-before.log` records one failing real-storage regression: generic
legacy metadata open accepted native rows/namespaces with legacy control and no
schema marker. The shared disposable-copy preflight now rejects mixed formats
before writable open. Its final test covers seven cases: native namespaces or
records under legacy control, legacy namespace or record under native control,
unknown namespace version, missing control, and missing selected-text fence.
Raw edits are deliberate corruptions of actual migrated fixtures, never a way
to construct supposedly valid native state.

`contract-before.log` and `capture-before.log` reproduce the independent review
findings with only the corresponding guards removed from an isolated source
copy. Each has one assertion failure, not a compile failure. Typed native
operations now require contract 2 independently of lifecycle policy version;
contract 1 changes neither metadata bytes nor clock count. A deterministic
capture hook signals after initial admission while the publication writer holds
the lock, then the writer poisons the handle. Exposure and feedback capture both
recheck after the lock and return no intent. No timing sleeps are used.

`before-source.tar.gz` preserves the exact inspected/failing Rust modules:
three for the mixed-format run and two for the native-admission reproductions.
`before-source-hashes.json` lists their uncompressed SHA-256 values. These are
module snapshots from the implementation work, not a claim that an earlier
published commit contained the new native API. To repeat the two admission
failures from final source, remove only the explicit native contract-2 check and
the post-lock exposure/feedback availability rechecks, retaining their tests.
For the mixed-format failure, remove the shared `validate_metadata_format` call
from metadata preflight and retain the test's first corruption case.

`initial-full.log` is retained rather than hiding an intermediate failure:
664 passed and one earlier policy negative fixture failed because its helper
attempted canonical inspection after raw-forging a maximum record token. The
new proof correctly rejects that unproven token earlier. The test now compares
the already-known raw file inventory and asserts early rejection and source
immutability, with a separate counter-overflow assertion. It is named
`expected_state_ownership_and_forged_tokens_leave_source_untouched`; no fake
native receipts or weaker proof were introduced to make it pass.

`hashes.json` binds every final changed Rust source file and raw/compressed
artifact. The fault test actually commits redb state before injecting an I/O
error and reconciling after reopen. It models an ambiguous return, not physical
power loss. Receipt success certifies canonical metadata only, not online
search/cache/graph readiness or complete A3 implementation.

# Policy receipt without a schema marker: reproduced and fixed

Fixed by `65edaa36a446789e7918e756c624025b1a91b6a7`, a standalone direct child
of `7bc409957ab0c7d12fc549de0dc5836ac1da5a5f`. Only `src/storage/control.rs`
changed. Main published this exact runtime fix on PR57; no native writer is in it.

The regression creates real temporary legacy metadata, commits a correctly
keyed `LifecyclePolicyMigrated` receipt without any canonical schema marker,
and invokes generic canonical validation and writable metadata startup. Before
the fix, the absent schema marker skipped policy-history validation and generic
receipt decoding accepted the forged operation. The test failed at the expected
rejection assertion. After the fix it passes and verifies rejected writable
startup leaves the metadata bytes unchanged.

The context guard explicitly requires canonical control schema 2 for a policy
migration receipt. Schema 2 still requires its marker and full canonical
consumer. Valid restored historical policy receipts continue through ancestry
and namespace/operation proof validation; this is not a blanket foreign-origin
rejection or wildcard operation acceptance.

Evidence:

- `before.log`: failing new regression against the baseline runtime.
- `after.log`: the same regression passes with the guard.
- `full-suite.log`: 654 library tests and 2 real CLI tests pass; 4 existing
  doctests ignored, including positive schema/restore histories.
- `hashes.json`: SHA-256 of both committed control source files and the raw logs.

The initial failing source was the baseline runtime plus the newly inserted
regression, subsequently formatted and committed with the fix. Its combined
pre-format file was not separately archived; the baseline source hash is not
claimed as the hash of that combined test file. Reproduce from the fixed commit
by removing only the context guard immediately after `validate_receipt` in
`validate_existing`, keeping the new test, then run the targeted command below.
Restore the guard to reproduce the passing result.

```sh
cargo test --offline --lib policy_receipt_requires_canonical_schema_before_writable_startup
cargo test --offline
cargo fmt --check
```

All execution used `recalld-ci:1.94`, `RUSTUP_TOOLCHAIN=1.94.1`,
`CARGO_BUILD_JOBS=2`, Docker `--network none --cpus 2 --memory 4g`, with only the
assigned worktree and separate scratch cargo/target mounts. Tests used synthetic
temporary data; no home, credentials, socket, personal memory or daemon mounts.
The native lifecycle work remains a separate subsequent implementation.

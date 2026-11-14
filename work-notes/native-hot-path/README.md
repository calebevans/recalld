# Native receipt work on the owned handle

Parent: `744ef3c27c6c2b8ffba07ddbf5c0f13eb2b298ce` (PR66).
Worktree: `/tmp/recalld-implementation/native-hot-path`, branch `impl/native-hot-path`.
This child contains no native-record, insertion, tombstone or profile parent join.
The separate `native-records` proof/writer WIP is not part of this evidence.

## Result

The original native capture and mutation guard invoked full receipt/schema proof
on every call. The fix reads the current control singleton and serving lineage
on the already-validated, exclusively owned healthy handle. Final transaction
policy/token checks, keyed replay, and unknown-commit poisoning remain intact.
Full disposable-copy preflight, open/reopen, schema inspection, backup and restore
proofs remain strict. Native preflight now also reuses restore's exact canonical
table allowlist, rejecting unsupported table/multimap state before writable open.

All numbers below count actual validation work, not time. Each bundle is capture,
fresh commit, then matching keyed replay. Checkpoints are the stored receipt
count after the exposure commit and before the following feedback commit.

| Stored receipt checkpoint | Before exposure decodes | Before feedback decodes | After exposure decodes | After feedback decodes |
| --- | ---: | ---: | ---: | ---: |
| 4 | 21 | 27 | 1 | 1 |
| 20 | 117 | 123 | 1 | 1 |
| 69 | 411 | 417 | 1 | 1 |

Before: 3 full proof entries per bundle. After: 0 per bundle. The regression
creates 64 exposure receipts and 3 feedback receipts on an actually migrated
synthetic store; IDs still use the existing nondeterministic allocation sources.
Strict reopen after the loop performs 7 full validations and 1,053 receipt
decodes. Deleting an old receipt then causes rejection without source byte
changes. These redundant startup proof passes are deliberately retained here.
The unknown-table regression fails before the allowlist fix and passes afterward
for both ordinary tables and multimaps, with unchanged source bytes on rejection.

- `before.log`: exact parent runtime plus counters/new tests, scaling FAIL as expected.
- `inventory-before.log`: same source, unsupported table accepted, FAIL as expected.
- `focused.log`: all 16 native tests PASS on the minimal child.
- `diagnostic.json`: machine-readable work counts and checkpoint definition.
- `full.log`: fresh executable, 670 library + 2 actual CLI tests PASS; 4 doctests ignored.
- `fmt.log`: `cargo fmt --check` PASS (empty successful output).
- `clippy.log.gz`: Clippy exit 0; 429 lib-test warnings including 411 duplicates.
  The two new helper signatures inherit the existing large-error-type lint;
  this is not a warning-free build or an unrelated lint cleanup.
- `package-clean.log`, `scaling-after-rebuild.log`, and
  `inventory-after-rebuild.log`: own-package rebuild and both new tests PASS.
- `validation.log`: final chained validation exit 0 (empty successful output).

The first full attempt reused the BEFORE test executable after switching source
mounts at the same `/work` path with one target directory: it reported 668 passes
and both new regressions failing with the original decode counts/behavior. Fixed
source hashes, original test-binary timestamp/hash and the failed full log are
retained in `stale-artifact-diagnostic.json` and
`first-full-stale-test-artifact.log`. `rebuilt-artifact-diagnostic.json` records
the distinct rebuilt executable and unchanged fixed source hashes. The package's own build artifacts were then
removed with `cargo clean -p recalld`; dependency caches were retained. Exact
unchanged fixed source was rebuilt for both focused regressions before the full
suite was rerun. This is not a hidden test retry or a product timing result.

The first exploratory run on the larger native-record WIP reported 1,683 reopen
decodes. It is not the minimal-parent result and is excluded from this comparison.
No latency, end-to-end speedup, bounded ancestry cost, ordinary v2 serving,
whole-store readiness or deterministic A6 ID claim follows from these counters.
B-tree lookup and lineage ancestry validation still have their own costs.

## Reproduction

Only synthetic fixtures, offline Docker, two CPUs, 4 GiB, private cargo/target:

```sh
docker run --rm --network none --cpus 2 --memory 4g \
  -e RUSTUP_TOOLCHAIN=1.94.1 -e CARGO_BUILD_JOBS=2 \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target \
  -v /tmp/recalld-implementation/native-hot-path:/work \
  -v /tmp/recalld-implementation/native-hot-path/.test-scratch/cargo:/cargo \
  -v /tmp/recalld-implementation/native-hot-path/.test-scratch/target:/target \
  -w /work recalld-ci:1.94 \
  cargo test --offline --lib storage::metadata::native_lifecycle::tests -- --nocapture
```

To reproduce BEFORE, start from a disposable checkout of the stated parent and
extract `before-source.tar.gz` over it. The archive includes exact source for
both instrumentation/tests and unchanged runtime boundaries, plus Cargo inputs.
Check `before-source-sha256.txt`. Mount that source at `/work` with a private
cargo/target and run each new test by its full name. Both commands exit 101.
Neither baseline fixture uses a fake valid native record or bypasses migration.
Use distinct target directories for BEFORE/AFTER, or clean this package before
switching source mounts at the same container path. Retained file timestamps can
otherwise leave a stale test executable even when another target recompiles.

The user released A5 at 13:57:21 UTC before these builds. No heavy job was started
by this agent during the hold. No personal store, daemon, home or credential
mounts were used; no image/dependency fetch was needed.

## Remaining integration

The separate native-record child must consume Halley's real ticket-bearing
`MutationGuard` when its minimal accepted-work parent is validated. It will keep
Curie's shared `finish_record_effects` signature, with the future batch-child
argument `None`. No unfinished admission parent or alternate guard is included
in this PR66 correction. Native insertion/tombstone, generated-ID control,
ordinary serving and coherent derived publication remain separate work.

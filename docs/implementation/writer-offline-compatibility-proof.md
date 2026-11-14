# Actual PR70 + purge/batch offline compatibility proof

The isolated `validation/writer-offline-compat` branch combines actual PR70
`af20d73` with PR77 `3b0aded`. PR77 contains PR74 `d03116c` and corrected PR72
`9d8b295` (runtime `98dbef6`, actual PR68 `b2923d1`). No broad integration or
unrelated native/tracker history is imported. Main worktrees were untouched.

Tested runtime: `6ccb48029cb65fd57f81ae6ee7b5f7a21dee054d`.
The joined tree passes **891 library tests + 2 CLI tests**, plus the focused
**13 purge/batch compatibility tests**. Four doctests are ignored. Formatting
and all-target clippy exit zero. Clippy reports 532 library and 596 test
warnings (523 duplicates), plus one binary warning; no warning-free claim.

## Required composition resolutions

- Restore the original completed-purge AND tombstone positive test using PR74's
  actual retained-ledger/progress consumer. Remove PR70's superseded negative
  for completed purge. Keep all actual active/Cleanup purge and unresolved
  batch immutable-rejection tests. The positives now pass on the actual guard
  composition, not only on standalone PR74.
- Preserve crate-private MetadataStore construction and the logical-import
  incomplete-marker rejection together.
- Retain batch reservation/rejection, Observed and LifecycleMaterialized
  outcome types/validation. Retain corrected shared legacy admission, including
  original request hashes, committed replay and pending invalid-item handling.
- Retain the actual batch-backed ordered forget route and scoped single-item
  forget. Keep Observed and committed-mutation error mappings. No weakening of
  namespace checks or raw writer restrictions.
- Join modules/test consumers from both parents. The concrete primitive join
  adds `None` for maintenance's new optional batch-child argument and lists
  batch outcomes in Observed's exhaustive receipt-validation match. The strict
  batch validator remains authoritative; no new batch outcome bypass is added.

Merge resolution commit `40c7ad5` and primitive adaptation `6ccb480` are retained
on this validation branch. Compact evidence includes the readable combined
merge diff and the four-line primitive patch. These are composition details for
main to carry into its intended join, not a request for a broad shipping PR.

## What this proves and what remains

Actual PR67/70/72 runtime plus PR74/77 offline consumers coexist on this exact
joined tree. Completed purge/batch origin IDs, receipts, ledger and retained
payload survive transfer and restore forks; incomplete work rejects without
publication. This does not change the behavior of unmodified standalone PR70,
which intentionally lacks its offline consumer.

No native serving activation, shared publication registry, deadline/barrier,
whole-query ReadView, reset or exact-scan performance claim is made. Halley owns
the shared core registry; Pascal and Curie own native/batch tracker joins.
Snapshot backend and Huygens's intended exact consumer are separate dependent
work. Required offline compatibility is complete here; whole-query consistency
remains an explicit independent requirement.

## Exact validation and retained evidence

```sh
docker run --rm --network none --cpus 2 --memory 4g \
  -e RUSTUP_TOOLCHAIN=1.94.1 -e CARGO_BUILD_JOBS=2 \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target \
  -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 \
  -v /tmp/recalld-implementation/writer-offline-compat-validation:/work \
  -v /tmp/recalld-writer-offline-compat-validation/target:/target \
  -v /tmp/recalld-unit-tests-l__ja1yo/cargo/registry:/cargo/registry:ro \
  -w /work recalld-ci:1.94 sh -c \
  'cargo fmt --all -- --check && cargo test --offline --all-features purge_offline && cargo test --offline --all-features && cargo clippy --offline --all-features --all-targets && cargo fmt --all -- --check'
```

Fresh dedicated target throughout, no source-root switching/cache copy. Only
synthetic stores, no personal data, home credentials, daemon or socket mounts.
The first compilation failed on the two primitive seams above; that original
log is retained beside the corrected successful run. No runtime test failure
was hidden by removing coverage.

Evidence: `docs/evidence/writer-offline-compatibility/`. Manifest contains all
source-file hashes and six actual test/CLI executable hashes. Manifest SHA-256:
`71efc9e0cec1a011032bc3462ef62e81a799a78a6c213698e53ba4484700ea1a`.
Final log SHA-256:
`23bae32f0354f8520d507f8bef3ee218c3c023bb0d96143f98fd19af8769466c`.

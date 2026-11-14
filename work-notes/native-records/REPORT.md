# Native record core validation, 2026-11-14

Runtime commit: `acaa285` on `impl/native-records`. Parent `d94e145` contains
the actual native lifecycle hotfix `b61cb665`, owned tracker `20192bf`, completed
purge offline consumer `d03116c`, insertion/tombstone `dad8465`, and profile
admission `ed242ec`. These are real dependencies; no parallel effect module or
replacement storage plane was copied. Source archive, patch against that parent,
file hashes, executable hash/mtime and raw logs are adjacent.

Validation used `recalld-ci:1.94`, `RUSTUP_TOOLCHAIN=1.94.1`,
`CARGO_BUILD_JOBS=2`, Docker `--network none --cpus 2 --memory 4g`.
Only the assigned native-records worktree and its own `.test-scratch/cargo` and
`.test-scratch/target` were mounted at `/work`, `/cargo`, `/target`.
No home, credentials, socket or personal store was mounted. All Rust commands ran
inside that container with `/work` as working directory.

Commands and results:

```text
cargo test --offline --lib storage::metadata::native_records::tests -- --nocapture
7 passed; 0 failed
cargo test --offline --lib storage::metadata::native_lifecycle::tests -- --nocapture
16 passed; 0 failed
cargo fmt --all -- --check && cargo test --offline
794 library passed; 0 failed
1 lifecycle CLI passed; 1 text migration CLI passed
4 doctests ignored
cargo fmt --all -- --check && cargo clippy --offline --all-targets
exit 0; warnings remain (565 lib, 597 lib-test including 565 duplicates)
```

Seven native tests exercise real insertion/feedback/tombstone/replay/reopen;
saved-default replay in an abandoned transaction and actual policy migration with
zero provider calls; invalid vectors/profile/labels and stale token rejection;
precommit abandonment and durable-but-uncertain commits with reopen recovery;
retained component corruption and terminal UUID reuse rejection; logical transfer
and deletion-aware fork restore; and closed/foreign work-ticket rejection.

The first create/feedback/forget/reopen test caught a concrete proof mismatch:
an edge-only update gave a migrated legacy parent a text commitment absent from
its original history. `native-record-first.log` preserves that failure;
`native-record-first2.log` preserves the focused correction pass. Legacy-inferred
text provenance stays explicitly unknown, while new native rows retain actual
content commitments. This is not a BEFORE/AFTER performance experiment.

The inherited hot-path regression remained bounded in this joined core:
at receipt checkpoints 6, 22 and 71, exposure and feedback each observed
`full_validations=0, receipt_decodes=1`. Strict reopen observed 9 full validations
and 2884 receipt decodes. The changed startup count reflects the real engine
composition; no latency or asymptotic startup improvement is claimed.

The source mount remained this worktree during these final runs. Its own package
artifacts were cleaned before the tracker join; no other agent's target was used
or cleaned. The earlier hotfix mount-switch evidence remains separately archived
under `work-notes/native-hot-path` on the actual hotfix parent.

Scope remains explicit private native entry. Ordinary schema-2 serving, native
batch, maintenance/purge writers, complete ReadView/readiness and deterministic
ID generation remain outside this core. PR69 real transport publication is a
separate child: the existing coordinator must actually rebuild before any native
checkpoint acknowledgment. Curie's frozen batch/tracker API `12e53e0` retains
`MutationGuard` first and optional `BatchChild` last; native passes `None` when
that real dependency is joined. Its tables remain rejected until their actual
offline consumer is present. No public activation or broad integration claim.

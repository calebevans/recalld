# Insertion vector boundary experiment

Production source `bfb4c85608812e206830f851a409cc044b8d1b9f` accepted supplied and
provider-generated zero vectors. Both added no-mutation probes failed; retain the
complete [failure log](before-zero.log) and [reproduction patch](before-regression.patch).
The patch reconstructs the recorded test addition against that source; it is
not an assertion that an unrecorded compiled test binary was archived.

Corrected source `7fa19f8927a9c1c6d46005044b37cae5eebca113` passed **645 library
and 1 CLI tests**, including both probes and real HTTP/MCP adapter validation.
[After-test excerpts](after-tests-summary.log) retain the individual insertion
results and suite totals. [Clippy summary](after-clippy-summary.log) records a
successful, warning-bearing run; formatting also exited 0.

The [manifest](manifest.json) pins source commits, relevant source file hashes,
retained artifacts, original full log hashes, commands, local image and Rust
versions. Full compiler/test logs and caches remain outside the commit under
`.validation/`. Verification commands run offline in the isolated image with
`RUSTUP_TOOLCHAIN=1.94.1`, `CARGO_BUILD_JOBS=2`, `--network none --cpus 2 --memory 4g`.
Only worktree and scratch cargo/target directories are mounted. To reproduce
before, apply the patch to an isolated checkout of the before commit and run the
manifest's focused command. After needs no patch; use the after source commit.

Coverage includes zero/signed-zero, nonfinite and wrong-size vectors with no
payload/receipt/generation changes; nonunit, extreme and subnormal vectors;
raw-bit idempotency; provider error classification without poisoning storage;
and an actual adapter fixture that changes a float bit under double normalization.
These synthetic process-level tests do not establish power-loss behavior,
provider provenance, complete A1, public CAS or schema-v2 readiness.

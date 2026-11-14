# Batch legacy admission evidence

Runtime **98dbef6** joins only batch `e51cb67` and actual shared-admission PR68
`b2923d1`. Exact commits, every tested source hash and executable hashes are in
[manifest.json](manifest.json); runtime patch and raw logs are gzip-compressed
alongside it. Compilation/tests preceded the runtime commit; verified source
bytes are identical. No cache/source-root switching occurred. The private copied
target was package-cleaned before the first build.

Final: **3 new focused + 2 inherited focused; 722 library + 1 CLI passed**.
Zero failures, four doctests ignored. Format passed; Clippy exited 0 with 448
library and 467 test warnings (446 duplicates). Docker used Rust1.94.1, network
none, two CPUs, 4 GiB, two build jobs, private caches and no personal home,
credentials, socket or daemon mounts. Final source was mounted read-only and
hash-checked before and after the checks.

The actual MCP and framed daemon paths each exercise 14 invalid field/label
combinations. They preserve every indexed error and stable replay/conflict,
without provider calls, canonical records, graph/effects, or text/vector changes.
The catalog reservation is intentionally one committed generation, including
all-invalid batches; this is not a whole-store no-write claim. Inherited PR68
single/unreserved early rejection still proves unchanged generation/files.

A synthetic historical manifest checks that old committed receipts replay
without another provider or mutation, while old pending invalid input receives a
separate final-error bookkeeping receipt. A valid normalized-label batch survives
canonical rebuild and reopen. Generic caller hashing remains before stricter
legacy representation validation. No warm-cache substitute, native serving,
whole-store CAS or contract2 activation.

The initial 2-pass/1-fail focused run had an incorrect test assertion looking for
lowercase `conflict`; the actual stable error is `IDEMPOTENCY_CONFLICT`. Its raw
log, source hashes and binary hash are retained as development evidence, not as a
production defect reproduction. Process/reopen checks do not establish power-loss
durability.

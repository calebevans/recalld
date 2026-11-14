# Combined namespace assertions and nonobserving retrieval

A by-ID request can combine its canonical namespace assertion with recordAccess=false without either control being silently ignored by a daemon. Bridge get/similarity signatures put record_access before namespace. Dedicated get_memory_scoped_read_only and find_similar_scoped_read_only aliases require a namespace and force false even if a raw payload says true. Old scoped aliases reject recordAccess; old read-only aliases reject namespace. Remote adapters never fall back. Explicit true scoped calls omit the wire flag so assertion-only servers remain compatible.

Scope checks precede embedding lookup and access work. Reinforcement checks scope again under the existing lifecycle storage write guard and preserves the shared feedback planner; mismatch leaves behavioral caches untouched. Omission retains existing by-ID scope and v1 observation behavior. No public v2, receipts or whole-store atomicity is advertised.

## Actual dependencies

This branch combines read-only PR24 at 5177642 and rebased scope PR27 at 22c5a34. PR27 itself needs validation PR16. PR base is PR24; PR27/PR16 must also be reviewed and included. The merge contains only these parents, with selected scope/read-only integration logic from 8a34a82. It does not inherit the temporary integration branch or require coverage PR19, vector-query support, retrieval semantics2, FTS refill or compaction/schema branches.

HTTP similarity still reaches its existing vector-only EmptyQuery error on this parent. Tests explicitly preserve that failure, while asserting namespace mismatch wins first and false mode does not write or warm caches. The separate vector-query consumer can later change the valid-scope expectation to success. MCP and framed daemon similarity are exercised successfully here.

## Validation

Full offline all-feature suite: **539 tests passed**, four doctests ignored. Formatting and all-target/all-feature Clippy passed with repository warnings. Docker image recalld-ci:1.94, RUSTUP_TOOLCHAIN=1.94.1, network none, two CPUs, 4 GiB, two Cargo build jobs; private review cargo/target caches, no home/credentials/socket mounts. Logs: /tmp/recalld-implementation/scoped-read-only-validation/.scope-{test,clippy}.log. Tests cover both flags through actual HTTP routes, MCP dispatch and framed Unix sockets, plus older method-table simulators that reject combined aliases without fallback. Those simulators are not historical daemon binaries.

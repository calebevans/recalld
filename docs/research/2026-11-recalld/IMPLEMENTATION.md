# Implementation tracking

Implementation starts from upstream `653c8d3`, which includes the tombstone-sweep and daemon-socket fixes merged after research baseline `5aa4692`. Research findings retain their historical baseline.

Independent branches target main. A branch targets another implementation branch only when it consumes that branch’s code. No PR is merged and no personal daemon is replaced by this workflow. Validation uses synthetic stores in isolated offline Docker containers.

| Workstream | Branch / PR | Scope | Status |
| --- | --- | --- | --- |
| Research artifacts | [#12](https://github.com/calebevans/recalld/pull/12) | Evidence and reconciled specs | CI passed |
| MCP structured results | [#13](https://github.com/calebevans/recalld/pull/13) | Modern transport object results, legacy text preserved | 498 tests; CI passed |
| Embedding cache | [#14](https://github.com/calebevans/recalld/pull/14) | Task isolation, complete keys, batch deduplication and vector checks | 500 tests; CI passed |
| Tool contracts | [#16](https://github.com/calebevans/recalld/pull/16) | Validation and truthful existing contracts | 510 tests; CI passed |
| Retrieval | [#17](https://github.com/calebevans/recalld/pull/17) | Supersession, hard filters, namespace caps | 509 tests; CI passed |
| Storage | [#15](https://github.com/calebevans/recalld/pull/15) | Containment, slot/namespace invariants, recovery | macOS/Linux tests passed after canonical-path fix; 515 tests on Linux |
| Lifecycle | [#22](https://github.com/calebevans/recalld/pull/22) | Shared clock, feedback policy and stale-sweep guards | 512 tests after review fixes |
| Mutation foundation | [#21](https://github.com/calebevans/recalld/pull/21) → #15 | Metadata generation and transactional access receipts | 530 tests after parent integration; no public whole-store CAS |
| Read-only recall | [#24](https://github.com/calebevans/recalld/pull/24) → #22 | Explicit no-write retrieval through transports | 518 tests |
| Evaluation | [#18](https://github.com/calebevans/recalld/pull/18) | Offline reproducible measurement foundation | 496 library + 2 harness + 3 runner tests; three-process replay |
| Scan coverage/session | [#19](https://github.com/calebevans/recalld/pull/19) → #16 | Observed bounded scan counts and v1 session context | 513 tests |
| Entity-link scope | [#20](https://github.com/calebevans/recalld/pull/20) | Canonical namespace checks before link caps | 499 tests after accepted-name review fix |
| Paired retrieval study | [#23](https://github.com/calebevans/recalld/pull/23) → #18 | Five seeds, 1k/10k, unchanged internal harness | 20 processes / 8,000 measured queries; evidence archived |
| Retrieval semantics | [#25](https://github.com/calebevans/recalld/pull/25) → #17 | Internal opt-in current/history, bounded graph work and diagnostics | 526 tests; no public v2 selector |
| Mutation admission | [#26](https://github.com/calebevans/recalld/pull/26) → #21 | Sticky unavailable handle after uncertain publication | 531 tests; entry checks, not whole-pipeline leases |
| Namespace assertions | [#27](https://github.com/calebevans/recalld/pull/27) → #19 | Canonical opt-in by-ID scope across transports | 518 tests |
| Precomputed vectors | [#28](https://github.com/calebevans/recalld/pull/28) | HTTP vector search/similarity, validation and normalization | 499 tests |
| Eligible FTS refill | [#29](https://github.com/calebevans/recalld/pull/29) → #25 | Stable bounded refill and internal coverage/error diagnostics | 533 tests |
| Text generations | impl/text-compaction → #26 | Metadata-selected durable text generations | Held for independent review corrections |
| Namespace read batching | impl/namespace-read-batch → #17 | One metadata snapshot per eligibility load | 509 tests; paired measurement underway |

## Remaining contract gates

These first PRs do not complete the specs. Durable effects/outbox and all canonical writers must participate before whole-store generations, readiness or public CAS are exposed. Lifecycle v2 migration, durable feedback, removal ledger and restore lineage, profile attestation, exact scan cursors, v2 envelopes/batches and public conformance remain dependent work. Optional backend/index/policy promotion remains gated by actual evidence.

Test counts apply to each branch separately, not a combined release. Integrated validation follows prerequisite integration; tests alone do not establish crash safety or performance improvement.

## Integration evidence

A temporary integration branch combined structured results, embedding cache, tool validation, retrieval, storage, lifecycle and offline harness. Formatting passed, 556 library tests and two harness tests passed, with four existing doctests ignored. This run preceded the storage path-alias test and lifecycle review fixes and does not certify the later dependent slices.

Independent lifecycle review reproduced a stale sweep overwriting newer feedback/permastore state, graph phase lag and a backward HTTP access timestamp. The lifecycle PR now includes fixes and dedicated regressions. Storage macOS CI exposed an expected-path assertion mismatch after parent canonicalization; the fix also tests that symlinked parent aliases share ownership. These findings remain recorded despite earlier green local suites.

The next integration checkpoint passed 577 library, two Rust harness and three Python runner tests after including mutation foundations, coverage and the first entity-scope change. A later checkpoint includes the lifecycle review fixes, accepted-name compatibility correction, namespace-read batching and read-only transport; it passed 608 library and two Rust harness tests after also incorporating the internal retrieval-semantics branch. Four existing doctests were ignored. The next checkpoint adds precomputed vector queries and updates the read-only HTTP similarity regression to require successful, no-write retrieval.

The paired study measured corrected retrieval at approximately 2.85x baseline p95 at 10k total records (1k queried-namespace records). Canonical eligibility dominated that overhead. This is characterization on a shared host, not a release-gate result. A separate transactional batching optimization retains the correctness rules and is being measured against the corrected baseline.

Entity-link review found that legacy tag parsing drops accepted multiword/Unicode names. The scope PR therefore checks canonical namespace/deletion state without treating incomplete stored tags as the full entity set. Real store-call regressions preserve accepted-name linking and prevent alias-count arithmetic underflow; durable structured-label persistence remains a schema/migration requirement.

The combined vector checkpoint passed 611 library and two Rust harness tests, including successful HTTP similarity with `recordAccess=false` and no access writes. Namespace-assertion integration is being checked separately against the combined read-only daemon protocol.

Independent compaction review found an old-writer window between selecting a copied legacy text generation and installing the rejection gate. It also demonstrated that a failed open with a missing selected payload could modify metadata before validation. Compaction publication is held for fixes and regressions; new-reader interruption tests alone did not establish safe upgrade behavior.

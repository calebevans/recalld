# Implementation tracking

Implementation starts from upstream `653c8d3`, which includes the tombstone-sweep and daemon-socket fixes merged after research baseline `5aa4692`. Research findings retain their historical baseline.

Independent branches target main. A branch targets another implementation branch only when it consumes that branch’s code. No PR is merged and no personal daemon is replaced by this workflow. Validation uses synthetic stores in isolated offline Docker containers.

| Workstream | Branch / PR | Scope | Status |
| --- | --- | --- | --- |
| Research artifacts | [#12](https://github.com/calebevans/recalld/pull/12) | Evidence and reconciled specs | CI passed |
| MCP structured results | [#13](https://github.com/calebevans/recalld/pull/13) | Modern transport object results, legacy text preserved | 498 tests; CI passed |
| Embedding cache | [#14](https://github.com/calebevans/recalld/pull/14) | Task isolation, complete keys, batch deduplication and vector checks | 500 tests; CI passed |
| Tool contracts | [#16](https://github.com/calebevans/recalld/pull/16) | Validation and truthful existing contracts | 510 tests; CI running |
| Retrieval | [#17](https://github.com/calebevans/recalld/pull/17) | Supersession, hard filters, namespace caps | 509 tests; CI running |
| Storage | [#15](https://github.com/calebevans/recalld/pull/15) | Containment, slot/namespace invariants, recovery | 514 initial tests; macOS path assertion corrected with alias regression; CI rerunning |
| Lifecycle | impl/lifecycle | Shared clock and reinforcement correctness | 505 initial tests; review reproduced stale-sweep/feedback race, fixes underway |
| Mutation foundation | impl/mutation-foundation → impl/storage | Metadata generation and transactional access receipts | 506 tests before parent integration; no public whole-store CAS |
| Read-only recall | impl/read-only-recall → impl/lifecycle | Explicit no-write retrieval through transports | Implementing |
| Evaluation | impl/evaluation | Offline reproducible measurement foundation | Implementing |

## Remaining contract gates

These first PRs do not complete the specs. Durable effects/outbox and all canonical writers must participate before whole-store generations, readiness or public CAS are exposed. Lifecycle v2 migration, durable feedback, removal ledger and restore lineage, profile attestation, exact scan cursors, v2 envelopes/batches and public conformance remain dependent work. Optional backend/index/policy promotion remains gated by actual evidence.

Test counts apply to each branch separately, not a combined release. Integrated validation follows prerequisite integration; tests alone do not establish crash safety or performance improvement.

## Integration evidence

A temporary integration branch combined structured results, embedding cache, tool validation, retrieval, storage, lifecycle and offline harness. Formatting passed, 556 library tests and two harness tests passed, with four existing doctests ignored. This run preceded the storage path-alias test and lifecycle review fixes and does not certify the later dependent slices.

Independent lifecycle review reproduced a stale sweep overwriting newer feedback/permastore state, graph phase lag and a backward HTTP access timestamp. The lifecycle PR is held for fixes and dedicated regressions. Storage macOS CI exposed an expected-path assertion mismatch after parent canonicalization; the fix also tests that symlinked parent aliases share ownership. These findings remain recorded despite earlier green local suites.

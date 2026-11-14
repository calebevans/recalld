# Implementation tracking

Implementation starts from upstream `653c8d3`, which includes the tombstone-sweep and daemon-socket fixes merged after research baseline `5aa4692`. Research findings retain their historical baseline.

Independent branches target main. A branch targets another implementation branch only when it consumes that branch’s code. No PR is merged and no personal daemon is replaced by this workflow. Validation uses synthetic stores in isolated offline Docker containers.

| Workstream | Branch / PR | Scope | Status |
| --- | --- | --- | --- |
| Research artifacts | [#12](https://github.com/calebevans/recalld/pull/12) | Evidence and reconciled specs | CI passed |
| MCP structured results | [#13](https://github.com/calebevans/recalld/pull/13) | Modern transport object results, legacy text preserved | 498 tests; CI passed |
| Embedding cache | [#14](https://github.com/calebevans/recalld/pull/14) | Task isolation, complete keys, batch deduplication and vector checks | 500 tests; CI running |
| Tool contracts | impl/tools | Validation and truthful existing contracts | Reviewing omitted-argument compatibility |
| Retrieval | impl/retrieval | Supersession, hard filters, namespace caps | Reviewing namespace enumeration cost |
| Storage | impl/storage | Containment, slot/namespace invariants, recovery | Implementing |
| Lifecycle | impl/lifecycle | Shared clock and reinforcement correctness | 505 tests; independent review |
| Mutation foundation | impl/mutation-foundation → impl/storage | Metadata generation and transactional access receipts | 506 tests before parent integration; no public whole-store CAS |
| Read-only recall | impl/read-only-recall → impl/lifecycle | Explicit no-write retrieval through transports | Implementing |
| Evaluation | impl/evaluation | Offline reproducible measurement foundation | Implementing |

## Remaining contract gates

These first PRs do not complete the specs. Durable effects/outbox and all canonical writers must participate before whole-store generations, readiness or public CAS are exposed. Lifecycle v2 migration, durable feedback, removal ledger and restore lineage, profile attestation, exact scan cursors, v2 envelopes/batches and public conformance remain dependent work. Optional backend/index/policy promotion remains gated by actual evidence.

Test counts apply to each branch separately, not a combined release. Integrated validation follows prerequisite integration; tests alone do not establish crash safety or performance improvement.

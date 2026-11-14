# Implementation tracking

Implementation starts from upstream `653c8d3`, which includes the tombstone-sweep and daemon-socket fixes merged after research baseline `5aa4692`. Research findings retain their historical baseline.

Independent branches are used where changes do not require an unmerged predecessor. Dependent foundations and consumers will form explicit stacks. No PR is merged and no personal daemon is replaced by this workflow.

| Workstream | Initial branch | Scope | Status |
| --- | --- | --- | --- |
| Research artifacts | impl/research | Evidence and reconciled specs | Publishing |
| Tool contracts | impl/tools | Validation and truthful existing contracts | Implementing |
| Retrieval | impl/retrieval | Supersession, hard filters, namespace caps | Implementing |
| Storage | impl/storage | Containment, slot/namespace invariants, recovery | Implementing |
| Lifecycle | impl/lifecycle | Shared clock and reinforcement correctness | Implementing |
| Evaluation | impl/evaluation | Offline reproducible measurement foundation | Implementing |

Later slices from INTEGRATION-REVIEW remain required work, not implicitly completed by these first PRs. Optional backend/index/policy promotion remains gated by actual evidence.

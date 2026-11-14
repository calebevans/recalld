# Recalld research package

Start with the [findings and recommendations](REPORT.md), then read the [shared implementation contracts](specs/CONTRACTS.md) and [integration review](specs/INTEGRATION-REVIEW.md) before assigning implementation work.

## Implementation specifications

| Action | Specification |
| --- | --- |
| A1 | [Storage correctness, recovery and backend decision](specs/A1-storage-recovery.md) |
| A2 | [Retrieval correctness and ranking](specs/A2-retrieval.md) |
| A3 | [Memory lifecycle and explicit feedback](specs/A3-lifecycle.md) |
| A4 | [Tool contracts and namespace workflows](specs/A4-tools-namespaces.md) |
| A5 | [Performance measurement and optimization](specs/A5-performance.md) |
| A6 | [Evaluation and learning harness](specs/A6-evaluation-harness.md) |

## Evidence and reproducibility

- [Preparation and research plan](PLAN.md)
- [Storage source review](evidence/exploration-storage.md)
- [Retrieval source review](evidence/exploration-retrieval.md)
- [MCP/API source review](evidence/exploration-tools.md)
- [Storage alternatives and primary sources](evidence/storage-options.md)
- [Memory evaluation and RL primary-source research](evidence/external-memory-research.md)
- [Actual Rust test baseline and regression probes](experiments/baseline/REPORT.md)
- [Synthetic ablations, transaction PoC and historical benchmark audit](experiments/ablations/REPORT.md)

Experiment directories contain raw logs/results and runnable commands. Synthetic models are explicitly separate from engine tests. All proposals remain unimplemented. Personal backups are outside this repo; lab notes are also recorded in `~/work-notes/work/Experiments/Recalld correctness and retrieval research.md`.

# Recalld improvement research

Started 2026-11-13 America/Los_Angeles (2026-11-14 UTC). Baseline commit: `5aa4692c7d326d2ef10712939eb01484a520e891`. Scope is research, reproducible probes, recommendations and implementation specifications. Production code is unchanged.

## Preparation

- Personal GitHub identity switched and verified as `calebevans`.
- Three Sol explorers independently inspect storage, retrieval/decay, and tool/namespace contracts.
- Personal daemon stopped for native backup plus complete data-directory/config archive, then restarted successfully. Private archives are outside git at `/Users/caleb.evans/recalld-backups/20261114T051705Z/`. Native backup contains 7 files; complete archive was decompressed/read and SHA-256 hashes recorded. This verifies archive integrity, not a full application restore drill. No personal data is included in research artifacts.
- Work-notes experiment instructions read from `~/work-notes/work/.agents/skills/experiment/SKILL.md`; records will mirror its template and include exact commands, samples, limitations and raw evidence.

## Research sequence

1. Map implementation and classify findings as demonstrated, source-supported risk, or hypothesis.
2. Run existing tests and focused correctness probes in copied source inside isolated Docker containers. Fetch build dependencies separately; execute tests without networking. No home directories, personal stores, credentials or host daemon sockets mounted.
3. Use synthetic data for retrieval/performance ablations. Separate algorithm microbenchmarks from actual application measurements; never infer end-to-end speedups from surrogate timing.
4. Research primary sources for transaction/recovery design, memory evaluations and learned memory policies. Audit existing benchmark methodology before trusting its scores.
5. Prioritize changes by correctness and measurable utility; compare incremental repair with a simpler storage redesign. RL remains gated by deterministic, isolated episodes and reliable rewards.
6. Produce an evidence-linked report and one delegated technical specification per action item. Have one independent integrator review all specifications together and resolve API, ownership, ordering and migration gaps.

## Evidence rules

Each experiment records revision, data provenance, toolchain, environment, exact commands, baseline, variants, metrics, sample count and limitations. No paid model training is presumed. A passing existing suite does not establish crash safety or answer quality. Synthetic fixtures establish narrow properties only. Untested work is explicitly labeled. Outputs and reproducible scripts stay in this repository; lab records also go in work-notes.

## Execution record

- Sol exploration completed in three disjoint tracks; reports persisted under `evidence/`.
- Primary-source memory/evaluation research and storage comparison completed.
- Isolated Python model ablations executed twice with identical results. Thirty SQLite process-exit cases and six saved benchmark-result reconciliations completed.
- Isolated actual Rust suite: 487 passed, four doctests ignored. Seven additional regression tests: three passed, four failed, confirming malformed quality/dimension handling and supersession ranking defects.
- Actual release `FlatVectorIndex` kernel measured at 1k/10k vectors, dimension 384, unfiltered/10%-namespace-filtered, 100 timed queries each. No end-to-end speedup claim.
- One initial test-agent branch was blocked by a content filter and closed; permitted ordinary testing completed through a fresh branch.
- Work-notes experiment and daily entry written with raw-artifact links and measured results.
- Six dedicated specification agents completed A1–A6; one independent reviewer reconciled shared contracts and wrote the dependency-ordered integration review. Final local links and saved JSON/hash integrity checks passed. Work-notes experiment marked done; production code remains unchanged.

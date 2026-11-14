# Frozen combined public runtime

Runtime `aeab9e68e749e4e178576c2283a6173c11611a31`, dedicated detached checkout
`/tmp/recalld-implementation/public-runtime-aeab9e6`. No runtime edits.

## Unchanged fixture v1 completed before extension

Nine episodes completed, three per stdio MCP, HTTP MCP and REST, each restarted.
All nine wrong-type quality requests rejected; before/after public records and
all recorded file hashes matched exactly. Summary `conformancePassed: true` is
scoped to this diagnostic, not full A6/API-v2 conformance. False-read, namespace,
feedback, summary/full-text deletion and restart assertions passed in every run.
The fixture hash remains `7a347fd0f97ce94ead4fa9f978c39de6060f804e0d5a0eb3afbeaa06812e9d00`.
The old runtime03d9b4d conformance-false archive is retained unchanged.

Actual validation: 672 library tests + 1 migration CLI integration test + 2
example tests + 7 Python runner tests passed. Fmt, all-target/all-feature Clippy
(with existing warnings) and production debug binary build passed. Both
`tests/text_migration_cli.rs` and `examples/offline_eval.rs` are explicit runtime
inputs; Cargo metadata records all six targets. Runtime source hashes match the
frozen commit. No doctests were invoked or claimed passing.

All tests/episodes used Rust1.94.1, image
`sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`,
network-none Docker,2CPUs/4GiB, copied Cargo/target, no personal mounts. First
attempt's linker was killed with signal9 before any public episode; retained
as [linker failure](linker-failure.tgz). Test-target linking was then serialized
with `-j 1`, retaining the2-job environment ceiling for other build commands.
No memory increase or runtime change. [Completed fixture-v1 evidence](fixture-v1-evidence.tgz)
contains all raw attempts, outcomes, hashes, logs and exact harness inputs.
Successful scratch `/tmp/recalld-public-conformance-0fxbl782/output`.

The separately declared fixture revision2 below adds actual supported embedding-only
REST and by-ID similarity probes. Fixture version2 is not API contract version2.
Removal ledger, online effects completion, hard-delete recovery, compaction
crash safety, canonical barriers/snapshots and manual-clock replay remain outside
these public episode assertions.

## Fixture revision2: completed read-only extension, wire gaps retained

Nine further episodes completed on the same frozen runtime, each restarted.
Fixture revision2 preserves the original four inputs and adds copper, vector
[0.8,0.6,0,0], in default scope. It remains legacy public API v1, not API v2.

| Extension | Before mutation | After restart | Total |
| --- | ---: | ---: | ---: |
| By-ID similarity, all three transports | 27 | 9 | 36 |
| Embedding-only REST search | 9 | 3 | 12 |
| Foreign namespace assertion rejections | 9 | 9 | 18 |
| REST wrong-dimension / zero-norm rejections | 6 | 6 | 12 |

All added read windows preserved exact public fields and durable hashes (only
SQLite fts.db-shm excluded with raw hashes retained), and invoked the embedding
provider zero times. Expected near neighbor copper was first, source excluded
from similarity, foreign distractor excluded, and deleted birch stayed absent
after restart. All nine wrong-type quality probes again rejected with identical
public fields and all captured file hashes. Original scoped assertions passed.
Full validation again passed672library +1CLI +2example +7Python tests, fmt,
Clippy and debug binary build. Clippy retained324library warnings,332library-test
warnings including duplicates, and1binary warning. No doctest claim.

There are **two real wire-parity gaps**. The existing machine summary reports
`conformancePassed: true` for its executed scoped assertions; it must not be
interpreted as complete response parity. Independent raw-trace inspection emits
`wireParityPassed: false` in [wire-parity-findings.json](wire-parity-findings.json):

- MCP similarity returned `namespace: ""` in24responses across6episodes. Each
  returned ID was canonically in default scope, so no foreign record leaked,
  but the public namespace representation is wrong.
- REST similarity requested `minScore: 0.1` and returned zero-score memories
  in12responses across3episodes (21below-threshold hits). MCP returned only the
  0.8neighbor. This is a threshold bug, not an acceptable parity definition.
  `FindSimilarRequest.min_score` documents a minimum similarity score, and
  docs/mcp.md and the MCP tool schema explicitly apply minScore to single-mode
  similarity. It must filter returned similarity scores; memory strength is a
  different quantity. No contract is redefined to excuse the result.

Example exact REST response in the first initial window: copper score0.8,
birch score0.0, olive score0.0 for minScore0.1. The corresponding MCP response
contains copper score0.800000011920929 with empty namespace. Every finding in
JSON names its episode, exact trace step, actual UUIDs and offending values;
the archive retains the full request/response. No raw outcome was rewritten.
A separate child fix branch will address these two surfaces, using this frozen
runtime as its measured parent, with strict real public regression checks.

Fixture2 generated45accepted records and951wire events (315stdio,315HTTPMCP,
321REST).135provider requests comprise126fixture hits and9deliberate misses;
none occurred in the vector/similarity windows. Source retention and Recall@20
remain1.0 on the single synthetic evidence source, with no semantic claim.
Mapped text ranks were [turn-001,turn-005,turn-002,turn-004], then
[turn-001,turn-005,turn-004] after deletion/restart. Similarity result membership
beyond the expected neighbor is deliberately reported, not silently normalized.

Final fixture2 scratch: `/tmp/recalld-public-conformance-zoq8mlcs/output`.
Fixture SHA-256 `77369cde7c02ff0e31afd9bfb8dd072c7891daef147321353e4376fac40ecd86`.
Driver SHA-256 `c73466aef2182676b4f2c81d0133dc565544587d1fd1312fe63fa499592620cb`.
All146source and131output hashes were independently verified. Every per-window
state/provider comparison and all nine strict-quality results were rechecked.

Reproduction (omit --fixture-version2 for the unchanged fixture1):

```sh
python3 scripts/public-conformance.py --fixture-version 2 \
  --source-root /tmp/recalld-implementation/public-runtime-aeab9e6 \
  --source-commit aeab9e68e749e4e178576c2283a6173c11611a31 \
  --source-input tests/text_migration_cli.rs \
  --source-input examples/offline_eval.rs \
  --cache /tmp/recalld-unit-tests-l__ja1yo/cargo \
  --build-cache /tmp/recalld-public-conformance-0fxbl782/target
```

## Immutable artifacts and boundaries

- [fixture-v1-evidence.tgz](fixture-v1-evidence.tgz): 203,828bytes, SHA-256 `54165db6bb2e2a2312a27953e56b91e5a33c9770b88d4d40814492dca861fc84`.
- [fixture-v2-evidence.tgz](fixture-v2-evidence.tgz): 238,053bytes, SHA-256 `421b0e29b14055e033f3b88a3ca75cb0b05607242c92a615301337016c8d2f0e`.
- [linker-failure.tgz](linker-failure.tgz): 29,951bytes, SHA-256 `cf50acc6f397b3e12e16d6ec3242c7e09e8e791e1d9312b7c88dbcd9a3413a6f`.
- [wire-parity-findings.json](wire-parity-findings.json): 9,858bytes, SHA-256 `3dfdacaf1897812d7c133224e462e2b4e6adebbd4ee01de450305bcf670c07c2`.

Both complete archives include exact harness/fixture inputs, manifests, wire
attempts/responses, mapped identities, all state evidence, build/test logs and
source/output hashes. No caches, binaries or database payloads are committed.
Original03d9b4d final and failed-conformance archives remain unchanged.

No removal-ledger, future online completion, hard-delete, compaction crash-safety,
manual-clock, canonical snapshot/barrier, v2 retrieval, performance-gate or full
A6 claim follows. CLI fresh-destination behavior passed its explicit integration
test; ordinary public restart does not establish production restore guarantees.

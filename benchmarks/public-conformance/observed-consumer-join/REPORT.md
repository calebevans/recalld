# Minimal consumer join: committed observations survive admission failure

Measured runtime source: **1d52153db6520d9b64e568f281557441b11f0ff3**.
Runtime branch handoff tip: **3b590fb28c84bb2aa2d8afaac66e34556d7da6a6** (documentation only after measured source).
Runner: **d6daac2ff1932fc4c9caacc578d3d71eb27d0f04**. Both worktrees were clean at snapshot.
This is local isolated validation, not CI or a release/performance gate.

## Parents and behavior

Actual parents include #47 `9ebe082` and published by-ID #54 `74c9bfb`, including
core #45 `fb7ae7e`, schema #40 `6c0aa74`, and minimal ownership #15 `2788645`.
The by-ID parent already includes #51 similarity, #46 strength and #30 namespace
batching. No full integration, frozen aeab runtime, catalog48/50, restore or
logical-export branch was imported. [ancestry.json](ancestry.json) records exact
positive and negative ancestor checks.

Original requested access mode survives preparation. The final hydrated hit set
(and eligible MCP neighbors actually exposed) is observed once. Similarity source,
below-threshold and over-limit candidates are not observed. Private Observed<T>
carries actual WriteReceipt values through the outermost effects-stamp check.
Postcommit failure retains committed/not-ready receipts and no result payload;
unknown outcomes and partial namespace receipts retain their classification.

The final metadata transaction decodes namespace schemas and checks policy,
prepared namespace, by-ID source and terminal UUID evidence. The legacy writer
rejects explicit policy and all other schema-bearing records/namespaces until a
native writer exists. It cannot corrupt native lifecycle anchors. Keyed historical
replay stays ahead of fresh policy/clock checks. Existing REST canonical metadata
and fallible text errors are preserved. #45 pending-effect preflight remains intact.
Main must preserve later permitted-origin/catalog-scope validation when joining
48/50; this branch does not replace absent catalog/restore semantics.

## Exact validation

Image `sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`
(`recalld-ci:1.94`), Rust1.94.1, Python3.11.2. Docker runtime network none,
2 CPUs, 4GiB memory/swap cap, 2 Cargo build jobs; Cargo test build/link `-j1`.
The full Rust test runner used its normal parallel default, with no
RUST_TEST_THREADS override. Private copied Cargo registry/target, temporary HOME,
read-only container root, no personal/credential/socket mounts. Exact commands and
resource readings are archived. The optional `--test-threads` runner parameter
is recorded in manifests; it was unset for both completed runs.

| Check on exact1d52153 | Result |
|---|---|
| Full library suite | 742 passed, 0 failed, 0 ignored; 64.19s test runtime |
| Actual text migration CLI integration | 1 passed |
| Three binary test targets | Built and run; 0 embedded tests each |
| Runner source-boundary checks | 7 passed |
| cargo fmt --check | Passed |
| cargo clippy --all-targets --all-features | Passed with warnings:374lib,391lib-test (374duplicates),1binary |
| Debug public server build | Passed |
| Process public replay | 9/9: stdio MCP, HTTP MCP, REST; 3 fresh episodes each |
| Strict independent checker | Passed; source/output hashes and restart state preserved |
| Postcommit fault probes | 18/18:2 faults x3 transports x3 operations |
| Text hydration error controls | 12/12:3 transports x get/search x false/true |
| Final transaction guards | 2 tests passed (schema/policy and source/scope) |

No doctest execution or absent offline_eval example is claimed. Cargo metadata
lists one library, three binaries and `tests/text_migration_cli.rs`; that test file
is explicitly included in the bounded source snapshot.

Full run:

```sh
python3 scripts/public-conformance.py --fixture-version 3 \
  --source-root /tmp/recalld-implementation/observed-consumer-join \
  --source-commit 1d52153db6520d9b64e568f281557441b11f0ff3 \
  --source-input tests/text_migration_cli.rs \
  --cache /tmp/recalld-unit-tests-l__ja1yo/cargo \
  --build-cache /tmp/recalld-public-conformance-5bjktsl_/target
```

Final output: `/tmp/recalld-public-conformance-3j81tywy/output`.
Additional exact-source raw fault responses: sibling `targeted/transport-faults.log`;
transaction checks: `targeted/final-transaction.log`; exact command/hash manifest:
`targeted/manifest.json`. These additional logs do not modify the finalized main
output manifest. Tests run against a read-only source mount and the pinned image.

Fault fixtures use temporary Rust-test storage; operations traverse production
HTTP routers, MCP tool dispatch, and real Unix-socket framed daemon RPC. Test-only
instance-local injection runs **after real exposure commit/cache publication**,
then either closes admission or changes its epoch. Every error's operation ID and
generation matches persisted receipt lookup after reopen; history has exactly one
event per exposed ID, and no selected payload escapes. False/empty controls bypass
writes and the fault hook. This is distinct from the nine ordinary server-process
public replays, which use public store calls and contain no test-only fault switch.

## Public evidence and frozen-input checks

Fixture3 SHA256: `74845b1e71ac6afd191e272d00cc43b3a0b331fef287a839749f2a68fd1384ee`.
Driver SHA256: `403ab67ee2fccf0e38d84dfa2a4aa8ffd24d7aa5cb7af30981dd00148842b14d`.
All **163 source hashes and 164 output hashes** were independently verified,
including against the archive contents.

All nine episodes report false-read public and durable-file state unchanged,
including vector/similarity windows and restart. The comparison excludes only
`fts.db-shm` SQLite coordination bytes; its raw hash remains recorded. This is
bounded observed byte evidence, **not a transitive drain or canonical snapshot**.
All9 malformed-quality probes rejected without change; all54 true similarity
cases passed, including18 empty-result no-write cases. Source/filtered IDs were
excluded; namespaces and minScore were preserved. Search minStrength remains a
separate hard filter. Each episode restarts after deletion and after exposures.

The synthetic provider received144 requests:135 fixed-input hits and9 intentional
fail-closed misses. No external model/provider calls. Extraction/source retention
and retrieval diagnostics remain separate; artificial vectors establish no
semantic quality result.

## Preserved earlier attempts

`validated-d075e4c.tgz` is the complete prior frozen run:742lib+1CLI+7runner and
9public episodes passed before published-parent alignment. The final runtime
change from that source is the equivalent OwnedFileLock type path; ancestry and
documentation now align with74c9. Neither completed run was overwritten.

`development-attempts.tgz` preserves failed probe requests (invalid MCP minScore1.1,
wrong scoped-daemon recordAccess syntax, then missing daemon SearchInput fields),
compile/format attempts and the first serial suite (732pass2failed). Those two
inherited expectations were corrected: default MCP get now observes exactly once;
feedback receipt counting distinguishes the existing graph receipt. Two formatting
attempts saw truncated bind-mount reads despite complete host files. New uniquely
named source snapshots were then used; no filesystem root-cause claim is made.
The development archive labels its captured source as uncommitted pre-ownership;
earlier logs came from earlier working revisions and are not misattributed to1d.

Original c9/b2 and981/b5 evidence remains unchanged. [artifacts.json](artifacts.json)
lists all archive sizes and SHA256 digests. No target/cache/binary outputs are
committed.

## Remaining scope and handoff

This completes this bounded legacy consumer join, not A6 or native-v2 serving.
Current effects epochs do not provide coherent readGeneration, a whole-store
barrier, controlled public clock, RNG restoration or frozen behavioral caches.
Successful legacy wire envelopes do not gain public keyed-query receipts.
See [A6-CONTROL-PLAN.md](A6-CONTROL-PLAN.md) for the required native-writer/A1
transitive-work and snapshot prerequisites, isolated controls, public schema fault
matrix and A5 trace boundary. Curie/Pascal/Hume received the exact frozen interface;
no shared native intent type or instrumentation was activated here.

## Changed files

Own consumer revision d075e4c relative to dependency join5551feb (additional
inherited47 files are visible when reviewing against74c9):

- `docs/implementation/observed-consumer-join.md`
- `src/api/adapters.rs`
- `src/api/errors.rs`
- `src/api/handlers.rs`
- `src/api/state.rs`
- `src/api/validation_tests.rs`
- `src/api/vector_query_tests.rs`
- `src/daemon/observed_join_tests.rs`
- `src/daemon/read_only_tests.rs`
- `src/decay/observation.rs`
- `src/decay/observation_tests.rs`
- `src/decay/storage_adapter.rs`
- `src/mcp/bridge_adapters.rs`
- `src/mcp/tools.rs`
- `src/search/adapters.rs`
- `src/search/pipeline.rs`
- `src/search/strength_tests.rs`
- `src/storage/control.rs`
- `src/storage/metadata.rs`
- `src/storage/metadata/effects.rs`
- `src/storage/metadata/observation.rs`
- `src/storage/metadata/observation_tests.rs`
- `src/test_support.rs`

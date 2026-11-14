# Public v1 transport characterization

2026-11-14. Runtime parent `03d9b4d1450be0cd38fe532f0098447927f4f636`.
Actual production CLI, MCP parsing/handlers/adapters and HTTP server, with an
isolated loopback OpenAI-compatible fixture server. No private ingestion or
replacement access recorder. [Runner and scope](README.md).

**Nine episodes completed, but full conformance did not pass.** All three fresh
replays of each transport passed the scoped assertions below. MCP accepted
`quality: "bad"` and changed durable files; REST rejected the same wrong type.
The final machine-readable summary explicitly sets `conformancePassed: false`.
This is evidence of the existing v1 validation gap, not a waiver or a v2 claim.

## Observed results

| Check | Stdio MCP | HTTP MCP | REST |
| --- | ---: | ---: | ---: |
| Fresh episodes, each with process restart | 3 | 3 | 3 |
| Public records unchanged during repeated false reads | 3/3 | 3/3 | 3/3 |
| Durable-file hashes unchanged during false reads | 3/3 | 3/3 | 3/3 |
| Omitted default scope matches explicit default, excludes foreign distractor | 3/3 | 3/3 | 3/3 |
| Seven malformed requests + one fixture miss rejected without durable-file changes | 24/24 | 24/24 | 24/24 |
| Quality 4 preserves stability with decay multiplier zero | 3/3 | 3/3 | 3/3 |
| Quality 1 lowers stability without deleting content | 3/3 | 3/3 | 3/3 |
| Delete removes summary and full text; tombstone persists through restart | 3/3 | 3/3 | 3/3 |
| Public record fields and post-delete ranks agree across restart | 3/3 | 3/3 | 3/3 |
| Present string quality rejected without mutation | **0/3** | **0/3** | 3/3 |

The final run created 36 accepted synthetic source records, nine foreign
namespaces, 18 valid feedback operations and nine explicit deletions. There
were six additional malformed-quality acceptances, all explicitly retained as
violations. Raw traces contain 612 response/notification events: 210 stdio,
210 HTTP MCP and 192 REST. Attempt records are written before sending requests;
no transport failure or unkeyed write is silently retried.

The fixture embedding server received 126 requests: 117 successful exact-input
replays and nine intentional missing-input requests returning HTTP400. Each
miss traversed the public store/provider path and caused no durable-file change.
No external connection or paid inference was possible in the network-none
container. Embedding profile `public-fixture-v1`, d4, empty document/query
prefixes; these vectors establish contracts, not semantic quality.

Unique-source evidence retention and retrieval Recall@20 were both 1.0 in all
nine episodes, scored separately. The sole annotated evidence source is the
raw `amber` record. This is a minimal source-mapping check, not extraction or
answer quality. UUIDs are retained with a bijective source mapping. All runs
returned mapped ranks `[turn-001, turn-002, turn-004]` initially and
`[turn-001, turn-004]` after deletion/restart. Public fields match exactly across
restart within each episode; wall-clock timestamps differ across fresh episodes
and are never claimed to be identical logical snapshots.

Physical comparisons retain every file hash but exclude `fts.db-shm` from the
durable equality assertion. SQLite's WAL-index coordination sidecar changes on
reads. `meta.db`, `edges.db`, `fts.db`, `fts.db-wal`, `fulltext.dat`, namespace
`vectors.dat` files and the lock file stayed equal during false-read windows.
A bounded 200ms quiet interval is not a transitive readiness barrier or proof
about inaccessible in-memory caches, history, receipts or pending tasks.

## Exact isolated validation

Command:

```sh
python3 scripts/public-conformance.py \
  --cache /tmp/recalld-unit-tests-l__ja1yo/cargo \
  --build-cache /tmp/recalld-public-conformance-jfl5ab6t/target
```

Final scratch: `/tmp/recalld-public-conformance-jl5_spns/output`.
Image `recalld-ci:1.94`, ID
`sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`.
Rust 1.94.1, aarch64 Linux, Python 3.11.2. Docker `--network none`, `--cpus 2`,
`--memory 4g --memory-swap 4g`, `CARGO_BUILD_JOBS=2`,
`RUSTUP_TOOLCHAIN=1.94.1`, read-only root, dropped capabilities, tmpfs `/tmp`.
Only the isolated scratch was mounted; child processes received an explicit
allowlist with temporary HOME/config/data/socket override and a synthetic key.

- `cargo fmt --all -- --check`: passed.
- `cargo test --offline --locked --lib --all-features`: **518 passed**, zero failures.
- `cargo clippy --offline --locked --all-targets --all-features`: passed with
  existing warnings (230 library, 231 library-test including duplicates, one binary).
- `cargo build --offline --locked --bin recalld`: passed. This is a debug binary;
  wire timings include Python overhead and are not release performance results.
- Three fresh replays per actual transport, including restart: scoped assertions
  and within-transport outcome replay passed; the known quality violation remains.
- All 125 source-input hashes and 111 output hashes verified independently.
  Initial and post-deletion mapped ranks also reconciled across all nine episodes.

Harness SHA-256:
`e887b16a9381f1a95d05c37992ff460cd4d4c7e966de9b556a23d70567db60a8`.
Fixture SHA-256:
`7a347fd0f97ce94ead4fa9f978c39de6060f804e0d5a0eb3afbeaa06812e9d00`.
The fixture and runner files are pinned by the manifest even though they were
new untracked inputs at execution time; a Git dirty patch alone is not their
provenance. No core source was edited.

## Retained artifacts

[evidence.tgz](evidence.tgz): 181,705 bytes; SHA-256 `3f041a5d8237bd8e8c4a3f87da708ad80355c8f5fe60620fe1ec3721b7c7e3bb`.

[development-attempts.tgz](development-attempts.tgz): 359,540 bytes; SHA-256 `8f48eca5191bb289d6823b6e0daf11c91da6db40f93a14976809ce67d9e94a5a`.

The final archive contains all wire attempts/responses, source mappings, turn
ledgers, read/restart/quality state evidence, provider requests, configuration,
source/output manifest and build/test logs. It also includes the exact runner,
fixture and image recipe. No caches, binaries or database files are packaged.

## Development attempts retained

[Earlier attempt evidence](development-attempts.tgz) retains raw output manifests,
logs and exact runner/fixture source for all five earlier attempts:

1. 518 library tests passed, but the broad all-features Cargo command then exited
   101 in rustdoc without an individual failing doctest diagnostic. A separate
   offline `--doc -- --nocapture` rerun reproduced it. Final validation explicitly
   uses `--lib`; **doctests are not claimed passing**. No core fix was attempted.
2. Public records and durable files matched; only `fts.db-shm` changed. The
   physical comparator was corrected to distinguish the SQLite sidecar, retaining
   its raw hashes rather than discarding evidence.
3. The fixture incorrectly expected positive grade-4 stability growth with
   multiplier zero. The current FSRS planner correctly preserves stability in
   this case. Added grade-1 weakening as an observable public feedback assertion.
4. Public fields survived restart, but an assertion compared post-delete ranks
   with the original pre-delete fixture. Corrected to compare against the state
   immediately before restart.
5. All six MCP episodes completed. REST collection requests with trailing slashes
   returned 404; the actual routes are `/memories` and `/namespaces`. Corrected
   the client spelling. The final fixture additionally stores/deletes full text.

These were failed development attempts, not passed episodes. The final evidence
bundle contains the complete rerun with the final source/fixture hashes above.

## Remaining work

This is an independently useful public transport foundation, not all of A6.
A1 canonical snapshots/reset, generations, receipts, readiness and failure
recovery are unavailable; no substitute storage/control protocol was invented.
Public manual-clock control and chronological lifecycle schedules are untested.
Default exposure semantics, graph/batch reads, supersession, namespace/profile
validation matrices, strict quality repair and complete canonical-state parity
remain outside this slice. No semantic dataset, answer model, live spend, RL,
release budget or performance promotion is included.

The read-only parent retains HTTP vector-only/similarity EmptyQuery behavior.
Successful HTTP similarity can be added on a separately agreed combined parent
with vector fix `f022127`; this branch did not merge it. Future FTS PR29 cost
measurements must call the actual v2 internal entry point explicitly. The earlier
foundation and both paired study branches remain unchanged.

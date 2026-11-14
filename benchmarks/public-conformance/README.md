# Real public v1 transport characterization

This standalone harness is based on read-only runtime `03d9b4d`, without merging
independent evaluation/study branches. It drives the real `recalld` binary over
MCP stdio, HTTP MCP `/mcp`, and REST. All stores and mutations traverse production
parsing, validation and adapters. It does not use private ingestion or replace
the access recorder. This is a limited v1 diagnostic, not public-v2 certification.

## Run

Prepare the image separately (may require network for Rust components):

```sh
docker build -t recalld-ci:1.94 - < scripts/public-conformance.Dockerfile
```

Then run with already cached dependencies and a previous isolated build cache:

```sh
python3 scripts/public-conformance.py \
  --cache /tmp/recalld-unit-tests-l__ja1yo/cargo \
  --build-cache /path/to/isolated/target
```

The supervisor copies only tracked `src/**/*.rs` and six explicit Cargo/harness/
fixture/recipe inputs. It excludes unknown untracked files, secrets, local Cargo
config and build scratch. Missing or symlink inputs fail closed. Every copied
file is hashed, including new harness files absent from the Git diff. Cargo and
target are copied into separate scratch directories. Builds/tests/episodes run
with `--network none`, 2 CPUs, 4 GiB, two build jobs, Rust 1.94.1, read-only root,
tmpfs `/tmp`, dropped capabilities and only the scratch mount. No host home,
credentials or daemon sockets are mounted. Results and source manifests remain
in the printed scratch output directory. Bulky caches never enter Git.

A stdlib loopback HTTP server implements the actual OpenAI embeddings request
shape. Its complete input/profile lookup is fixed by `fixture.json`; misses
return HTTP400 with no live fallback. The child gets an explicit environment
allowlist and a synthetic placeholder API key, never host credentials. Artificial
vectors test contracts, not semantic relevance. Labels/mappings stay evaluator-side.

MCP pins protocol `2025-06-18`, performs initialize/initialized/tools discovery,
and sends normal JSON-RPC tool calls. HTTP MCP uses its real legacy session.
For stdio, an explicit temporary daemon socket has a regular file as its parent:
the existing CLI fails daemon setup before spawn and enters its real direct
fallback. The harness checks the fallback log. This is not a daemon-client test.

## Scope and interpretation

Each transport has three fresh stores/process replays, each including a restart.
The fixed four-source fixture has three default-namespace records and a foreign
namespace distractor with an identical query embedding. It checks omitted versus
explicit default scope, successful store/recall/get with false mode, repeated
read-only windows, valid grade-4 feedback (unchanged stability under multiplier zero), grade-1
feedback (lower stability with content retained), tombstone content absence, rejection of
seven malformed requests and one deliberate embedding-fixture miss, and persisted public fields/ranks across restart.
Generated UUIDs are retained and mapped bijectively to source IDs. Raw timestamps
are retained; equality is asserted within an episode across reads/restart. Across
fresh replays, only declared outcome flags and mapped source ranks are compared.
There is no claim of equal full logical state or timestamps across replays.

Present string-valued reinforcement quality is probed separately. A v1 acceptance
or state change is retained as a conformance violation in the summary, not hidden
as a successful rejection. A completed characterization can have
`conformancePassed: false`. No unkeyed write is retried on transport errors.

Public record comparisons omit fields that the API does not expose. Physical
file hashes across a bounded 200ms quiet read window supplement those checks.
The comparison excludes only `fts.db-shm`, SQLite WAL-index coordination state;
its before/after hashes are still retained. Durable SQLite DB/WAL, metadata,
edges, vectors and text stay in the comparison. These observations
are not a canonical snapshot, transitive readiness barrier, in-memory cache
inspection, event/receipt proof or concurrent-writer guarantee. Background sweeps,
autolinking, RIF and warm-file persistence are disabled. The actual system clock
remains active; there is no public manual-clock control. Restart is a normal
process restart of the same synthetic directory, not snapshot reset or fault
injection. HTTP shutdown follows the current CLI's two-stage signal sequence.

Source retention and unique-source retrieval recall are scored separately;
unknown evidence is unscored. No extraction model, answerer, judge, paid provider
or RL is used. Timings in traces include the Python client and debug binary;
they are diagnostic, not production performance measurements.

Still outside scope: A1 generations/receipts/barrier/snapshot/reset, full canonical
state/hash replay, controlled chronological lifecycle, concurrent races, graph
and batch reads, supersession fixtures, strict-validation repair, real semantic
datasets and grouped statistical evaluation. The parent retains the known HTTP
vector-only/similarity EmptyQuery limitation; successful HTTP similarity needs a
subsequent combined parent with the independent vector fix. No full A6 completion.

# A5 bounded pipeline instrumentation protocol (preregistered)

Parent/anchor: frozen Observed PR58 `3b590fb28c84bb2aa2d8afaac66e34556d7da6a6`.
The anchor receives only the identical probe source, no instrumentation. The
candidate is measured with the performance target disabled and with full event
emission enabled. No broad integration ancestry is required. Actual prepared
selection/hydration/exposure/final-admission interfaces are frozen with Halley;
`effects_stamp` is an admission epoch, never a canonical GenerationRef/barrier.

Hardware: Apple Mac17,6, 18 host logical CPUs, 38,654,705,664 bytes host RAM;
Docker ARM64 Linux. Image recalld-ci:1.94, ID
`sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`.
Rust 1.94.1, release, same lockfile/probe, 2 CPUs, 4 GiB, 2 build jobs. Runtime
network none; no home, credential or socket mounts. Separate private targets.
No benchmark arm runs concurrently with another arm or an intentional build.
A quiet interval must be coordinated with the main agent after both final
binaries are built. Capture host load and concurrent Docker containers before
and after every arm, plus a periodic host/container observer during the run.
If exclusivity is unavailable or unrelated builds run during measurement, all
results are characterization only: no <=5% promotion claim regardless of ratios.
Interruptions/failures remain in raw evidence. This is build-exclusive, not
whole-host exclusive: unrelated long-running background containers stay running.
Snapshots publish aggregate counts by image, CPU totals and memory usage only;
no external project/container names or IDs. Before the first measured arm,
three host CPU interval samples must each show at least 90% idle, with at most
five percentage points of spread and successful container capture. Failure
stops a quiet run before measurement; it does not silently change mode. Periodic
samples and all competing activity still require review after this preflight.
The coordinator supplies an explicit UTC deadline; the runner obeys the earlier
of that deadline minus 15 seconds and its own 19m45s scheduling cutoff.

Frozen workload: 10,000 canonical legacy records, 384-dimensional seeded
SplitMix64 unit vectors, 10 namespaces with 10% of records in the queried scope,
hybrid vector+FTS, k=20, graphDepth=0, fixed clock 1767225600000, recordAccess=false,
cache bypass, no provider calls. Exact engine QueryEngine with production
storage/vector/FTS/graph adapters; no second query implementation. Graph dense
work, true exposure/publication, cancellation and failure classifications are
correctness probes, not claims measured by this latency cell.

Five process pairs/seeds: 7,19,41,73,101. Each seed supplies a closed store snapshot, freshly copied into each arm. Order: A-D-E, D-E-A, E-A-D, A-E-D, D-A-E (anchor/disabled/enabled).
Each arm: 200 warmups, 1,000 measured requests, then at least 60 seconds and
1,000 completed saturated sequential requests. Full enabled JSON traces go to a
buffered file; flush occurs within EVERY timed request and throughput iteration.
No sampling or asynchronous exporter drops. Sink failure is an error in the
benchmark, never a silent missing sample. Raw latencies/events/summaries and
failed attempts are retained. Canonical record/config/control/vector hashes and result UUID+score hashes
must agree across arms. Canonical file bytes must be unchanged within each arm's
query interval. Do not compare physical redb file hashes across close/reopen:
functional smoke observed different pre-query hashes across sequential opens,
while each query interval was byte-identical. This is not evidence of query
writes; preserve those smoke attempts and use fresh snapshot copies per arm. The store initialization/build cost is excluded and
reported as setup; derived FTS construction finishes before timing begins.

Pre-measurement absolute budgets for this isolated cell: warm p95 <=25 ms,
steady completed throughput >=40 requests/s, peak RSS <=1 GiB, zero failures and
zero canonical changes. These are experimental budgets, not production SLOs.
Instrumentation gates compare both D/A and E/A: p95 ratio <=1.05 and throughput
ratio >=1/1.05. Report individual pairs and a paired bootstrap 95% interval over
process seeds (10,000 resamples, seed 20261114), not pseudo-independent queries.
Five pairs can be inconclusive; no promotion unless the adverse interval bounds
clear both gates. Record the compiled-disabled comparison even if enabled fails.

This slice adds internal tracing, not public diagnostics or full A5 completion.
No ANN/backend/cache/durability/ranking/filter/lifecycle changes. Transport
serialization/network enqueue, provider queues, RIF pair counters, fsync detail,
compaction and A1/A6 transitive readiness barrier are remaining scope. No sleep
or empty-queue stand-in for readiness. Added data contains bounded operation/stage
names and aggregate counts, with actual committed receipt identity only in
isolated trace fields (never metric labels); no memory IDs, text, tags or names.

Failure and hierarchy interpretation: the sink-failure regression returns an
ordinary `std::io::Error` from the writer and proves that error does not replace
actual committed-not-ready classification or receipts. It does **not** test or
promise safety against arbitrary panicking subscribers, including double panics
while a Timer/Timeline is dropped during unwinding. No panic-catching layer is
introduced here. Disabled Worker context and its queued timer share one decision.

Each performance request span has an internal monotonic `trace_id` and event
version 1; JSON's ordered `spans` stack preserves parent/child trace IDs. A nested
engine request is part of its REST/MCP parent, not additional wall time. Likewise
`duration` adapter/lock intervals nest inside `exclusive_pipeline` intervals.
Only consecutive exclusive intervals plus their explicit residual partition the
pipeline total. Synchronous emission cost falls into the next interval/residual;
request latency also includes the final event and explicit sink flush. These
are internal event fields, not a public response or A6 readiness schema.

Exclusive window: request 20 minutes after both binaries, fixtures and all builds
are ready. Expected 16–18 minutes for 15 sequential arms. Stop scheduling by
19m45s, per-arm timeout at most 90s or remaining window, and remove only the
probe's own named container on timeout. Preserve incomplete/timed-out attempts;
do not replace them silently or promote from incomplete pairs. Setup is outside
this window. Host snapshots continue through cleanup.

Measured source remains the frozen `3b590fb` parent even though PR58 has since
advanced to `faa0b0a`. Publication must name the actual measured candidate and
probe-only anchor commits. Later native/writer/PR58 integration requires its own
validation and is not covered by this benchmark.

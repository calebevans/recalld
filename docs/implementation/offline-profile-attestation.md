# Offline schema, profile attestation and retained-state transfer

This CLI uses A1's canonical schema migration and logical export/import APIs,
including their profile checks and staging capacity guard. Its minimal parents
are PR41 e7c0331 and PR49 faf0f81, with no temporary integration-branch dependency.
It does not activate ordinary schema-v2 serving. `recalld-schema inspect SOURCE`
performs owned, validated inspection without writable canonical opens and reports
source control identity, namespace bindings and any durable migration identity.

To migrate a stopped, generation-text store to a **fresh destination**:

```
recalld-schema migrate SOURCE DESTINATION --at 1794614400000 \
  --expected-store-id STORE_UUID --expected-generation GENERATION \
  --attest-profile attestation.json
```

`--at` is an explicit Unix-millisecond migration time. Generation is a canonical decimal u63 from inspection, not a public whole-store readiness certificate. Existing legacy text layouts first require the separate `recalld-text-migrate` fresh-destination step. No command starts a daemon or contacts an embedding provider.

An attestation file is strict JSON with `namespaceId`, `namespaceName` and `descriptor`. The descriptor uses the canonical EmbeddingProfile fields described in canonical-schema.md. Supplying the file explicitly asserts that **all retained vectors in that namespace originated from that exact profile**. Width, model names in current configuration and cached labels cannot establish that provenance. Repeat the flag for multiple namespaces. No flag means unknown legacy provenance stays unknown; it does not infer a binding.

The shared migration path resolves canonical namespace ID/name, reuses profile preparation, checks owned vector integrity and the declared normalization without altering bytes, and atomically adopts OperatorAttestation provenance in inaccessible staging. Existing bindings cannot be re-attested or replaced, even with an equal descriptor. A different model/prefix at the same dimension is not compatible. Ordinary serving remains blocked by the all-writer barrier.

Successful output contains the durable canonical migration state. Explicit profile attestation advances the source generation once and includes a receipt. Format-only migration preserves the generation and reports a null receipt. Retrying an already published destination is refused, not replayed as a new write. Use `inspect DESTINATION` to reconcile the migration ID, receipt operation ID, lineage and exact binding. A rejection never authorizes deleting/overwriting a destination. Migration keeps the source unchanged; operator cutover and any future serving activation are separate actions.

This does not reconstruct labels already lost under the legacy Tag alphabet. Native Unicode/multiword future writes and their actual store/reopen regression still need the participating writer path.

## Validation

Full all-feature offline suite passed **559 library tests and four CLI integration tests** (three new schema CLI tests plus the existing text migration CLI test); four doctests ignored. Formatting and all-target/all-feature Clippy passed with repository warnings. After removing two avoidable test clones, the three new CLI tests and formatting/Clippy were rerun and passed. Production code was unchanged after the full run.

The actual subprocess tests verify explicit and omitted attestation, canonical source-byte preservation, descriptor/provenance persistence, retry reconciliation with unchanged migration ID/generation, canonical scope/lineage mismatch, strict generation strings, bad descriptors and JSON, duplicate namespace attestations, nonunit vectors, rebind rejection and continued ordinary-serving rejection without canonical byte changes. Format-only migration preserves generation and has no receipt; explicit attestation advances once. The CLI prints the unrecoverable legacy-label warning on success.

Validation used recalld-ci:1.94, RUSTUP_TOOLCHAIN=1.94.1, network none, two CPUs, 4GiB, CARGO_BUILD_JOBS=2 and private review cargo/target caches. No home, credentials or daemon socket mounts. Logs: /tmp/recalld-implementation/profile-attestation-validation/.attestation-{full,final,clippy}.log.

## Retained-state export and import

The operator consumer joins PR41 e7c0331 with PR49 faf0f81, including its staging
capacity guard. It uses the existing offline API without importing ManifestV2,
restore, catalog or temporary integration history.

```
recalld-schema export SCHEMA_V2_SOURCE FRESH_FILE.jsonl
recalld-schema import FRESH_FILE.jsonl FRESH_DESTINATION
```

Export requires a stopped schema-v2 source and a fresh file outside that source.
Import validates the stream and available staging space through the shared API.
Retry the same file and destination to resume its bound staging operation or
validate an already completed import. An unrelated existing destination is
rejected. No command replaces a serving store or infers legacy provenance.

Success prints the shared ExportSummary JSON on stdout: exportId, source
{storeId,generation}, chunks, records, logicalSha256. Errors use stderr and a
nonzero exit status. Successful import also states on stderr that the destination
remains offline. It does not enable ordinary serving or claim derived readiness.

Actual subprocess regressions exercise unknown and explicitly attested profile
round trips, exact logical re-export hashes, source/input byte immutability,
completed-import retry, refusal to overwrite exports or unrelated destinations,
future/malformed/truncated stream rejection, and continued ordinary-serving
rejection. Backend process-interruption and capacity tests remain owned by the
shared importer; the CLI does not introduce its own recovery protocol.

Final consumer validation:567 library tests,5 schema/profile CLI tests and1
text-migration CLI test pass under normal parallel scheduling;4 doctests ignored.
The library run includes insufficient-capacity/source-immutability and import
process-interruption recovery regressions. Formatting and all-feature/all-target
Clippy pass with warnings. Commands: `cargo test --offline --locked --all-features`,
`cargo fmt --all -- --check`, `cargo clippy --offline --locked --all-features --all-targets`.
Runs used recalld-ci:1.94/Rust1.94.1, network none,2 CPUs,4GiB,2 build jobs,
private sequential Cargo/target caches and synthetic stores; no home, credentials
or daemon socket mounts. Logs: `/tmp/recalld-implementation/schema-export-cli-validation`.

Remaining scope: participating ordinary writers and runtime profile enforcement/discovery,
native Unicode/multiword future-write/reopen validation, complete backup integration,
and public versioned contracts. These offline consumers do not establish full A4
completion or authorize public v2 discovery/envelopes before writer readiness.

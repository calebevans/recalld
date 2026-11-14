# A4 profile validation and attestation preparation

This dependent slice uses canonical schema codec commit 67ce928. It adds Rust consumer helpers in embedding::profile; it does not connect ordinary transports or enable schema-v2 serving. Canonical model/schema.rs is unchanged. Use its existing EmbeddingProfile::validate and profile_id methods; there is no second identity hash or model router.

parse_profile reads raw JSON directly into the strict canonical descriptor and validates supported versions, enums, preprocessing and integer dimension bounds. Duplicate/unknown fields, wrong types, mandatory null/missing fields, and out-of-range dimensions fail. Missing or null modelRevision remains unknown; Some(empty) remains a distinct exact identity. Model IDs and prefixes are neither trimmed nor case folded. Errors omit supplied values.

require_compatible validates a canonical namespace schema and existing binding against an explicitly supplied active descriptor. Unknown legacy bindings (including absent schema) fail rather than being inferred from dimensions/configuration. Equal-width model, provider, revision or prefix changes fail. Success establishes descriptor compatibility only; it does not establish a provider runtime, index generation or writer readiness. Metadata inspection/export does not need this semantic-operation check.

prepare_attestation requires a matching immutable namespace ID, migrated valid namespace schema, UnknownLegacy binding and matching profile width. It rejects every existing binding, including an identical one. Its PreparedAttestation is only an operator proposal carrying namespace ID, descriptor and profile ID. It does not produce an EmbeddingBinding or change provenance, namespace state, records or vectors.

Feynman/A1 explicitly requested preparation-only coordination: persisted attestation still needs independently validated vector provenance and ownership, atomic namespace adoption and the all-writer barrier. Offline migration must preserve UnknownLegacy and exact existing bindings. No operator assertion alone silently blesses live vectors. The native Unicode/multiword structured-label write/reopen regression remains pending the participating writer path; this slice makes no substitute codec-only completion claim. Known legacy labels already dropped cannot be recovered.

## Validation

Isolated Docker recalld-ci:1.94, Rust1.94.1, network none, two CPUs,4GiB,CARGO_BUILD_JOBS=2, private review cargo/target, no home/credentials/socket mounts. Full all-feature suite passed554 library tests+1CLI integration test;4doctestsignored. Formatting and all-target/all-feature Clippy passed with repository warnings. Three new tests exercise strict descriptor input, same-width identity mismatches/unknown provenance and nonmutating attestation preparation. Logs: /tmp/recalld-implementation/profile-consumer-validation/.profile-{full,clippy}.log.

Remaining A4 scope includes validated generated/precomputed vectors and provider batches, persisted attestation/reembedding adoption, canonical namespace binding across all writers/search consumers, actual structured-label future-write/reopen coverage, truthful runtime profile discovery, receipts/envelopes and coherent exact-scan pagination. None is advertised as ready by this helper module.

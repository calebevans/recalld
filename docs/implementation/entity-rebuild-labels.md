# Canonical entity rebuild parity

Parents are PR45 `fb7ae7edf5a228dcb29824334b7eaeddc9327530`, PR35
`852221e`, and updated PR40 `e7fd455`. PR45 supplies actual derived completion
and its publication boundary; PR35 supplies namespace-partitioned, idempotent
postings; PR40 supplies authoritative `CachedRecord.entities`. No insertion,
maintenance, Observed, catalog or evaluation branch is added as a parent.

Startup and `DerivedCompletion::complete` build their fresh entity views through
one `EntityIndex::from_records` function. It uses the shared disk-to-cache
conversion rather than reparsing tags, excludes Ghost/Tombstone records, and
passes the canonical namespace and UUID to the existing index. Entity-key
normalization, alias weighting and UUID tie order are unchanged. Rebuilding
replaces the complete view, so overwritten labels, authoritative empty lists and
deleted records cannot leave old postings behind. Existing publication locks and
effects acknowledgement still determine when the completed view becomes usable.

Canonical schema entities retain exact UTF-8, order and duplicates in the cache
conversion; the index intentionally normalizes and deduplicates matching keys.
Schema-less records still use the legacy retained-tag parser. Canonical
`LegacyRetainedOnly` provenance is unchanged: rebuilding cannot recover names
lost by legacy tag encoding.

## Proof scope

The schema-v2 regression feeds encoded/decoded canonical records into the actual
shared production rebuild function. It covers UTF-8, duplicate/alias count
idempotence, conflicting retained tags, empty labels, namespace isolation,
replacement/disappearance, removed phases and unchanged canonical bytes and
provenance. It does not admit schema-v2 serving or invent a native write.

Separate tests run real legacy `Recalld` startup/reopen and real pending-deletion
`DerivedCompletion` over isolated storage. They check namespace counts, stale
posting removal, repeated completion, another canonical deletion, and successful
effects acknowledgement. Full native write/reopen/publication proof remains a
dependency on the schema-aware writer and readiness join.

## Remaining consumers

A4 filter and hydration paths remain a separate consumer slice: they need full
canonical entity/topic/emotion labels and provenance without weakening legacy
hard-filter validation. Feynman's store/link canonical-count follow-up must
preserve accepted raw legacy input names and use canonical labels for native
records; legacy tags are never a complete substitute. Those owners have been
notified. This change does not enable public v2 or claim full serving coherence.

## Validation

With the shared function still using the old tag parser, the focused rebuild run
fails its canonical-label regression (four indexed records instead of three);
three other rebuild tests pass. With the canonical cache conversion, all four
rebuild tests pass inside `cargo test --locked --offline --features bench`:
**639 library + 1 CLI tests passed**, zero failures, four existing ignored
doctests, normal parallel execution. `cargo fmt --all --check` and all-target,
all-feature Clippy pass with existing warnings.

Runtime: `recalld-ci:1.94`, `RUSTUP_TOOLCHAIN=1.94.1`, network none, 2 CPUs,
4 GiB, 2 Cargo jobs, private copied cargo/target caches; no home, credentials or
socket mounts. Logs retained outside commits in
`/tmp/recalld-implementation/entity-rebuild-labels/.validation/`:
`before.log`, `all-tests.log`, `fmt-check.log`, `clippy.log`. This is correctness
validation, not a performance, full native writer or serving-readiness claim.

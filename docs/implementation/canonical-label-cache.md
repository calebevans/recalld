# Canonical entity labels in CachedRecord

Parent: PR40 `69917f5e65a2da06580aebae3fe3aee4b5391bfd`. No Observed,
native-writer or namespace-postings parent is required for this conversion.

`CachedRecord::from(&DiskRecord)` now copies `schema.labels.entities` verbatim
when schema is present: UTF-8 bytes, case, order, duplicates and authoritative
empty lists. Schema-less records retain the existing tag parser. Normalization
belongs to matching/index keys, not this conversion. Canonical label provenance
is unchanged; `LegacyRetainedOnly` never proves the full original label set.
No cache fields, wire schema, writer or serving gate changes.

Tests round-trip actual schema-v2 record bytes before conversion and cover native
UTF-8 (including distinct composed/decomposed forms), order, duplicates, the
128-byte limit, empty labels with retained tags, legacy parser parity and
retained-only migration provenance. These are codec/conversion tests, not proof
of native write/restart or public v2 serving.

## Remaining consumer work

Read-only audit sources: integration `75882c3`, Observed `1d52153`, schema
`69917f5`. This patch does not complete the following paths.

- **Derived entity rebuilds:** `src/system.rs` and `src/derived_completion.rs`
  still parse tags directly. A narrow follow-up should consume canonical entities
  for schema-bearing records and preserve legacy parsing. Retain PR35 namespace
  + UUID replacement/idempotence and normalized matching keys. Required parents
  are this conversion, PR35 and the actual derived-completion interfaces; no
  arbitrary broad integration ancestry. Test repeated rebuilds, replacement,
  removal, namespace isolation, UTF-8 and authoritative empty lists. Actual
  native write/reopen proof additionally needs the native writer/readiness join.
- **Store/link counts:** retain accepted raw input names from d8d0406/PR35;
  legacy tags cannot reconstruct them. Native record link counts should use
  same-scope canonical labels and existing distinct normalized-key counts, with
  eligibility before cap. Cache-based counts and legacy fallback do not establish
  full canonical label coverage. Coordinate native insertion/completion owners.
- **Filters and hydration:** structured filters still translate to restricted
  `Tag` values; JSON/MCP/REST response paths reparse tags. Canonical UTF-8 entity,
  topic and emotion predicates and responses require full labels and provenance
  through internal result/hydration interfaces, coordinated with A4. Preserve v1
  compatibility and fail-closed invalid filters; never omit an unencodable
  predicate. This is separate from the small entity cache conversion and requires
  its own transport/native-writer proofs before any v2 activation.

Recommended next bounded consumer slice: derived entity rebuild parity after
its owners freeze the relevant interfaces. Keep filter/hydration work separate.

## Validation

The three new regressions fail against the parent converter and pass with the
change. `cargo test --locked --offline --features bench`: **572 library + 1 CLI
passed**, zero failures, four existing ignored doctests, normal test parallelism.
`cargo fmt --all --check` and `cargo clippy --locked --offline --all-targets
--all-features` pass (existing warnings remain).

Runtime: `recalld-ci:1.94`, `RUSTUP_TOOLCHAIN=1.94.1`, network none, 2 CPUs,
4 GiB, 2 Cargo build jobs, privately copied cargo/target caches. No home,
credentials or socket mounts. Local logs are under
`/tmp/recalld-implementation/canonical-label-cache/.validation/`: `before.log`,
`all-tests.log`, `fmt-check.log`, `clippy.log`; initial test-development failure
also retained. Build outputs are excluded from the commit. This is functional
validation, not an instrumentation/performance or serving-readiness claim.

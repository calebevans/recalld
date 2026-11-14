# Namespace creation against the active provider

This consumer joins profile PR62 `ed242ec` and catalog PR50 `d4f756c` at
`89f5274`. It does not import the broader consumer/integration branches. The
prerequisite merge preserves both insertion and catalog receipt outcomes, scope
checks and numeric validation.

HTTP, MCP and daemon namespace adapters now hold the same immutable provider
instance used for generated writes and queries. A shared creation check resolves
omitted `embeddingDim` from that provider, not from the existing `default`
namespace or a hardcoded fallback. A supplied dimension must equal the active
dimension. The shared wire parser still accepts integers 1..8192, including both
boundaries; a parsed width is only usable if the actual provider supports it.

The actual provider's width must be in range. When a descriptor exists, its
version/fields must validate and its width must agree with `dimensions()`.
Creation does not call the provider's embedding service or infer provenance from
its display name. A valid but inactive requested width is HTTP 422 with field
`embeddingDim`, or MCP/daemon invalid input. An unusable provider configuration
is HTTP 503 / MCP unavailable / daemon -32008. Parsing range/type failures retain
their existing bad-input classification. Checks precede catalog allocation,
namespace ID assignment, filesystem staging and receipt publication.

The staged v1 exception remains explicit: schema-less namespaces and custom or
passthrough providers with `profile() == None` remain unverified. A new namespace
is still schema-less, even with a known native provider descriptor. This does not
attest legacy vectors, persist a binding, select another model, or enable a
schema-v2 writer. Explicit offline migration/attestation and the all-writer
barrier remain prerequisites for that lane. Existing namespaces are not resized.

Regressions exercise HTTP and MCP with an existing default namespace of width4
and active width8; inactive requests preserve file bytes and directory membership,
generation and next namespace ID. Omission and explicit width8 create one catalog
generation each without schema binding. Boundary parser regressions create widths
1 and 8192 only with correspondingly configured providers. Invalid descriptors,
descriptor/provider width disagreement and invalid provider widths fail without
mutation. Actual daemon frames cover invalid-input versus unavailable mapping and
successful omission using the active width. No provider calls or personal data.

Canonical UTF-8 result hydration/filter predicates are a separate consumer child.
Hume's schema/cache handoff makes `DiskRecord.schema.labels` authoritative,
including empty labels; `CachedRecord` currently carries canonical entities only.
The followup must carry entities/topics/emotions and `NativeV2` versus
`LegacyRetainedOnly` provenance, retaining schema-less tag fallback and rejecting
unencodable legacy filters. Lost historical labels cannot be reconstructed.

This change does not alter query selection. When composing with retrieval PR58
and vector PR28, supplied vectors must retain precedence and literal metadata-only
queries must not consult an active provider. Keep `GeneratedEmbedding` context
through insertion and PR60's `CommittedMutation` receipt-bearing errors. Those
consumer branches are not prerequisites of namespace creation here. No v2
capability, envelope, public profile selector or full A4 readiness is advertised.

The first combined full run passed 685 tests and failed two existing fixtures:
MCP boundary-positive controls still used the default width4 provider, and the
insertion guard fixture attempted schema-bearing creation now rejected by catalog
admission. Boundary controls now configure matching providers. The insertion
fixture correction is the exact hunk from Curie's `97f0fd9`; it introduces no purge
ancestry. The profile parent's independent final-transaction schema rejection and
byte-immutability tests remain. Neither correction changes production behavior.

Final validation: `cargo test --offline --locked --all-features` passed 687 library
and 1 CLI tests with normal parallelism; 4 doctests ignored. Formatting passed.
Clippy exited 0 with 387 library and 407 test warnings (387 duplicates), not a
warning-free result. All validation used Rust1.94.1 in `recalld-ci:1.94`, Docker
network none, 2 CPUs/4GiB and 2 build jobs with a private Cargo/target cache.

# Namespace profile consumer evidence

Frozen source `466e7549ad88c1bb8aef95de7c5e62c54d4ea8eb` on
`impl/namespace-profile-boundary`. Minimal merge
`89f5274306d2f424ad7d55d08529b3f61c03c82f` has exactly profile PR62
`ed242ec99354020089cd366bec03792e063015c4` and catalog PR50
`d4f756c148b7ed817e0ff0b1d452846fece96c16` as parents. Publish on62 with50 declared
additional prerequisite; no unrelated integration ancestry. Control merge retains
insertion and catalog receipt variants, scope guards and numeric validators.

## Validation

Three focused real-transport regressions passed. Final normal-parallel
`cargo test --offline --locked --all-features`: **687 library + 1 CLI passed,
0 failed; 4 doctests ignored**. `cargo fmt --all -- --check` passed.
`cargo clippy --offline --locked --all-targets --all-features` exited0 with
387 library /407 test warnings (387 duplicates). Not warning-free.

Rust1.94.1 (`RUSTUP_TOOLCHAIN=1.94.1`), `recalld-ci:1.94` image
`sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`,
Docker network none,2CPU/4GiB,2buildjobs. Only assigned source, private scratch
Cargo/target and log directory mounted; no home/credentials/socket mounts.
All stores and daemon socket pairs are synthetic; no paid provider calls.

HTTP/MCP with existing default width4 and actual provider width8 reject inactive
requests without file-content/directory-membership, generation or ID allocation
changes. Omitted and explicit active width8 create one catalog generation each.
Valid range boundaries1/8192 require matching providers. Invalid provider widths,
malformed descriptors and descriptor/provider disagreement fail unavailable before
catalog effects. Actual daemon framed RPC proves invalid-input/unavailable
classification and omission resolves active width. Known/native or customNone
configuration does not auto-bind schema-less namespaces.

## Retained initial failures

First full run: **685 passed /2 failed**. The exported MCP positive range probe
still asked a width4 provider to create width1; positive controls now configure
actualmatching providers. The insertion guard fixture expected schema-bearing
creation to succeed, but catalog50 properly rejects it before allocation. Exact
fixture-only correction reused from Curie's97f0fd9 without importing purge
ancestry; profile-parent canonical final-transaction rejection and byte-immutability
regressions remain. No production behavior was changed for either failure.
The tracked patch restores these two initial fixtures on the validated source
in disposable scratch only. Original failure log, summaries and full-log hash
manifest are retained. Full logs are copied to the worknotes artifact archive.

## Limits and next consumer

Creation is now an actual single-active-provider consumer. Existing v1 remains
explicitly legacy-unverified; this is not attestation or fullA4 conformance.
Unknown/bound schema-v2 ordinary serving remains gated. Query handling is unchanged.
Next canonical UTF8 hydration/filter consumer is separate: authoritative empty
labels, full entities/topics/emotions and provenance; legacy fallback only for
schemaNone and no invented recovered labels. Its required retrieval/vector join
must preserve supplied-vector precedence, provider-free literal MetadataOnly,
GeneratedEmbedding and CommittedMutation. No publicv2 readiness advertised.

## Changed source/document files

```text
docs/implementation/namespace-profile-boundary.md
docs/implementation/native-profile-boundary.md
docs/mcp.md
src/api/adapters.rs
src/api/handlers.rs
src/api/validation_tests.rs
src/daemon/server.rs
src/embedding/runtime.rs
src/main.rs
src/mcp/bridge_adapters.rs
src/mcp/tools.rs
src/storage/engine/insertion_tests.rs
src/system.rs
src/test_support.rs
```

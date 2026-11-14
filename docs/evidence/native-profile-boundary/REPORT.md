# Native profile boundary validation

Frozen source: `38a06bc186f7081ed5ace7fe670cdcfa6eccf15b`.
Minimal dependency merge: `112a058b2ac4a849fa22f9e7a301846848bd32fa`, parents
`7fa19f8927a9c1c6d46005044b37cae5eebca113` (corrected insertion) and
`2735746cb21a1868e82ab94434b8f042076be87f` (PR14 provider-local cache).
No catalog/CLI/whole-integration parent. No push or public v2 activation.

## Validation

Final normal-parallel `cargo test --offline --locked --all-features`: **657 library
+ 1 CLI passed, 0 failed; 4 doctests ignored**. `cargo fmt --all -- --check` passed.
`cargo clippy --offline --locked --all-targets --all-features` exited 0 with
373 library / 385 test warnings (including 373 duplicates). This is not a
warning-free claim; the child adds argument-count/test-layout warnings.

All executed in `recalld-ci:1.94`, image
`sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef`,
`rustc 1.94.1 (e408947bf 2026-03-25)`, `RUSTUP_TOOLCHAIN=1.94.1`,
`CARGO_BUILD_JOBS=2`, Docker `--network none --cpus 2 --memory 4g`.
Only the assigned worktree, private scratch Cargo/target and validation-log
folder were mounted. No home, credentials or Docker socket mount. Native OpenAI
fixtures use loopback HTTP inside that container, no paid calls.

Actual probes cover factory/prefix/cache descriptor forwarding, invalid vectors
and batch counts never entering cache, OpenAI single/batch decoding with exact
unique indices, same-width wrong-profile rejection before provider/index calls,
MCP unavailable code -32008, canonical final-row recheck preserving metadata,
text and vector bytes, and legacy insertion without auto-attestation. Receipt
replay succeeds even if fresh provider evidence/output would now reject.

## Retained failures

The initial compile reported three unconverted Vec arguments after internal
GeneratedEmbedding plumbing; call sites were updated with explicit unverified
conversion. The first focused run was 5 pass / 1 fail: a test expected the word
“nonzero” while the existing validator correctly says “finite positive norm”.
Only that assertion changed; two additional real-path regressions were added.
The intermediate focused run passed 8 tests before the OpenAI response test was
expanded to actual loopback HTTP fixtures. The final full run includes that test.

First full run: **656 pass / 1 fail**. The new final profile gate had changed the
existing schema writer refusal from `RecoveryRequired` to
`ProviderVectorUnavailable`. Production mapping was corrected to preserve the
original schema gate classification. The pre-existing insertion guard test is
unchanged. The tracked patch reproduces the failing classification on frozen
source in a disposable checkout; it is evidence, not a proposed fix.
Full original failure log, initial focused failure, exact summaries and full-log
SHA-256 manifest are retained here. Full logs are also copied into the worknotes
artifact archive; their original scratch paths appear in the manifest.

## Scope limits

This is actual insertion/query/provider/cache enforcement, not all A4 completion.
Schema-less v1 remains explicitly legacy-unverified; no dimension-only or
passthrough-label provenance is invented. Schema-v2 Unknown rejects semantics;
Bound requires exact ID and dimensions, and still rejects ordinary serving until
all writers participate. Namespace creation still needs the actual single-provider
check in a separate catalog-dependent child. Public precomputed profile selectors,
UTF-8 result/filter propagation, writer activation and versioned discovery/envelopes
remain separate. See ../../implementation/native-profile-boundary.md.

## Changed source/document files

```text
docs/implementation/native-profile-boundary.md
src/api/adapters.rs
src/api/handlers.rs
src/api/state.rs
src/derived_completion.rs
src/embedding/bedrock.rs
src/embedding/cache.rs
src/embedding/mod.rs
src/embedding/ollama.rs
src/embedding/openai.rs
src/embedding/prefix.rs
src/embedding/profile.rs
src/embedding/provider.rs
src/embedding/runtime.rs
src/mcp/bridge_adapters.rs
src/search/adapters.rs
src/search/pipeline.rs
src/storage/engine/insertion.rs
src/storage/metadata/insertion.rs
```

# Native profile boundary (staged v1 compatibility)

This child uses corrected insertion `7fa19f8927a9c1c6d46005044b37cae5eebca113`
and provider-local cache PR14 (`2735746`) as its only direct prerequisites.
It enforces runtime identity at actual generated insertion and semantic query
boundaries. It does not complete A4's namespace/profile contract or enable v2.

## Runtime evidence and validation

The single active OpenAI, Ollama or Bedrock provider reports the existing
canonical `EmbeddingProfile` descriptor and `ep1` identity. Document/query prefix
wrappers compose their actual prefixes; cache wrappers retain the same descriptor.
Identity contains configured provider kind, model ID, dimensions, preprocessing
versions/prefixes and normalization. Revision remains unknown (`None`). This is
configuration identity, not verification of remote weights, endpoint behavior or
historical vector provenance. Credentials, endpoints and filesystem paths are
not profile fields. Reconfiguring a native provider requires a new instance/cache.

Custom `EmbeddingProvider` implementations retain the additive default
`profile() -> None`. Passthrough also reports `None`: its display label is not
evidence about the model that produced caller-supplied vectors.

Factory-built providers validate and normalize outputs before the provider-local
cache. Generated results must have the expected width, finite elements and a
positive norm; batch count must match. OpenAI single/batch responses must have a
unique complete index sequence, reordered to input order. These failures are
provider-unavailable errors, not caller-vector errors. Cache identity is scoped
to the immutable provider instance; task and exact transformed text remain keys.

HTTP and MCP/daemon insertion carry an internal generated vector/descriptor pair
to derived completion and the final canonical transaction. Namespace/profile
checks run before provider/index work and again against the final canonical row
before payload allocation. The generated descriptor is not caller-deserializable
proof and does not enter request identity. Original supplied vector bits remain
in the request hash; advisory normalization never replaces the raw vector sent
to the final transaction. Receipt replay precedes fresh profile/output checks.

Semantic queries check the resolved namespace against the actual provider before
embedding, then validate/normalize the generated vector before index access.
Metadata-only queries do not require an embedding profile.

## Explicit compatibility exception and remaining scope

Schema-less v1 namespaces remain in the existing **legacy-unverified** lane.
Known provider descriptors are checked for validity and dimension, but matching
width does not attest compatibility with stored vectors. Providers with no
descriptor and existing v1 supplied vectors remain usable in that lane. No
namespace or record is silently bound, upgraded or labeled proven. This is an
explicit staged compatibility exception to A4's unknown-provenance semantic gate,
not full A4 conformance; legacy applications must opt into offline migration and
explicit provenance attestation/re-embedding before the future schema-aware lane.

For a canonical schema-v2 namespace, `UnknownLegacy` rejects semantic use. A
`Bound` namespace requires exact immutable profile ID and dimension equality;
even a match still rejects ordinary semantic serving/writes until the all-writer
barrier is complete. The existing disk/open and final writer gates remain closed.
Metadata reads and offline export do not acquire a provider requirement.

Remaining work includes actual namespace-create validation against the single
active provider (parser range 1..8192 is not end-to-end support for arbitrary
inactive widths). That consumer needs catalog PR50 in its own minimal child;
this change does not introduce that parent. Canonical UTF-8 label hydration and
filter predicates, participating schema-v2 writers, public precomputed-vector
profile selectors, versioned envelopes and capabilities/discovery remain separate
work. This child advertises no new public capability or readiness.

## Regressions

`cargo test --offline --locked --all-features native_profile` exercises cache
rejection without caching, wrapper/factory identities, actual OpenAI HTTP response
fixtures on isolated loopback, pre-provider wrong-profile rejection, query rejection
before indexes, MCP/daemon unavailable mapping, final-transaction canonical row
checks with unchanged metadata/text/vector bytes, and unverified legacy insertion
with replay independent of changed provider evidence. Existing insertion tests
also retain original caller-bit hashing and the double-normalization regression.
Tests use synthetic stores and `--network none` Docker; no paid provider calls.

Validation on the final source: 657 library tests and 1 CLI test passed with
normal test parallelism; 4 doctests ignored. Formatting passed; Clippy exited 0
with 373 library warnings and 385 test warnings (including 373 duplicates).
Warnings remain, including argument-count/test-layout warnings in this child.
The first full run found an existing schema-gate error classification regression
(656 passed, 1 failed). Production mapping was corrected to retain
`RecoveryRequired` for schema-bearing namespace rejection; the existing guard
regression was preserved unchanged.

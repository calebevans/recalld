# Precomputed query vectors

Independent correction based on `653c8d3`. HTTP `POST /search` now forwards its
`embedding` to the actual query pipeline instead of losing it and returning
EmptyQuery. `POST /similar/{id}` searches the indexed source embedding through
the same path and excludes the source from the HTTP response as before.

The internal `SearchQuery.query_embedding` field serializes as `queryEmbedding`,
defaults to None and is omitted from serialization when absent. Existing text
callers keep their defaults. Supplied vectors must have exactly the resolved
namespace's dimension, finite elements and a positive finite norm. The pipeline
validates them before vector/FTS/metadata retrieval or access recording. It uses
an f64 sum of squares and f64 division for L2 normalization so valid large finite
f32 values and nonzero subnormals neither overflow nor collapse to zero. Invalid
vectors produce typed InvalidEmbedding errors, mapped to HTTP 422 with field
`embedding`. JSON syntax/deserialization errors can reject invalid numbers earlier.

Precedence is explicit:

- Vector only: the supplied normalized vector drives semantic search; no text
  embedding provider is called and no FTS text is invented.
- Text plus vector: the vector drives semantic search; text remains the FTS input.
  An internal `fts_query`, when supplied, still overrides text for FTS.
- Text only: the existing provider, FTS fallback and default query mode remain.
  This slice does not change validation/normalization of provider output.
- MetadataOnly: existing metadata-only behavior remains. A supplied vector is
  validated but does not activate the vector channel.

HTTP now accepts `query` and `embedding` together using that precedence. This
replaces the previous mutual-exclusion rejection. API QueryInput::TextAndVector
carries both to the shared adapter. Existing response shapes, filters, fusion,
ranking and v1 asynchronous exposure behavior are retained.

Validation uses isolated temporary storage, real indexes/adapters and axum HTTP
requests. A test provider returns a deliberately different fixed vector and counts
calls. Tests cover successful HTTP embedding search and similarity, large/tiny
finite inputs, internal and HTTP text/vector precedence, explicit FTS precedence,
text-only behavior, invalid dimensions/zero/nonfinite vectors, a bad indexed
similarity source, and unchanged records/access history after rejection. Access
completion notifications and bounded quiet checks are test observations, not a
production readiness barrier.

Integration notes: this branch deliberately has no dependency on read-only
retrieval or A2 semantics changes. When combining, retain `record_access` forwarding
and false-mode guards; replace the read-only test's old HTTP-similarity EmptyQuery
expectation with successful/no-write assertions. A2's validation/embed stages now
live inside `search_with_semantics`; merge vector validation/selection there so
both internal modes share it. Preserve A2's existing nonfinite threshold/source
score checks. Adapt test entity-index calls if integrating namespace-index changes.
No embedding-profile identity, v2 compatibility, performance or quality claim is
made by dimensional/numerical validation alone.

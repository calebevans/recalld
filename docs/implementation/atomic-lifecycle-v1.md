# Atomic legacy lifecycle metadata

This bounded A3 slice depends on **both PR #22** (`impl/lifecycle` at
`845f2f3`) **and PR #26** (`impl/mutation-admission` at `bc01b2e`). Merge
`7ad9409` brings the actual admission prerequisite into `impl/atomic-lifecycle`.
A review may target #22 while declaring #26 as an additional dependency until
one parent lands. This is not a duplicate cherry-pick of a parallel parent.

Feedback now commits stability, strengths, sticky permastore, clamped access
anchor and the legacy manual-reinforcement history event in one redb metadata
transaction. HTTP and MCP continue through the shared adapter/planner. The clock
is sampled by the caller once; storage never samples it again.

Sweeps commit Full -> Summary and Summary -> Ghost strength, phase and retained
content changes in one transaction, without a preliminary strength write for a
planned transition. Exact v1 semantics remain: Full -> Summary clears full-text
offset/length and retains summary; Summary -> Ghost clears summary and retains
tags. Physical payload bytes are not erased by clearing pointers. Phase index
publication occurs under the admission gate after successful commit; tag and
namespace indexes remain valid because their indexed fields are unchanged.
Graph state is published after metadata succeeds, and existing cache invalidation
still runs. No graph lock is acquired inside the metadata helper.

The final storage lock still checks the planned sweep snapshot. Inside the metadata
write transaction, the helper rereads both record and namespace policy and rejects
stale lifecycle inputs, terminal records, illegal phase promotion and missing
retained summary. It patches the fresh record, preserving unrelated edge/tag
updates. The current concrete namespace config is the internal v1 policy snapshot;
there is no invented revision field or public CAS token. Global configuration is
immutable for the operation. A rejected snapshot commits nothing. Each successful
patch advances the inherited internal metadata generation once. An uncertain
redb commit uses admission's existing unavailable guard.

Lock order is graph -> outer storage -> metadata admission -> redb -> phase bitmap
for sweep publication; feedback takes storage -> admission -> redb. The effects
agent was notified of both prerequisites and these helpers. Integration must keep
any newer canonical-pending/admission checks when composing effects and schema.
This does not certify lock order for uninspected future writers.

Validation uses real temporary stores and offline Docker. Tests cover all three
patches abandoned after record/generation staging, actual generation-exhaustion
errors after record staging, unchanged state after reopen, successful reopen and
phase-index rebuilding, bounded feedback history, stale record/policy rejection,
terminal/no-promotion guards, preserved unrelated fields, and admission contention
after commit but before bitmap publication. Adapter/sweep regressions assert one
generation per operation; a real full-text fixture verifies reference removal,
retained v1 tags and reopen. These are transaction-abort/error tests, not claims of
power-loss or uncertain-commit fault injection.

Remaining A3/A1 scope: v2 logical record fields and ledger, explicit policy migration,
schema-aware projection/feedback consumers, durable lifecycle receipts and keyed
retry, public CAS, synchronous exposure parity, and the canonical effects/derived
readiness boundary. Ghost deletion and graph/edge effects remain outside this
atomic metadata patch; a policy snapshot check under the existing storage lock is
not an atomic edge/deletion transaction. No whole-store readiness claim is made.
Admission's disabled text compaction is retained; schema compaction P1/P2 review is
still a prerequisite. V1 defaults remain; ordinary v2 serving must not be enabled
before schema and explicit migration integration are ready.

Validated with `recalld-ci:1.94`, `RUSTUP_TOOLCHAIN=1.94.1`, offline locked Cargo,
`--network none --cpus 2 --memory 4g`, and separate scratch cargo/target directories:
`cargo test --offline --locked --all-features` passed **555 tests**, with four
existing doctests ignored; `cargo fmt --all --check` passed;
`cargo clippy --offline --locked --all-features --all-targets` completed with
warnings (including the existing large `StorageError` lint pattern in new helper
signatures). No warning-free or macOS validation claim. Scratch outputs are not
part of the commit.

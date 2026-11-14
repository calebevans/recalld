# Canonical writer harness compatibility

Minimal actual parents are writer restrictions `af20d7331410feef8ea1d2a8803e6a0772bb5d99`
(PR70), original offline foundation `159f987` (PR18), and A5
`16c574ba9e6fde4efa3a8ad6ba065731e67f9e94` (runtime `dec5b23`, parent `3b590fb`).
This branch does not import the tracker or broad integration history. Feynman
retains offline export/restore ownership; no duplicate example changes were planned.

Both examples now call the existing canonical `insert_memory_with_receipt` API
with `InsertMemory`, then use `Recalld::with_clock` and existing
`complete_pending_effects` for actual production startup/view publication. Raw
DiskRecord insertion, private metadata access, ad hoc effect acknowledgement and
public fixture-loader escapes are not used. The helper is example-only code;
no production Rust API is added. Sweeps are disabled, configuration is constructed
without ambient loading, and the provider is offline passthrough. All external
model/network/training calls remain zero.

The original exact-index synthetic vectors, IDs and scalar oracle are preserved.
Canonical seeding explicitly supplies supported caller-chosen memory IDs,
timestamps and vector bytes. The real writer validates and normalizes them and
creates receipts. Store/operation UUIDs remain random: this is not whole-store
deterministic replay, a public transport fixture, a ReadView or a snapshot/reset.
Production startup may create its default namespace; it is part of the new
fixture state. A bounded helper regression proves invalid vector and duplicate
ID rejection leave control unchanged, then queries the accepted record through
the actual system QueryEngine after startup.

The offline harness is now `canonical-internal-offline-v2`, with the production
access recorder and explicit false mode. It compares record bytes and canonical
control before/after its query interval. It does not fabricate a background-work
barrier. The turn ledger retains actual insertion receipts. Cross-process stable
artifacts cover deterministic fixture/record/result projections only; receipts
and store UUIDs are excluded from that comparison and are not erased from evidence.

The probe is `canonical-writer-internal-probe-v2`. Initialization completes actual
startup before writing a versioned fixture marker. Query processes require that
marker and matching seed/count, rebuild through the production path and retain
false-query canonical-file/control/record/vector checks. Its canonical-state hash
does not enumerate the entire receipt history or every store manifest. The small
smoke can lower the minimum throughput-request count; the normal default remains
1000. Smoke durations are not new performance evidence or a release gate.

Historical probe SHA-256 is
`d7c1ca01f28ce3c91cd7411596f5085a83b9ca5e0b3dd75930e4994608503079`; original A5 and
PR18 reports, measured sources and archives remain unchanged. The historical A5
matrix driver now refuses a changed probe before any Docker call/output creation,
requiring the old frozen checkout or a newly authorized/frozen protocol. No old
measurement or promotion claim transfers to this composition.

The offline runner includes tracked Rust modules under src/tests/examples/benches
and verifies explicit Cargo targets; unknown target assets/build scripts fail
closed rather than being silently omitted. Scratch, untracked inputs, ambient
Cargo configuration and secrets remain excluded. Required new example modules
and CLI sources are covered by runner regression checks. Final validation uses
separate source-specific private targets and `cargo clean -p recalld` before the
crate rebuild, preserving dependency caches and executable hashes.

Validation is pending. Planned scope: runner/example checks, three tiny offline
fresh-process replays, a 200-record d384 probe with trace disabled/enabled, full
library/CLI checks, formatting and Clippy. No new performance matrix is authorized.

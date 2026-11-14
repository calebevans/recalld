# Bounded removal ledger

`impl/removal-ledger` starts at atomic lifecycle `862ff28` and inherits its real
PR #22 + #26 dependencies. It adds payload-free removal evidence to the existing
metadata transaction domain. It does not import the parallel effects or schema
branches. Curie owns effects hooks; Feynman owns schema/export/restore consumers.
Both were coordinated before shared hooks were edited.

`meta.db` now retains `removal_ledger` and `removal_ledger_state`. The entry key is
36 bytes: origin lineage UUID, big-endian u32 namespace ID, memory UUID. JSON values
contain only those identities, optional terminal kind, removed-field mask,
operation UUID and decimal-string generation. Masks union; hard delete dominates
tombstone. The operation/generation pair identifies the latest removal update;
this compact ledger is not a complete per-operation audit log or a retry receipt.
Entries are never removed by record or namespace purge.

The mask bits are FULL_TEXT=1, SUMMARY=2, TAGS=4, EMBEDDING=8, ALL=15. Summary and
tags deliberately have independent bits: v1 Summary -> Ghost clears summary but
retains tags. Terminal removal records ALL as logical unavailability; physical
payload erasure is not promised. Phase labels alone do not prove a retained field
was stripped. Atomic phase loss and actual field removal record the corresponding
mask in the same transaction.

Actual hooks cover shared record patches (including direct full-text/summary
stripping and batch replacements), atomic lifecycle phase loss, single/batch
forget, hard delete and each namespace purge chunk. They share the canonical
transaction's one generation. Legacy unkeyed removals allocate an operation UUID
inside the operation; no public retry semantics follow. Admission/phase publication
ordering is unchanged. A final metadata insert guard rejects known terminal UUIDs
across namespaces; engine preflight also rejects before vector/text allocation.
Ordinary lifecycle patches use a current-lineage point lookup. UUID creation and
content-adding replacements scan lifetime evidence; a derived UUID index is a
future measured optimization, not a hidden truncation limit.

The additive `ControlState.removal_ledger_version` (`removalLedgerVersion` on disk)
marks ledger adoption. Marker version 1 requires both tables, correctly typed,
with a supported state version, valid key/value identities, masks, operation IDs,
generations and compatible retained records. Missing tables, unknown fields or
versions, future generations and contradictory content fail validation. Present
ledger tables without a marker fail too. Both missing tables are never silently
recreated for a marked store. The existing backup inspector runs this validator.

A genuinely new database reports `complete_from_creation`. A previously existing
store reports `priorHistory=unknown`, captures its current lineage/generation as
`trackingStartedAt`, and creates no invented historical entries. The distinction
persists on reopen. Deleting all markers and tables outside the application cannot
be detected without external provenance; no forensic tamper-proof claim is made.

## Integration interfaces

`metadata::removal_ledger::record_removal(tx, at, namespace, id, terminal, mask,
operation_id)` takes an existing `&redb::WriteTransaction` and `GenerationRef`.
It does not commit, allocate a receipt, or advance generation. The caller must
supply the transaction's already advanced lineage/generation and operation UUID.

Curie's canonical effects hook belongs **after `control::record_receipt` and
before pending-effects insertion**, inside the same transaction. Iterate each
removed `(id, record)` and call with:

```rust
record_removal(
    &tx,
    GenerationRef {
        store_id: receipt.store_id,
        generation: receipt.committed_generation,
    },
    record.namespace_id,
    *id,
    Some(TerminalKind::HardDelete),
    ALL,
    receipt.operation_id,
)?;
```

That parallel effects module is not present on this branch; its hook must land
when the branches are composed. The legacy engine and sweep delete paths here
already call the wired metadata delete transaction. No claim is made that ledger,
vector release and external edge/graph effects are one transaction on this parent.

Feynman's export registry must preserve both tables exactly:

| Table | redb key | redb value |
| --- | --- | --- |
| `removal_ledger` | `&[u8]` | `&[u8]` |
| `removal_ledger_state` | `&str` (`state`) | `&[u8]` |

`read_export(&ReadTransaction, Option<&ControlState>)` validates and returns
`Option<LedgerExport>` (typed state plus every entry). None means a genuine
pre-ledger format, not complete deletion coverage. Preserve the control marker,
coverage, origin lineage, masks and operation/generation fields unchanged.
The ledger section is losslessly serializable; existing file backup includes it
through `meta.db`. The new whole-store canonical export registry is owned by
Feynman and is not on this parent, so its integration remains a handoff requirement.

## Evidence and remaining scope

Tests exercise the real metadata writers, exact v1 masks, union/terminal dominance,
UUID rejection across namespace/purge/reopen, dropped transactions after ledger
staging, actual generation-overflow failures for each removal writer, corrupt and
missing table/marker/entry rejection, and unchanged ledger after the atomic
lifecycle precommit-abandon regressions. A real engine deletion rejects reinsertion
before payload-file growth; a real offline backup's captured `meta.db` reopens with
identical ledger state. JSON round-trip checks cover the lossless ledger section.
These tests do not exercise power-loss hardware behavior or retained-state restore.

Remaining: canonical effects composition, whole-store export registry/import,
lineage ancestry validation, captured target-ledger persistence, monotone merge and
application to restored payloads/edges/indexes before serving, and historical-mode
coverage reporting. Current restore must not be described as deletion-aware on
this branch. Older archives cannot reveal later deletions when newer state is lost.
No public restore/no-resurrection or whole-store readiness guarantee is asserted.

Validation: `recalld-ci:1.94`, `RUSTUP_TOOLCHAIN=1.94.1`, `CARGO_BUILD_JOBS=2`,
`--network none --cpus 2 --memory 4g`, assigned worktree plus separate scratch
cargo/target mounts only. `cargo test --offline --locked --all-features` passed
**562 tests**, with four existing doctests ignored. `cargo fmt --all --check`
passed; `cargo clippy --offline --locked --all-features --all-targets` completed
with warnings, including the repository's large `StorageError` pattern in added
helpers. Scratch caches/logs are excluded from commits. No macOS or warning-free
validation claim.

Urgent graph P1 follow-up: the baseline reproducer also failed on PR #22. Parent
fix `745ca69` passed 515 tests; atomic forward merge `e0187fc` passed 558 tests.
Merge `931994b` brings that corrected atomic parent into this ledger branch.
Combined validation passed **565 tests**, four existing doctests ignored,
formatting and Clippy (with warnings), using the same offline Docker constraints.
The ledger remains in the actual canonical metadata mutation transaction after
final graph revalidation. No restore/ancestry guarantee is added by this fix.

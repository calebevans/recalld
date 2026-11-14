# Legacy catalog receipt participation

Parent: minimal core join1ca36c4. Existing v1 engine namespace creation delegates
through `create_namespace_with_receipt`; its public return remains the assigned
ID. `update_namespace_policy_with_receipt` is a typed storage API, not a new
HTTP/MCP operation. These changes do not activate ordinary schema-v2 writers,
public CAS, profile binding, lifecycle-v2 policies or whole-store readiness.

## Frozen request identity

Uses existing `requestHashVersion=1` length-prefixed SHA256 with operation,
contract version, immutable scope and expected-generation string, followed by
one compact Serde JSON field value. The client key and store lineage select the
receipt domain but are not additional hash fields. Expected generation remains
part of request identity: an exact retry repeats its original precondition.

- Create: operation `namespace_create_v1`, Catalog scope, JSON tuple
  `[exactName,embeddingDim,policy]`. Namespace ID and `created_at` are excluded:
  the ID is allocated transactionally and the timestamp is supplied by the
  existing server caller. Replay retains the original ID and creation time.
- Update: operation `namespace_policy_update_v1`, Namespace(ID) scope, policy
  JSON object only. No update timestamp is sampled.
- Policy field order: `initialStability`, `defaultDifficulty`, `phaseThresholds`
  (ordered `fullToSummary`, `summaryToGhost`, `ghostToDelete`),
  `permastoreThreshold`, `desiredRetention`, `decayRateMultiplier`.
  Finite f32 values use Serde's shortest round-trippable encoding. Signed zero
  remains distinct; no trimming or case folding changes hash input. The exact
  create spelling is retained, with existing ASCII-insensitive name uniqueness.
  `null` multiplier means inheritance and differs from any explicit value.

Lineage is checked before replay; matching replay precedes CAS, namespace
existence/uniqueness checks, ID allocation and filesystem side effects. Replaying
a create after deletion returns its historical ID, not a recreated namespace.
Changed inputs with the same key conflict. IDs from committed rows never reuse.

## Transaction and file publication

Name/dimension/policy/schema validation occurs at storage entry. Policy updates
have no identity, directory, dimension, timestamp, lifecycle or profile setters.
Legacy desired retention accepts finite values in [0,1], including both endpoints,
matching tools16 HTTP/MCP validation. Zero decay multiplier remains valid. This
compatibility rule does not change schema-v2 validation or enable its writers.
New ordinary rows require legacy `schema: None`; profiles and v2 policy toggles
remain migration work. The existing fenced new-store layout is retained.

Create holds metadata admission and one write transaction while it checks
replay/CAS/capacity/uniqueness, allocates the next ID, and prepares row+receipt.
Before commit, the engine creates a private temporary vector directory, syncs
its file and directory, publishes it with atomic no-replace rename, then syncs
the parent. Linux uses renameat2(RENAME_NOREPLACE); macOS uses
renameatx_np(RENAME_EXCL), from the existing libc dependency. Unsupported
platforms fail closed. No existing directory, file or symlink is adopted.
The row, counter, receipt and key commit together and advance generation once.
The prepared vector handle is then installed in memory without reopening it.

Only the owned temporary directory is automatically removed on ordinary
pre-publication failure. Once published, no compensating delete is attempted.
An error after payload publication makes the handle unavailable; reopen
quarantines an unreferenced namespace directory using existing orphan recovery,
or opens the committed row and file. An uncertain metadata commit returns its
operation ID and makes the handle unavailable; original files remain for
reconciliation. Process exits can leave hidden owned staging directories, which
are retained rather than broadly deleted. This is a bounded leak, not adoption
of unknown data. Policy updates commit row+receipt together with the same
uncertain-commit behavior and no filesystem publication.

`MetadataStore::create_namespace` remains the explicit metadata-only API used by
metadata fixtures. It shares validation/receipt allocation but cannot promise
payload creation. Production engine and v1 adapters use the prepared-file path.
No claim that all remaining direct record/catalog/purge writers participate.

## Receipt compatibility and validation

New payload-free outcomes are `NamespaceCreated {namespaceId,createdAt}` and
`NamespacePolicyUpdated {namespaceId}`. receiptVersion stays1; old decoders
reject unknown mandatory outcomes, and the inherited selected-text fence still
rejects old readers/writers. Validation rejects zero IDs and invalid creation
times. Startup also requires creation receipts under Catalog scope and policy
update receipts under Namespace scope with the same namespace ID as the outcome.
This is a relationship between retained evidence, not a namespace-existence
check: valid receipts remain readable after purge. Future joins must preserve all
graph/text/schema/access/feedback outcome variants rather than replace the match
table with one branch's variants.

Validation results and fault coverage are supplied with the final handoff.

## Executed validation

Image recalld-ci:1.94/toolchain1.94.1, network none,2CPUs,4GiB,2build jobs,
private sequential cargo/target; synthetic stores only, no home/credentials or
socket mounts. Final serial suite passed627library+1text-migration CLI test,
4doctests ignored; formatting and all-target/all-feature Clippy passed with
warnings. Eight added tests include one subprocess entry point. They cover
keyed replay before stale CAS/filesystem preparation, input conflicts, replay
after purge/name reuse, monotone IDs, policy atomicity/identity preservation,
nine injected I/O/commit/publication boundaries, seven actual process exits,
existing empty/nonempty directory preservation, invalid values/lineage,
generation/ID exhaustion, a fixed request-hash digest and strict receipt outcomes.
The legacy empty-name safety regression now plants its row through a raw
synthetic transaction, because production metadata create correctly rejects it.

One normal-parallel full run failed an existing graph-crash test with
`DatabaseLocked` (626passed/1failed), after that same test had passed in the
preceding full run. This error can originate only from stable-owner or legacy
directory flock here, not redb; the original run did not identify which flock.
Three isolated repeats and the serial full suite passed. One bounded ordinary-
parallel full recheck with scratch-only test instrumentation passed627+1CLI.
It logged the exact parent-drop, child-reaped and both reopen/drop lifetimes;
no busy lock occurred in that graph test, so the original origin and proposed
parallel-spawn inherited-descriptor mechanism remain unconfirmed. There is no
production lock relaxation, retry or test-harness serialization change.
Failure logs and diagnostic patch are preserved, not treated as resolved by
passing reruns. macOS syscall declarations were inspected in the pinned libc
source; runtime validation here was Linux only.

Raw logs and scratch diagnostic patch:
`/tmp/recalld-implementation/catalog-validation`. The diagnostic copy is
`/tmp/recalld-implementation/catalog-lock-diagnostics/source`, separate from
this implementation branch. No exhaustive crash/power-loss or readiness claim.

## Scope validation regression (child of 756b22e)

Before the fix, the new startup regression accepted all six forged cases:
creation under Namespace scope, policy update under Catalog scope, and policy
update under another namespace ID, each with and without a retry key. Keyed
fixtures move both receipt and retry-key reference consistently, isolating the
missing outcome/scope check from the existing key-link validation.

The fixed regression requires the specific scope rejection and compares all
store files byte-for-byte after failed startup. A second regression reopens valid
creation/update receipts after namespace purge, with and without retry keys.
The catalog filter passes 11 tests. Older outcome validation is unchanged.
The final normal-parallel `cargo test --offline --locked --all-features` passes
629 library tests and 1 CLI test, with 4 doctests ignored. Formatting check and
all-feature/all-target Clippy pass with warnings. Runs use the same isolated
offline Rust 1.94.1 Docker configuration above. This passing run does not resolve
the earlier catalog-parent flock failure or establish its cause.
Raw before/focused/full/fmt/Clippy logs are retained outside the source tree at
`/tmp/recalld-implementation/catalog-receipt-scope-validation`.

## Legacy policy compatibility correction

Minimal validation join107407a contains only catalog scope82e840b and tools16
cad1e99. Its merge keeps strict duplicate-scan validation/coverage inside the
existing effects-stamp guard and retains both branches' test modules.

Before correction, 54 namespace tests passed and three failed: direct engine
creation rejected retention0 with `InvalidMutation`, HTTP/MCP option parity
returned409 for the distinct name http-1, and an actual filesystem failure
returned409 with a false duplicate-name message. Legacy storage now accepts
finite retention0 and1 for creation and policy updates, with reopen persistence
tests and zero decay multiplier. Nonfinite/out-of-range retention and ordinary
schema-v2 creation remain rejected without allocation.

HTTP creation maps errors by type: only DuplicateName produces the namespace
already-exists conflict; invalid policy/name produces400, recovery/pending or
unknown commit produces503 (preserving operation ID), unexpected I/O produces500.
The real filesystem-error regression restores its synthetic root before checking
response status and confirms no namespace or generation was committed.

Fixed validation:58 focused namespace tests pass; the final normal-parallel
all-feature suite passes646 library tests and1 CLI test, with4 doctests ignored.
Formatting and all-feature/all-target Clippy pass with warnings. The same offline
Rust1.94.1 Docker limits and private sequential caches apply. This does not resolve
the separately retained earlier flock failure or certify whole-store readiness.

Raw before/focused/full/fmt/Clippy logs:
`/tmp/recalld-implementation/catalog-policy-compatibility-validation`.

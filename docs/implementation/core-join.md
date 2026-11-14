# Minimal storage prerequisite join

This branch joins PR39 `cdee36e` (online graph-effect completion), PR38
`214d719` (removal ledger, including atomic lifecycle and its clock parent),
and PR40 `4391118` (offline schema/snapshot and exact publication generation).
It does not require the temporary integration checkout's unrelated MCP
structured content, embedding cache or evaluation branches. Profile CLI41 and
retrieval/scope consumer joins remain separate. No ordinary schema-v2 writer
or public whole-store readiness is enabled.

## Source glue retained

- Preserve both injected clock and derived completion handles in constructors.
  Atomic lifecycle retains its final expected-record, namespace-policy and
  sampled graph projection guards; Ghost deletion plans on a private graph and
  commits through durable graph effects before publishing.
- Record hard-delete ledger entries in the same metadata transaction and with
  the same operation ID/store ID/generation as the graph-effect receipt.
- Select text using the constructor-only recovery accessor, scan canonical
  records/namespaces before replay, then recover effects before startup cleanup.
- Combine disposable control/ledger/effect validation with the ordinary-v2
  format gate. Retain graph, text-selection and schema receipt outcomes.
  Combine removal-ledger changes with fallible record encoding.
- Keep sibling legacy test records explicitly `schema: None`, use permitted
  structural edges for retained Ghost fixtures, and count reported text
  compaction separately from a logical lifecycle mutation.
- Include the two actual combined migration fixtures from `4e73f81`, which
  exercise ledger, receipts, canonical effects and pending-canonical refusal.

## Pending-plan preflight fix

The frozen integration review found that a malformed pending peer-count plan
was rejected only after opening original redb files for writes, changing
metadata bookkeeping bytes before refusal. The pure `validate_plan` function
now lives in `storage::metadata::effects`. Disposable preflight invokes it
against decoded records for `!canonical_applied` effects before any writable
canonical open. Replay retains the same check. Already-applied historical
plans are not compared against later canonical counts; those counts can change
before derived acknowledgement.

Durable regressions include the exact original failing metadata-byte probe,
real valid pending recovery, two completed effects with historical peer counts,
and same-receipt ledger preservation through migration/snapshot. The original
failure and frozen source review are retained in research evidence commit
`c0d9717`, under `evidence/integration-join-review`.

## Validation and remaining gates

Offline `recalld-ci:1.94`, Rust1.94.1, network none,2CPUs,4GiB,2build jobs,
private sequential Cargo/target, synthetic stores only. All-feature suite:
619 library tests and1text-migration CLI test passed;4doctests ignored.
Formatting passed; all-target/all-feature Clippy passed with warnings.
Logs are in `/tmp/recalld-implementation/core-join-validation`.

Actual historical restore proof belongs to the separate restore bridge:
format-only and attested migration→backup→fork→inspect positives, missing
historical receipt/publication mismatches and invalid ancestry negatives, and
canonical-applied/derived-pending outbox rows whose receipts retain original
lineage keys. This join does not implement that bridge or claim those tests.

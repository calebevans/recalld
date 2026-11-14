# Canonical graph effects: bounded storage slice

Parent: `impl/mutation-admission` at `bc01b2e`. This is a sibling of text
compaction and atomic lifecycle, not an implementation of the full A1 contract.

## Commit and recovery

The metadata publication guard spans the edge snapshot, metadata transaction,
edge materialization and completion mark. One durable metadata transaction
contains record removals, absolute surviving peer counts, a generation/receipt,
exact edge puts/deletes, vector releases and content-free derived invalidations.
`effects_schema` and `mutation_effects` are additive version-1 tables. There is
no new public mutation/CAS API; graph receipts are currently unkeyed evidence,
not a transport retry contract.

Engine edge writes, hard deletion (including bridge puts), and each namespace
purge chunk use this boundary. New puts recheck namespace existence, endpoint
identity/scope and phase under the final guard. Associative/entity/temporal and
other nonstructural links reject Ghost/Tombstone endpoints. V1 structural
compatibility permits a retained old Supersedes target and a retained parent;
the new replacement/child must be live. Structural bridges can retain endpoints.
New Supersedes cycles and ambiguous successor branches are rejected atomically.
Legacy foreign/invalid incident edges may be deleted as integrity cleanup.

After metadata commit, forward/reverse edge indexes are updated in one redb
transaction. Reapplying puts/deletes is idempotent. Vector releases check that
no current record owns the slot, and sync before the completion mark. Namespace
purge drops the entire namespace vector file after all chunks/config removal;
it does not release individual slots. Purge remains resumable, not one atomic
catalog operation. A failed materialization returns `CommittedEffectsPending`
with the committed operation ID and poisons ordinary admission until reopen.
A metadata commit error remains `CommitOutcomeUnknown` with operation ID.

Startup validates effects schema/receipt identity and replays incomplete
canonical work **before orphan cleanup**. Normal metadata methods refuse a
pending canonical handle; constructor-only `scan_all_for_recovery` and
`list_namespaces_for_recovery` let the unescaped engine validate/open files.
Recovery does not increment generation, create another receipt or decrement
counts again. Backup/restore validation rejects incomplete canonical effects;
applied rows with outstanding derived invalidations may be copied for rebuild.

## Publication and admission

Explicit/automatic link callers acquire graph owned write, then storage write,
then metadata publication/redb. They plan on a private graph copy, commit first,
publish the graph and absolute cache counts, then acknowledge derived work.
No storage lock is held while acquiring/awaiting a graph lock. Copy planning
costs O(V+E); the canonical edge snapshot/count calculation also scans edges.
This is a correctness baseline, not a throughput improvement.

Canonical completion retains durable invalidations until derived publication.
QueryEngine search/get/similar and HTTP get/search/similar check internal effects
admission before and after their complete read. The process-local epoch detects
an intervening effect even if it finished publication before the second check.
Dirty is published before the epoch advances; admission samples the epoch twice.
HTTP pending-work errors are 503, including cache hits, rather than false 404s.
These stamps are **not canonical generations or leases**. Direct raw graph/index
access, other legacy cached/advisory readers, and mutations outside this effects
boundary are not certified as one read view. Transport-wide readiness and every
writer still need the later canonical read-view contract.

Hard-delete/purge callers currently lack a complete derived acknowledgement
path. The lifecycle runner in particular lacks cache/FTS/vector/entity handles.
Their invalidations intentionally leave queries unavailable until a successful
reopen/rebuild. Startup clears all FTS rows before rebuilding when effects are
pending, eliminating deleted rows in an otherwise nonempty index. Incomplete
vector/graph rebuilding or FTS/text errors fail startup; only successful rebuild
of graph, vector, FTS, entity and empty cache clears invalidations. This availability
cost is explicit; it is not background readiness or automatic online repair.

## Integration instructions

- **Lifecycle `845f2f3` / atomic `862ff28`:** retain `apply_transition`'s graph
  owned write -> storage write order, `expected.matches` and namespace-policy
  comparison. Inside those final guards, clone/plan bridging without modifying
  the live graph, then call `delete_memory_with_edges(id, &bridge_puts)`. Publish
  the planned graph only for a successful `Some(EffectResult)`. Remove the split
  `delete_memory`/`batch_add_edges` sequence. Do not discard lifecycle's checks
  when resolving this branch's older `transition_ghost_to_deleted` helper.
  Leave derived work pending until all required indexes/cache publish.
- **Text generation:** preserve selected-header/seal/text-ref validation before
  engine construction. Its constructor scan must use `scan_all_for_recovery`;
  namespace opening uses `list_namespaces_for_recovery`. Then replay effects,
  then run startup validation/orphan cleanup. Keep both text-catalog and effects
  validators in strict backup inventory/migration preflight; do not remove the
  pending-work gate to make startup pass.
- **Scoped calls `7a9ac89`:** preserve optional namespace assertions inside the
  final storage write guard before protected mutations. Forward namespace and
  record-access arguments through the new HTTP/query admission wrappers; never
  move assertion solely to an earlier cache/graph precheck. New canonical
  methods such as `records_in_namespace` also need admission on integration.
- **Removal ledger:** once its real dependency lands, call
  `removal_ledger::record_removal` inside `commit_graph_effects`, immediately
  after receipt creation and before outbox insert/commit, for each removed ID.
  Supply `GenerationRef { store_id: receipt.store_id, generation:
  receipt.committed_generation }`, the removed record's namespace, `HardDelete`,
  `ALL`, and `receipt.operation_id`. No extra generation/operation ID or second
  transaction. Current outbox IDs are temporary recovery data, not the retained
  removal ledger; receipt counts alone cannot prevent UUID resurrection.

## Validation and remaining boundary

Validated source: `dd5d2199bddb8370411867153529142a123e7046`, parent `bc01b2e`,
executed 2026-11-14. This final result update is documentation-only.

| Check | Result |
| --- | --- |
| `cargo test --offline --locked --all-features -- --test-threads=2` | 543 passed; 4 existing doctests ignored |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --offline --locked --all-targets --all-features` | Passed with warnings; not warning-free |

Runs use `recalld-ci:1.94`, Rust 1.94.1, `--network none --cpus 2 --memory 4g`,
`CARGO_BUILD_JOBS=2`, private Cargo/target scratch mounts, and only synthetic
stores. No home/credentials/host socket/personal data mounts. Logs/caches remain
untracked under `.validation/`.

Subprocess exits bypass destructors at eight real boundaries: before/after
metadata commit, before/after edge commit, after vector sync, after canonical
acknowledgement, before graph publication, and before derived acknowledgement.
Repeated reopen checks exact edges/counts, slot ownership, unchanged receipt ID
and one generation advance; precommit exit leaves old state. Other regressions
cover committed-but-unavailable errors, malformed/partial/future schemas,
transaction-wide prevalidation, stale tombstone candidates, structural rules,
graph/storage lock order, an effect completed during an in-flight miss, cached
queries/HTTP 503, and removal of stale FTS rows on real system reopen. Process
interruption is not arbitrary I/O failure or hardware power-loss proof.

Next actual dependent slice: integrate removal ledger and atomic lifecycle
transactions, then route remaining insert/content/tombstone/catalog writers
through receipt plus invalidation transactions. Add online derived completion
and typed committed/unavailable outcomes across actual transports. Only after
all writers/readers participate should canonical snapshots, public whole-store
CAS, readiness barriers, batch manifests and retry contracts be exposed.
Restore lineage/ledger reconciliation, durable ownership epochs, unified
canonical payload transactions and backend migration remain separate A1 work.

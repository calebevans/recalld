# Canonical insertion and forget batches

Validated runtime `b7c1ddc261b1f00b7f28fa0147b8212b7d2596f9` on `impl/canonical-batches`, parent
`c3ac21bd5a0b6e2495c312f44088add72afbc2ed` (purge runtime `f6e4d92`,
published tombstone 60 and catalog 50 beneath it). No native/schema-v2 writer,
public whole-store CAS, accepted-work barrier or deterministic A6 reset claim.
Final validation: 34 focused, 717 library and 1 CLI tests passed; zero failed, four doctests ignored. Format passed and clippy completed (448 library / 467 test warnings including duplicates). [Evidence and hashes](../evidence/canonical-batches/README.md).

The actual MCP batch tools and new daemon `mutate_batch` method send the full
ordered request through one typed bridge call. The existing daemon
`delete_memories` method still returns `Vec<bool>`; missing/already-forgotten
records explicitly project to false. A non-projectable partial result remains a
partial error carrying the full batch outcome. Single-write methods retain their
existing interfaces. Old bridge implementations do not manufacture receipts and
cannot accept keyed/versioned batches; actual direct/remote adapters use the
durable method and never fall back after an error.

## Identity and transaction boundary

`batch_schema` (`&str -> u32`, version 1) and `mutation_batches`
(`&[u8] -> &[u8]`, parent operation UUID -> versioned manifest) are lazy canonical
tables in the existing metadata database. No second receipt journal is added.
The immutable catalog reservation receipt binds the batch kind, ordered intent
hashes and resolved manifest hash. Each child receives a private reserved UUID.
Final record validation, payload staging, record/ledger/effects/receipt and the
successful child outcome use the existing canonical transaction. A later final
validation error has its own catalog bookkeeping receipt. Parent and bookkeeping
writes advance generation even when zero children succeed; the response lists
them separately and has no aggregate committed generation.

The caller hash precedes mutable policy resolution. Fixed defaults such as empty
tag arrays and `checkDuplicates=true` expand first. Omitted item namespace hashes
the original effective session/default namespace, while explicit namespace hashes
that caller override. Mutable policy defaults, policy hash and immutable
namespace ID freeze in the manifest. Pending children recheck the frozen policy
under the final canonical guard. A removed name cannot bind to a new incarnation.
Original supplied vector and stability bits remain distinct, including scaled
vectors and signed zero; working vector normalization does not replace caller
input. Generated UUIDs/time/provider output are excluded from caller identity.
Invalid input is retained only as a hash and bounded code/field, never payload.

The single-memory MCP parser/schema is unchanged. Only the dedicated batch
boundary parses optional `embedding` and `initialStability`, with schema entries
on the batch tool. Non-semantic unknown item fields retain legacy execution
tolerance but their canonical JSON participates in retry identity; changing or
adding one conflicts. Unsupported canonical identity/version fields (`id`,
`createdAt`, scope/receipt IDs, expected generation and contract version) are
indexed input failures, not silently discarded semantic inputs. JSON null and
omission share the existing optional-field meaning; explicit numeric zero is not
an absent default.

Matching full keyed resubmission returns committed and finalized error outcomes
before current policy/CAS checks. The public batch envelope does not expose
`expectedGeneration`; internal tests exercise reservation replay before CAS.
An originally unkeyed call can resume using its returned `batchOperationId` plus
`storeId` and the identical complete payload. This is mutually exclusive with an
idempotency key and can only load an existing reservation. Missing/foreign IDs,
child IDs used as parent IDs, and changed payloads fail without a new reservation.
An unkeyed request whose entire response was lost cannot promise recovery without
the parent ID; callers needing transport retry identity must supply a key.

Every input index has `ok|error|notAttempted|unknown`, known scope and operation ID,
receipt/error, readiness and retryability. A committed current child retains its
own receipt on publication failure; an uncertain child retains its actual
reserved ID. Unknown reservation and unknown final-error bookkeeping are reported
separately. No same-handle readback changes an uncertain commit to confirmed.
Reopen reconciles the manifest before matching resubmission can allocate again.
`total=succeeded+failed+unresolved`; a committed unready item is a success with
`ready=false`. All non-all-ready MCP results set `isError=true`. Compatibility
fields remain alongside the additive `batch` envelope. `batchResultVersion=1`
negotiates this envelope only. The daemon retains its existing 8 MiB negotiated
frame bound; an oversized whole batch fails before sending/reserving and must be
submitted as smaller independent batches, not silently split under one key.

## Canonical-table and consumer audit

| Consumer | Required handling in this slice |
| --- | --- |
| Control/startup disposable preflight | Validate both tables together, version/type, lineage, immutable parent receipt/hash, child identity/scope/hash/kind, final-error receipts and no orphan histories before writable open. |
| Normal startup | Valid unfinished manifests survive; payload-free pending work is not auto-executed. Canonical effects use existing recovery before resubmission. |
| Strict offline inventory/backup | Reject unfinished manifests explicitly. Completed physical metadata copies retain both tables/receipts. |
| Logical export 49 `faf0f81`, restore and schema migration | No allowlist-only adoption. Existing unknown-table rejection remains until an ancestry-aware consumer explicitly preserves complete progress and reconciles origin/target receipts. Feynman owns the independent compatibility follow-up. |
| Accepted-work tracker/freeze | The [batch/accepted-work join](batch-accepted-work.md) carries actual batch descendants and rejects durable pending slots at metadata capture; no whole-core readiness claim. |
| Native insertion/tombstone | Existing ordinary tail receives `None`; reserved legacy children use `Some(BatchChild)`. Preserve Pascal's native proof/token arms at the later join. |

The admission guard remains the parent engine/metadata availability gate, not a
leased whole-pipeline read view. The pending manifest itself does not globally
poison serving; publication/uncertain commit failures retain existing admission
rules. Unfinished offline freeze remains blocked until matching caller input or
an explicitly designed terminal cancellation operation, which this slice does
not invent.

A6 owned randomness is a future integration requirement: reservation child UUIDs
are generated in `MetadataStore::reserve_or_resume_batch`, parent/bookkeeping IDs
in `control::record_receipt`, inserted memory IDs in the existing insertion
writer, and store lineage IDs in control initialization. No process-global RNG
override, fixed-ID deterministic claim, or power-loss inference from process
termination tests is made.

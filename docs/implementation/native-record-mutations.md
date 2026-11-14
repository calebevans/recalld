# Explicit native insertion and tombstone

This slice adds native record creation and forget to the existing owned
`RedbStorageEngine`, metadata publication guard and canonical effects transaction.
`NativeLifecycleSession` owns that same engine; it does not acquire another owner
for each request. Ordinary schema-2 serving and legacy writers remain blocked.

`NativeInsert` preserves the original summary, optional text, tags, UTF-8 entity,
topic and emotion lists, optional creation/observation times, supplied ID, parent
and supersession choices. Supplied embeddings require their explicit compatible
profile. Generated embeddings carry the actual provider's profile separately.
Structured labels persist exactly with `NativeV2` provenance; they are not
converted into lossy compatibility tags. Unknown record extensions cannot be
silently discarded by native forget.

`capture_insert` and `capture_tombstone` bind opaque intents to the actual guarded
metadata read, namespace policy and record tokens. `insert_prepared` and
`tombstone` use the existing metadata transaction and `RecordMutation` effect
tail. Final validation checks the current policy, tokens and namespace state.
Creation, affected peers' edge counts/tokens, canonical edge plan, receipt and
generation commit together. Forget strips retained content, structured labels
and vector ownership, records the removal ledger and preserves content-free
structural edges in that same transaction. Canonical file effects use the existing
recovery outbox. Payload staging before a failed metadata commit can leave
unreachable bytes; the handle is unavailable until strict reopen/recovery.

Every fresh operation samples the injected clock once. Replay uses the saved
original default/profile expansion from its receipt before consulting current
namespace defaults or invoking a provider. The mutable-default regression changes
defaults only in an abandoned real transaction; it is not evidence of a supported
namespace-default writer. A separate regression performs an actual lifecycle
policy migration and verifies zero provider calls on replay. Operation IDs and
generated memory IDs remain nondeterministic. Legacy and native insertion share
the owned `MetadataStore::allocate_memory_id` allocation point; no A6 RNG/reset
claim follows from the injected clock.

Receipts contain numeric state, identifiers and component hashes, never source
strings. Reverse history validation carries these commitments instead of
reconstructing forgotten strings. Current retained payload, labels, extensions,
vector references and profile binding are checked against creation evidence;
canonical inventory validation also hashes actual retained text. Legacy-inferred
records do not acquire invented historical text commitments merely because a new
child changes their edge count. Original schema publication state and receipts
remain immutable. Only explicitly typed operations extend the contiguous proof.

Restore preserves native creation receipts as historical evidence, applies later
removal evidence before publication, and forks the store identity. Old-origin
requests cannot replay as new-origin writes. Logical export/import retains the
same immutable schema state and native component proof. Terminal UUID reuse is
rejected. Explicit supplied-ID insertion currently scans historical creation
receipts to reject reuse after a fork; exposure/feedback keep the bounded owned
hot path from `b61cb665`.

Work admission uses the real `MutationGuard` and owner-instance `WorkTicket`.
Async generated insertion accepts a root before awaiting the provider; final
canonical work consumes a descendant of that root. Foreign tickets, including
another tracker with the same store UUID, fail. Admission closure prevents new
roots. Uncertain completion leaves unresolved work and requires recovery.

The minimal core joins native lifecycle/hotfix (`744ef3c` / `b61cb665`), real
insertion+tombstone (`dad8465`), profile admission (`ed242ec`), accepted work
(`20192bf`) and its completed-purge offline consumer (`d03116c`). The latter is
required by the actual tracker ancestry and strict inventory; adding table names
without the consumer would be insufficient. No maintenance or native purge writer
is introduced. The pending batch join must pass `None` to its actual optional
`BatchChild` tail, without inventing native batch support.

This core does not certify derived readiness, coherent whole-query ReadViews,
public keyed search, ordinary v2 serving, global generation comparability or
deterministic ID generation. PR69 transport consumers require their actual reader
parents and publication integration; a synthetic offline reader fixture alone
does not establish that end-to-end contract.

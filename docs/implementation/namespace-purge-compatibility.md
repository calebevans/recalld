# Namespace purge canonical-table compatibility audit

Implementation parent: published tombstone `dad8465` (runtime `1ae3d5a`),
plus current catalog `d4f756c`, including retention endpoints and error mapping.
The design in `namespace-purge-next.md` is approved; its older parent examples
are superseded by these exact parents. This audit precedes implementation.

Two lazy canonical tables are proposed: `namespace_purge_schema` (version) and
`namespace_purge_progress` (payload-free durable operation state). Do not create
them on ordinary legacy opens. An absent pair with no purge receipts is legacy;
a partial pair, wrong table kind/type, future version, inconsistent receipt,
reservation, cursor, counters, lineage or name lease must fail closed.

| Consumer | Required treatment |
| --- | --- |
| Control/startup disposable preflight | Validate both tables and receipt references before writable source open; valid unfinished operations are recoverable work. Never adopt malformed evidence. |
| Pending canonical effects | Validate purge child outcomes against exact removals and the ordinary pure pending plan. Ledger, child receipt and progress advance in the removal transaction. Applied historical plans remain historical. |
| Strict offline inventory, backup and OfflineSnapshot | Reject unfinished purge. Completed progress remains canonical inventory in whole `meta.db`; validate on inspection copies. |
| Schema migration | Reject unfinished through canonical inventory. Preserve completed progress and receipt bytes in copied metadata; do not reinterpret the legacy deletion operation as native lifecycle behavior. |
| Text migration/selection | Both migration and snapshot copying carry whole metadata; migration validates canonical inventory first. Reject unfinished purge and preserve completed rows. Ordinary compaction must obey mutation admission. |
| Logical export/import (`7892b8e`) | Existing explicit table-kind allowlist rejects both new names. Keep this explicit unsupported boundary until an actual lossless consumer supports them. Never omit them from an apparently successful export. |
| Lineage-changing restore (`e93617d` capture/reconcile) | Existing unknown-canonical-table rejection applies even to completed history. Future support must retain origin IDs and historical receipts, validate ancestry, and never re-key or silently rewrite progress under a new lineage. |

No export/restore parent is needed merely to preserve its existing rejection.
Pascal confirmed the restore capture boundary; native writer integration must
use the final-transaction active-namespace check after replay, before resolving
current namespace policy. Feynman is coordinating the maintenance writer join.
The shared guard is not yet implemented or handed off in this audit.

Recovery must run bounded canonical chunks automatically before normal startup
admission and through the actual online completion coordinator. Retain the
namespace name lease until owned directory cleanup is durable; never unlink a
new incarnation using a historical name. A start receipt remains immutable;
versioned progress and separate child/final receipts carry completion. No
public whole-store CAS, query read-view, freeze barrier or native-v2 readiness
claim follows from these tables. Raw writer participation remains a later audit.

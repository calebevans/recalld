//! Payload-free removal evidence. Restore ancestry/reconciliation is a separate consumer.
use redb::{
    MultimapTableHandle, ReadTransaction, ReadableTable, ReadableTableMetadata, TableDefinition,
    TableHandle, WriteTransaction,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{META_TABLE, MetadataStore};
use crate::model::{DecayPhase, DiskRecord, MemoryId};
use crate::storage::{
    StorageError,
    control::{self, ControlState, Generation, GenerationRef},
};

pub(crate) const LEDGER: TableDefinition<&[u8], &[u8]> = TableDefinition::new("removal_ledger");
pub(crate) const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("removal_ledger_state");
pub(crate) const FULL_TEXT: u8 = 1;
pub(crate) const SUMMARY: u8 = 2;
pub(crate) const TAGS: u8 = 4;
pub(crate) const EMBEDDING: u8 = 8;
pub(crate) const ALL: u8 = FULL_TEXT | SUMMARY | TAGS | EMBEDDING;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalKind {
    Tombstone,
    HardDelete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RemovalEntry {
    pub origin_lineage: Uuid,
    pub namespace_id: u32,
    pub memory_uuid: Uuid,
    pub terminal_kind: Option<TerminalKind>,
    pub removed_fields_mask: u8,
    pub operation_id: Uuid,
    pub generation: Generation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PriorHistory {
    Unknown,
    CompleteFromCreation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LedgerState {
    pub ledger_version: u32,
    /// A coverage boundary, not an invented historical deletion operation.
    pub tracking_started_at: GenerationRef,
    pub prior_history: PriorHistory,
}

/// Lossless ledger section for A1 export. Does not imply a whole-store snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LedgerExport {
    pub state: LedgerState,
    pub entries: Vec<RemovalEntry>,
}

fn invalid(reason: &str) -> StorageError {
    StorageError::InvalidControl(format!("removal ledger: {reason}"))
}

fn key(origin: Uuid, namespace: u32, id: Uuid) -> [u8; 36] {
    let mut bytes = [0; 36];
    bytes[..16].copy_from_slice(origin.as_bytes());
    bytes[16..20].copy_from_slice(&namespace.to_be_bytes());
    bytes[20..].copy_from_slice(id.as_bytes());
    bytes
}

fn decode_entry(bytes: &[u8]) -> Result<RemovalEntry, StorageError> {
    let entry: RemovalEntry =
        serde_json::from_slice(bytes).map_err(|_| invalid("invalid entry encoding"))?;
    if entry.origin_lineage.is_nil()
        || entry.memory_uuid.is_nil()
        || entry.operation_id.is_nil()
        || entry.namespace_id == 0
        || entry.removed_fields_mask == 0
        || entry.removed_fields_mask & !ALL != 0
        || entry.generation.get() == 0
        || (entry.terminal_kind.is_some() && entry.removed_fields_mask != ALL)
    {
        return Err(invalid("invalid removal identity/mask/generation"));
    }
    Ok(entry)
}

/// Validate without creating/repairing anything, and export every retained entry.
/// None identifies a genuine pre-ledger store; callers must preserve unknown coverage.
pub(crate) fn read_export(
    tx: &ReadTransaction,
    control: Option<&ControlState>,
) -> Result<Option<LedgerExport>, StorageError> {
    let names: Vec<_> = tx.list_tables()?.map(|t| t.name().to_owned()).collect();
    let reserved = [LEDGER.name(), STATE.name()];
    if tx
        .list_multimap_tables()?
        .any(|t| reserved.contains(&t.name()))
    {
        return Err(invalid("reserved table is a multimap"));
    }
    let present = reserved.map(|n| names.iter().any(|s| s == n));
    let marker = control.and_then(|c| c.removal_ledger_version);
    if marker.is_none() && present.iter().all(|p| !p) {
        return Ok(None);
    }
    if marker != Some(1) || !present.iter().all(|p| *p) {
        return Err(invalid("missing, unmarked or unsupported ledger tables"));
    }
    let control = control.ok_or_else(|| invalid("missing control"))?;
    let table = tx.open_table(STATE)?;
    if table.len()? != 1 {
        return Err(invalid("state must have exactly one row"));
    }
    let value = table
        .get("state")?
        .ok_or_else(|| invalid("missing state"))?;
    let state: LedgerState =
        serde_json::from_slice(value.value()).map_err(|_| invalid("invalid state encoding"))?;
    if state.ledger_version != 1
        || state.tracking_started_at.store_id != control.store_id
        || state.tracking_started_at.generation > control.committed_generation
    {
        return Err(invalid("invalid coverage boundary/version"));
    }
    let records = tx.open_table(META_TABLE)?;
    let mut entries = Vec::new();
    for row in tx.open_table(LEDGER)?.iter()? {
        let (k, value) = row?;
        let entry = decode_entry(value.value())?;
        if k.value() != key(entry.origin_lineage, entry.namespace_id, entry.memory_uuid)
            || (entry.origin_lineage == control.store_id
                && entry.generation > control.committed_generation)
        {
            return Err(invalid("key mismatch or future generation"));
        }
        if let Some(value) = records.get(entry.memory_uuid.as_bytes().as_slice())? {
            let record = DiskRecord::from_bytes(value.value())?;
            validate_retained_record(&entry, &record)?;
        }
        entries.push(entry);
    }
    Ok(Some(LedgerExport { state, entries }))
}

fn validate_retained_record(entry: &RemovalEntry, record: &DiskRecord) -> Result<(), StorageError> {
    if entry.terminal_kind == Some(TerminalKind::HardDelete)
        || (entry.terminal_kind == Some(TerminalKind::Tombstone)
            && record.phase != DecayPhase::Tombstone)
        || record.namespace_id != entry.namespace_id
        || (entry.removed_fields_mask & FULL_TEXT != 0
            && (record.text_offset != 0 || record.text_length != 0))
        || (entry.removed_fields_mask & SUMMARY != 0 && !record.summary.is_empty())
        || (entry.removed_fields_mask & TAGS != 0 && !record.tags.is_empty())
    {
        return Err(invalid("record contradicts retained removal evidence"));
    }
    Ok(())
}

/// Caller validated existing state before opening the initialization transaction.
pub(crate) fn initialize(tx: &WriteTransaction, pristine: bool) -> Result<(), StorageError> {
    let mut control = control::read_control(tx)?;
    if control.removal_ledger_version.is_some() {
        return Ok(());
    }
    let state = LedgerState {
        ledger_version: 1,
        tracking_started_at: control.generation_ref(),
        prior_history: if pristine {
            PriorHistory::CompleteFromCreation
        } else {
            PriorHistory::Unknown
        },
    };
    tx.open_table(LEDGER)?;
    tx.open_table(STATE)?
        .insert("state", control::encode(&state)?.as_slice())?;
    control.removal_ledger_version = Some(1);
    tx.open_table(control::CONTROL)?
        .insert("state", control::encode(&control)?.as_slice())?;
    Ok(())
}

/// Called INSIDE the canonical mutation transaction, with its already allocated
/// operation ID and advanced generation. Never commits, allocates a receipt, or
/// advances generation itself. HardDelete dominates Tombstone; masks only grow.
pub(crate) fn record_removal(
    tx: &WriteTransaction,
    at: GenerationRef,
    namespace: u32,
    id: MemoryId,
    terminal: Option<TerminalKind>,
    mask: u8,
    operation_id: Uuid,
) -> Result<(), StorageError> {
    let control = control::read_control(tx)?;
    if control.removal_ledger_version != Some(1) || at != control.generation_ref() {
        return Err(invalid("mutation generation/lineage mismatch"));
    }
    let key = key(at.store_id, namespace, id.into_inner());
    let mut entry = RemovalEntry {
        origin_lineage: at.store_id,
        namespace_id: namespace,
        memory_uuid: id.into_inner(),
        terminal_kind: terminal,
        removed_fields_mask: mask,
        operation_id,
        generation: at.generation,
    };
    // Validate the caller's mask before union; malformed input cannot be hidden by old bits.
    decode_entry(&control::encode(&entry)?)?;
    let mut table = tx.open_table(LEDGER)?;
    if let Some(value) = table.get(key.as_slice())? {
        let old = decode_entry(value.value())?;
        if old.generation > at.generation
            || key != self::key(old.origin_lineage, old.namespace_id, old.memory_uuid)
        {
            return Err(invalid("generation moved backward or key mismatch"));
        }
        entry.terminal_kind = old.terminal_kind.max(terminal);
        entry.removed_fields_mask |= old.removed_fields_mask;
    }
    table.insert(key.as_slice(), control::encode(&entry)?.as_slice())?;
    Ok(())
}

pub(crate) fn ensure_insertable(tx: &WriteTransaction, id: MemoryId) -> Result<(), StorageError> {
    // ponytail: scan lifetime evidence for UUID reuse across namespaces/origins;
    // a derived UUID index can replace this if measured lineage size requires it.
    for row in tx.open_table(LEDGER)?.iter()? {
        let (_, value) = row?;
        let entry = decode_entry(value.value())?;
        if entry.memory_uuid == id.into_inner() && entry.terminal_kind.is_some() {
            return Err(StorageError::DuplicateId(id.into_inner()));
        }
    }
    Ok(())
}

/// Compare actual content changes, not phase labels. V1 summary loss retains tags.
pub(crate) fn removed_fields(before: &DiskRecord, after: &DiskRecord) -> u8 {
    let mut mask = 0;
    if (before.text_offset != 0 || before.text_length != 0)
        && after.text_offset == 0
        && after.text_length == 0
    {
        mask |= FULL_TEXT;
    }
    if !before.summary.is_empty() && after.summary.is_empty() {
        mask |= SUMMARY;
    }
    if !before.tags.is_empty() && after.tags.is_empty() {
        mask |= TAGS;
    }
    mask
}

/// Include logical phase loss even when the legacy field was already absent.
pub(crate) fn lifecycle_removed_fields(before: &DiskRecord, after: &DiskRecord) -> u8 {
    let mut mask = removed_fields(before, after);
    if before.phase == DecayPhase::Full
        && after.phase != DecayPhase::Full
        && after.text_offset == 0
        && after.text_length == 0
    {
        mask |= FULL_TEXT;
    }
    if matches!(before.phase, DecayPhase::Full | DecayPhase::Summary)
        && after.phase == DecayPhase::Ghost
        && after.summary.is_empty()
    {
        mask |= SUMMARY;
    }
    mask
}

/// Shared by actual legacy strip, batch replacement and atomic lifecycle writers.
pub(crate) fn record_change(
    tx: &WriteTransaction,
    at: GenerationRef,
    before: &DiskRecord,
    after: &DiskRecord,
    operation_id: Uuid,
) -> Result<(), StorageError> {
    let mut mask = lifecycle_removed_fields(before, after);
    let terminal = if after.phase == DecayPhase::Tombstone {
        Some(TerminalKind::Tombstone)
    } else {
        None
    };
    if terminal.is_some() {
        mask = ALL;
    }
    if mask != 0 {
        record_removal(
            tx,
            at,
            after.namespace_id,
            MemoryId::from_bytes(after.id),
            terminal,
            mask,
            operation_id,
        )?;
    }
    let table = tx.open_table(LEDGER)?;
    if let Some(value) =
        table.get(key(at.store_id, after.namespace_id, Uuid::from_bytes(after.id)).as_slice())?
    {
        validate_retained_record(&decode_entry(value.value())?, after)?;
    }
    // Only replacement that adds content needs a cross-origin evidence scan.
    // Ordinary strength/feedback/sweep patches use the current-lineage point lookup.
    if (!after.summary.is_empty() && before.summary != after.summary)
        || (!after.tags.is_empty() && before.tags != after.tags)
        || ((after.text_offset != 0 || after.text_length != 0)
            && (before.text_offset, before.text_length) != (after.text_offset, after.text_length))
    {
        for row in table.iter()? {
            let (_, value) = row?;
            let entry = decode_entry(value.value())?;
            if entry.memory_uuid.as_bytes() == &after.id {
                validate_retained_record(&entry, after)?;
            }
        }
    }
    Ok(())
}

impl MetadataStore {
    pub(crate) fn removal_ledger_export(&self) -> Result<LedgerExport, StorageError> {
        self.ensure_available()?;
        let tx = self.db.begin_read()?;
        let control = control::validate_existing(&tx)?;
        read_export(&tx, control.as_ref())?.ok_or_else(|| invalid("missing initialized ledger"))
    }

    pub(crate) fn ensure_uuid_not_terminal(&self, id: MemoryId) -> Result<(), StorageError> {
        if self
            .removal_ledger_export()?
            .entries
            .iter()
            .any(|e| e.memory_uuid == id.into_inner() && e.terminal_kind.is_some())
        {
            return Err(StorageError::DuplicateId(id.into_inner()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NamespaceConfig, NamespaceId, Tag};
    use crate::storage::{RedbStorageEngine, StorageEngine};

    fn record(ns: u32) -> DiskRecord {
        DiskRecord {
            schema: None,
            version: DiskRecord::CURRENT_VERSION,
            id: *MemoryId::new().as_bytes(),
            namespace_id: ns,
            created_at: 0,
            last_accessed_at: 0,
            phase: DecayPhase::Full,
            strength: 1.,
            decay_strength: 1.,
            stability: 1.,
            difficulty: 5.,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: "private-summary-marker".into(),
            tags: vec![Tag::new("private-tag-marker").unwrap()],
            access_history: vec![],
            text_offset: 0,
            text_length: 0,
        }
    }

    fn fixture() -> (
        tempfile::TempDir,
        MetadataStore,
        NamespaceConfig,
        DiskRecord,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.id = store.create_namespace(&ns).unwrap();
        let mut r = record(ns.id.get());
        r.text_offset = 42;
        r.text_length = 10;
        store.insert(MemoryId::from_bytes(r.id), &r).unwrap();
        (dir, store, ns, r)
    }

    #[test]
    fn lifecycle_masks_are_monotone_and_exact_v1_tags_survive() {
        let (dir, store, ns, original) = fixture();
        let id = MemoryId::from_bytes(original.id);
        let initial = store.removal_ledger_export().unwrap();
        assert_eq!(
            initial.state.prior_history,
            PriorHistory::CompleteFromCreation
        );
        assert!(initial.entries.is_empty());
        store
            .apply_legacy_decay(id, &original, &ns, DecayPhase::Summary, 0.6, 0.6)
            .unwrap();
        let entry = store.removal_ledger_export().unwrap().entries.remove(0);
        assert_eq!(entry.removed_fields_mask, FULL_TEXT);
        assert_eq!(
            entry.generation,
            store.control_state().unwrap().committed_generation
        );
        let current = store.get(id).unwrap().unwrap();
        store
            .apply_legacy_decay(id, &current, &ns, DecayPhase::Ghost, 0.1, 0.1)
            .unwrap();
        let ledger = store.removal_ledger_export().unwrap();
        assert_eq!(ledger.entries[0].removed_fields_mask, FULL_TEXT | SUMMARY);
        assert_eq!(ledger.entries[0].terminal_kind, None);
        assert_ne!(ledger.entries[0].operation_id, entry.operation_id);
        assert_eq!(store.get(id).unwrap().unwrap().tags, original.tags);
        let json = serde_json::to_string(&ledger).unwrap();
        assert!(!json.contains("private-summary-marker") && !json.contains("private-tag-marker"));
        assert_eq!(serde_json::from_str::<LedgerExport>(&json).unwrap(), ledger);
        // Attempted stale whole-record replacement cannot put removed text back.
        assert!(store.batch_update_records(&[(id, original)]).is_err());
        assert_eq!(store.removal_ledger_export().unwrap(), ledger);
        drop(store);
        let reopened = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        assert_eq!(reopened.removal_ledger_export().unwrap(), ledger);
    }

    #[test]
    fn every_legacy_removal_writer_records_evidence_and_purge_retains_it() {
        let (dir, store, ns, original) = fixture();
        let id = MemoryId::from_bytes(original.id);
        store.strip_full_text(id).unwrap();
        assert_eq!(
            store.removal_ledger_export().unwrap().entries[0].removed_fields_mask,
            FULL_TEXT
        );
        store.strip_summary(id).unwrap();
        assert_eq!(
            store.removal_ledger_export().unwrap().entries[0].removed_fields_mask,
            FULL_TEXT | SUMMARY
        );
        store.tombstone(id).unwrap();
        let first = store.removal_ledger_export().unwrap().entries.remove(0);
        assert_eq!(
            (first.terminal_kind, first.removed_fields_mask),
            (Some(TerminalKind::Tombstone), ALL)
        );
        store.delete(id).unwrap();
        let second = store.removal_ledger_export().unwrap().entries.remove(0);
        assert_eq!(second.terminal_kind, Some(TerminalKind::HardDelete));
        assert!(second.generation > first.generation);
        assert!(store.insert(id, &original).is_err());
        let mut different_scope = original.clone();
        different_scope.namespace_id += 1;
        assert!(store.insert(id, &different_scope).is_err());
        let a = record(ns.id.get());
        let b = record(ns.id.get());
        let aid = MemoryId::from_bytes(a.id);
        let bid = MemoryId::from_bytes(b.id);
        store.insert(aid, &a).unwrap();
        store.insert(bid, &b).unwrap();
        store.batch_tombstone(&[aid, aid, bid]).unwrap();
        assert_eq!(store.removal_ledger_export().unwrap().entries.len(), 3);
        store.purge_namespace_records(ns.id, 1).unwrap();
        store.delete_namespace(ns.id).unwrap();
        let ledger = store.removal_ledger_export().unwrap();
        assert_eq!(ledger.entries.len(), 3);
        assert!(
            ledger
                .entries
                .iter()
                .all(|e| e.terminal_kind == Some(TerminalKind::HardDelete)
                    && e.removed_fields_mask == ALL)
        );
        drop(store);
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        assert_eq!(store.removal_ledger_export().unwrap(), ledger);
        assert!(store.insert(aid, &a).is_err());
    }

    #[test]
    fn helper_union_hard_delete_dominance_and_abandoned_transaction() {
        let (_dir, store, ns, original) = fixture();
        let id = MemoryId::from_bytes(original.id);
        let before = store.removal_ledger_export().unwrap();
        let state_before = store.control_state().unwrap();
        let guard = store.mutation_guard().unwrap();
        let tx = store.db.begin_write().unwrap();
        let state = control::advance_generation(&tx).unwrap();
        let op = Uuid::new_v4();
        // Model Curie's canonical delete hook: record removal precedes outbox
        // publication, all within this real transaction, deliberately abandoned.
        tx.open_table(META_TABLE)
            .unwrap()
            .remove(id.as_bytes().as_slice())
            .unwrap();
        record_removal(
            &tx,
            state.generation_ref(),
            ns.id.get(),
            id,
            Some(TerminalKind::HardDelete),
            ALL,
            op,
        )
        .unwrap();
        record_removal(
            &tx,
            state.generation_ref(),
            ns.id.get(),
            id,
            Some(TerminalKind::Tombstone),
            ALL,
            op,
        )
        .unwrap();
        let table = tx.open_table(LEDGER).unwrap();
        let value = table
            .get(key(state.store_id, ns.id.get(), id.into_inner()).as_slice())
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_entry(value.value()).unwrap().terminal_kind,
            Some(TerminalKind::HardDelete)
        );
        drop(value);
        drop(table);
        drop(tx);
        drop(guard);
        assert_eq!(store.control_state().unwrap(), state_before);
        assert_eq!(store.removal_ledger_export().unwrap(), before);
        assert_eq!(
            store.get(id).unwrap().unwrap().to_bytes(),
            original.to_bytes()
        );
    }

    #[test]
    fn real_generation_failure_aborts_each_removal_with_no_ledger_or_record_loss() {
        for kind in 0..6 {
            let (dir, store, ns, original) = fixture();
            let id = MemoryId::from_bytes(original.id);
            let before = store.removal_ledger_export().unwrap();
            let mut state = store.control_state().unwrap();
            state.committed_generation = Generation::MAX;
            let tx = store.db.begin_write().unwrap();
            tx.open_table(control::CONTROL)
                .unwrap()
                .insert("state", control::encode(&state).unwrap().as_slice())
                .unwrap();
            tx.commit().unwrap();
            let failed = match kind {
                0 => store.strip_full_text(id).is_err(),
                1 => store.strip_summary(id).is_err(),
                2 => store.tombstone(id).is_err(),
                3 => store.delete(id).is_err(),
                4 => store.batch_tombstone(&[id]).is_err(),
                _ => store.purge_namespace_records(ns.id, 1).is_err(),
            };
            assert!(failed);
            assert_eq!(
                store.get(id).unwrap().unwrap().to_bytes(),
                original.to_bytes()
            );
            assert_eq!(store.removal_ledger_export().unwrap(), before);
            drop(store);
            let reopened = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
            assert_eq!(reopened.removal_ledger_export().unwrap(), before);
            assert_eq!(
                reopened.get(id).unwrap().unwrap().to_bytes(),
                original.to_bytes()
            );
        }
    }

    #[test]
    fn legacy_adoption_reports_unknown_prior_history_without_invented_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.db");
        {
            let db = redb::Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            // A pre-ledger empty store may have deleted every historic record.
            tx.open_table(META_TABLE).unwrap();
            tx.commit().unwrap();
        }
        let store = MetadataStore::open(&path).unwrap();
        let ledger = store.removal_ledger_export().unwrap();
        assert_eq!(ledger.state.prior_history, PriorHistory::Unknown);
        assert!(ledger.entries.is_empty());
        drop(store);
        assert_eq!(
            MetadataStore::open(&path)
                .unwrap()
                .removal_ledger_export()
                .unwrap(),
            ledger
        );
    }

    #[test]
    fn corrupt_or_missing_ledger_never_reinitializes_as_legacy() {
        for corruption in [
            "ledger",
            "state",
            "both",
            "marker",
            "version",
            "mask",
            "future",
            "key",
            "payload",
            "operation",
        ] {
            let (dir, store, _ns, original) = fixture();
            store.tombstone(MemoryId::from_bytes(original.id)).unwrap();
            let export = store.removal_ledger_export().unwrap();
            let tx = store.db.begin_write().unwrap();
            match corruption {
                "ledger" => {
                    tx.delete_table(LEDGER).unwrap();
                }
                "state" => {
                    tx.delete_table(STATE).unwrap();
                }
                "both" => {
                    tx.delete_table(LEDGER).unwrap();
                    tx.delete_table(STATE).unwrap();
                }
                "marker" | "version" => {
                    let mut state = control::read_control(&tx).unwrap();
                    state.removal_ledger_version = if corruption == "marker" {
                        None
                    } else {
                        Some(99)
                    };
                    tx.open_table(control::CONTROL)
                        .unwrap()
                        .insert("state", control::encode(&state).unwrap().as_slice())
                        .unwrap();
                }
                _ => {
                    let entry = &export.entries[0];
                    let k = key(entry.origin_lineage, entry.namespace_id, entry.memory_uuid);
                    let mut value = serde_json::to_value(entry).unwrap();
                    match corruption {
                        "mask" => value["removedFieldsMask"] = 128.into(),
                        "future" => value["generation"] = i64::MAX.to_string().into(),
                        "key" => value["namespaceId"] = (entry.namespace_id + 1).into(),
                        "payload" => value["summary"] = "forbidden".into(),
                        "operation" => value["operationId"] = Uuid::nil().to_string().into(),
                        _ => unreachable!(),
                    }
                    tx.open_table(LEDGER)
                        .unwrap()
                        .insert(k.as_slice(), serde_json::to_vec(&value).unwrap().as_slice())
                        .unwrap();
                }
            }
            tx.commit().unwrap();
            assert!(store.removal_ledger_export().is_err(), "{corruption}");
            drop(store);
            assert!(
                MetadataStore::open(&dir.path().join("meta.db")).is_err(),
                "{corruption}"
            );
        }
    }

    #[tokio::test]
    async fn real_backup_preserves_ledger_and_lossless_export_after_terminal_reuse_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let mut engine = RedbStorageEngine::open(&source).unwrap();
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.embedding_dim = 4;
        ns.id = engine.create_namespace(&ns).unwrap();
        let mut r = record(ns.id.get());
        let id = MemoryId::from_bytes(r.id);
        engine
            .insert_memory(
                id,
                ns.id,
                &mut r,
                &[1., 0., 0., 0.],
                Some("private-full-text-marker"),
            )
            .unwrap();
        engine.delete_memory(id).unwrap();
        let size = std::fs::metadata(source.join("fulltext.dat"))
            .unwrap()
            .len();
        let vector_size = std::fs::metadata(source.join("default/vectors.dat"))
            .unwrap()
            .len();
        assert!(
            engine
                .insert_memory(
                    id,
                    ns.id,
                    &mut r,
                    &[1., 0., 0., 0.],
                    Some("must not append")
                )
                .is_err()
        );
        assert_eq!(
            std::fs::metadata(source.join("fulltext.dat"))
                .unwrap()
                .len(),
            size
        );
        assert_eq!(
            std::fs::metadata(source.join("default/vectors.dat"))
                .unwrap()
                .len(),
            vector_size
        );
        engine.sync().unwrap();
        drop(engine);
        let store = MetadataStore::open(&source.join("meta.db")).unwrap();
        let expected = store.removal_ledger_export().unwrap();
        drop(store);
        let path = crate::backup::run_backup(
            &crate::config::RecalldConfig::default(),
            &dir.path().join("archive.zip"),
            Some(&source),
            true,
        )
        .await
        .unwrap();
        let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        let copy = dir.path().join("snapshot-meta.db");
        std::io::copy(
            &mut archive.by_name("meta.db").unwrap(),
            &mut std::fs::File::create(&copy).unwrap(),
        )
        .unwrap();
        let snapshot = MetadataStore::open(&copy).unwrap();
        assert_eq!(snapshot.removal_ledger_export().unwrap(), expected);
        let encoded = serde_json::to_vec(&snapshot.removal_ledger_export().unwrap()).unwrap();
        assert_eq!(
            serde_json::from_slice::<LedgerExport>(&encoded).unwrap(),
            expected
        );
        // This inspects the captured metadata; it is not a retained-state restore test.
        assert_eq!(
            expected.entries[0].terminal_kind,
            Some(TerminalKind::HardDelete)
        );
        assert_eq!(expected.entries[0].namespace_id, NamespaceId::new(1).get());
    }
}

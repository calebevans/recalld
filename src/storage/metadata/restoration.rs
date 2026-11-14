//! Offline restore reconciliation. Only inaccessible, fresh staging may be mutated.
use super::removal_ledger::{self as ledger, LedgerExport, PriorHistory, TerminalKind};
use super::*;
use crate::storage::{
    TextRef, TextStore,
    control::{Generation, GenerationRef},
    lineage::{self, LedgerSource, LineageState, RestoreMode},
};
use redb::{MultimapTableHandle, TableHandle};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

type Rows = Vec<(Vec<u8>, Vec<u8>)>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CapturedSnapshot {
    pub control: ControlState,
    pub lineage: Option<LineageState>,
    pub ledger: LedgerExport,
    pub namespace_counter: u32,
    pub receipts: Rows,
    pub keys: Rows,
}

/// Read-only inspection copies metadata; caller holds stable ownership/legacy lock.
pub(crate) fn capture(path: &Path) -> Result<CapturedSnapshot, StorageError> {
    crate::storage::validation::validate_existing(path)?;
    let (_copy, db) = crate::storage::validation::inspect_database(&path.join("meta.db"))?;
    let read = db.begin_read()?;
    // Pending effects and record-v2 migration need their owners' restore consumers.
    let known = [
        "memories",
        "namespaces",
        "namespace_counter",
        "indexes",
        "canonical_control",
        "mutation_receipts",
        "mutation_keys",
        "text_generations_v1",
        "removal_ledger",
        "removal_ledger_state",
        "restore_lineage_v1",
    ];
    if read.list_tables()?.any(|t| !known.contains(&t.name()))
        || read
            .list_multimap_tables()?
            .any(|t| !["tag_index", "namespace_index"].contains(&t.name()))
    {
        return Err(StorageError::RecoveryRequired(
            "unsupported canonical restore tables; integrate their recovery consumer first".into(),
        ));
    }
    let control = control::validate_existing(&read)?.ok_or_else(|| {
        StorageError::RecoveryRequired(
            "restore requires explicit offline migration of legacy control/ledger".into(),
        )
    })?;
    let selected = text_generation::read_selection(&read)?.ok_or_else(|| StorageError::RecoveryRequired("restore requires a selected, old-reader-fenced layout; migrate legacy data into a fresh destination offline".into()))?;
    if selected.format_version != 2 {
        return Err(StorageError::RecoveryRequired(
            "restore requires completed selected-text migration".into(),
        ));
    }
    crate::storage::text::generation::validate_rejection_gate(path, control.store_id)?;
    let lineage = lineage::read_lineage(&read, &control)?;
    let mut ledger = ledger::read_export(&read, Some(&control))?.ok_or_else(|| {
        StorageError::RecoveryRequired("restore requires a retained removal ledger".into())
    })?;
    // An additive ledger predating this gated restore has no proof that old
    // writers never bypassed it. Preserve known entries without inventing coverage.
    if lineage.is_none() {
        ledger.state.prior_history = PriorHistory::Unknown;
    }
    let namespace_counter = read
        .open_table(NAMESPACE_COUNTER)?
        .get("max_id")?
        .map(|v| v.value())
        .unwrap_or(0);
    let rows = |definition: TableDefinition<&[u8], &[u8]>| -> Result<Rows, StorageError> {
        read.open_table(definition)?
            .iter()?
            .map(|row| {
                let (key, value) = row?;
                Ok((key.value().to_vec(), value.value().to_vec()))
            })
            .collect()
    };
    Ok(CapturedSnapshot {
        control,
        lineage,
        ledger,
        namespace_counter,
        receipts: rows(control::RECEIPTS)?,
        keys: rows(control::KEYS)?,
    })
}

pub(crate) fn compatible(snapshot: &CapturedSnapshot, target: &CapturedSnapshot) -> bool {
    snapshot.control.store_id == target.control.store_id
        || target
            .lineage
            .as_ref()
            .is_some_and(|l| l.permits(snapshot.control.generation_ref()))
}

fn merge_rows(
    tx: &redb::WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    rows: &Rows,
) -> Result<(), StorageError> {
    let mut table = tx.open_table(definition)?;
    for (key, bytes) in rows {
        if table
            .get(key.as_slice())?
            .is_some_and(|v| v.value() != bytes.as_slice())
        {
            return Err(StorageError::InvalidControl(
                "conflicting historical receipt/key".into(),
            ));
        }
        table.insert(key.as_slice(), bytes.as_slice())?;
    }
    Ok(())
}

/// The stage has passed strict archive validation and is never ordinarily served.
/// Caller owns it until final validation, journal persistence and publication.
pub(crate) fn reconcile(
    stage: &Path,
    snapshot: &CapturedSnapshot,
    target: Option<&CapturedSnapshot>,
    mode: RestoreMode,
    operation_id: Uuid,
    new_store_id: Uuid,
    archive_sha256: [u8; 32],
) -> Result<LineageState, StorageError> {
    if target.is_some_and(|t| !compatible(snapshot, t))
        || (mode == RestoreMode::Serving && target.is_none())
    {
        return Err(StorageError::StoreLineageMismatch);
    }
    if target.is_some_and(|t| {
        t.lineage
            .as_ref()
            .is_some_and(|l| l.mode == RestoreMode::Historical)
    }) && mode == RestoreMode::Serving
    {
        return Err(StorageError::RecoveryRequired(
            "historical stores require separate explicit promotion, which is not implemented"
                .into(),
        ));
    }
    let mut ancestors = BTreeMap::new();
    let mut merged = BTreeMap::<[u8; 36], ledger::RemovalEntry>::new();
    let mut sources = Vec::new();
    for source in std::iter::once(snapshot).chain(target) {
        for origin in std::iter::once(source.control.generation_ref()).chain(
            source
                .lineage
                .iter()
                .flat_map(|l| l.ancestors.iter().copied()),
        ) {
            ancestors
                .entry(origin.store_id)
                .and_modify(|g: &mut Generation| *g = (*g).max(origin.generation))
                .or_insert(origin.generation);
        }
        sources.push(LedgerSource {
            source: source.control.generation_ref(),
            coverage: source.ledger.state.clone(),
        });
        for entry in &source.ledger.entries {
            let k = ledger::key(entry.origin_lineage, entry.namespace_id, entry.memory_uuid);
            if let Some(old) = merged.get_mut(&k) {
                if old.generation == entry.generation && old != entry {
                    return Err(StorageError::InvalidControl(
                        "conflicting same-generation removal evidence".into(),
                    ));
                }
                let mask = old.removed_fields_mask | entry.removed_fields_mask;
                let terminal = old.terminal_kind.max(entry.terminal_kind);
                if entry.generation > old.generation {
                    *old = entry.clone();
                }
                old.removed_fields_mask = mask;
                old.terminal_kind = terminal;
            } else {
                merged.insert(k, entry.clone());
            }
        }
    }
    if ancestors.contains_key(&new_store_id) || new_store_id.is_nil() {
        return Err(StorageError::StoreLineageMismatch);
    }
    let lineage = LineageState {
        version: 1,
        active_store_id: new_store_id,
        operation_id,
        mode,
        source_snapshot: snapshot.control.generation_ref(),
        captured_ledger_source: target.map(|t| t.control.generation_ref()),
        archive_sha256,
        ancestors: ancestors
            .into_iter()
            .map(|(store_id, generation)| GenerationRef {
                store_id,
                generation,
            })
            .collect(),
        ledger_sources: sources,
    };
    let db = redb::Database::open(stage.join("meta.db"))?;
    let read = db.begin_read()?;
    let selected = text_generation::read_selection(&read)?.ok_or(StorageError::InvalidTextRef)?;
    let old_text = TextStore::open_generation(stage, &selected, false)?;
    let mut records: Vec<(MemoryId, DiskRecord)> = read
        .open_table(META_TABLE)?
        .iter()?
        .map(|r| {
            let (k, v) = r?;
            let id = Uuid::from_slice(k.value())
                .map_err(|_| StorageError::InvalidControl("bad UUID".into()))?;
            Ok((MemoryId::from_uuid(id), DiskRecord::from_bytes(v.value())?))
        })
        .collect::<Result<_, StorageError>>()?;
    let namespaces: Vec<NamespaceConfig> = read
        .open_table(NAMESPACE_TABLE)?
        .iter()?
        .map(|r| {
            let (_, v) = r?;
            serde_json::from_slice(v.value()).map_err(|e| StorageError::Deserialize(e.to_string()))
        })
        .collect::<Result<_, _>>()?;
    drop(read);
    let mut by_memory = std::collections::HashMap::<Uuid, Vec<&ledger::RemovalEntry>>::new();
    for entry in merged.values() {
        by_memory.entry(entry.memory_uuid).or_default().push(entry);
    }
    let mut removals = std::collections::HashSet::new();
    let mut released = Vec::new();
    let mut desired = Vec::new();
    for (id, mut record) in records.drain(..) {
        let mut mask = 0;
        let mut terminal = None;
        for entry in by_memory.get(&id.into_inner()).into_iter().flatten() {
            if entry.namespace_id != record.namespace_id {
                return Err(StorageError::MutationNamespaceMismatch);
            }
            mask |= entry.removed_fields_mask;
            terminal = terminal.max(entry.terminal_kind);
        }
        if terminal.is_some() {
            removals.insert(id);
            if record.phase != DecayPhase::Tombstone {
                released.push((record.namespace_id, record.vector_slot));
            }
        }
        if terminal == Some(TerminalKind::HardDelete) {
            continue;
        }
        if mask & ledger::FULL_TEXT != 0 {
            record.text_offset = 0;
            record.text_length = 0;
        }
        if mask & ledger::SUMMARY != 0 {
            record.summary.clear();
        }
        if mask & ledger::TAGS != 0 {
            record.tags.clear();
        }
        if terminal == Some(TerminalKind::Tombstone) {
            record.phase = DecayPhase::Tombstone;
            record.strength = 0.;
            record.decay_strength = 0.;
        } else if mask & ledger::EMBEDDING != 0 {
            return Err(StorageError::RecoveryRequired(
                "nonterminal embedding removal requires a canonical schema consumer".into(),
            ));
        } else if mask & (ledger::FULL_TEXT | ledger::SUMMARY)
            == ledger::FULL_TEXT | ledger::SUMMARY
        {
            record.phase = DecayPhase::Ghost;
        } else if mask & ledger::FULL_TEXT != 0 && record.phase == DecayPhase::Full {
            record.phase = DecayPhase::Summary;
        }
        desired.push((id, record));
    }
    // Multi-file work is confined to staging; interruption cannot expose it.
    let edges = crate::storage::EdgeStore::open(&stage.join("edges.db"))?;
    for id in &removals {
        edges.remove_all_edges(*id)?;
    }
    let mut edge_counts = std::collections::HashMap::<MemoryId, usize>::new();
    for edge in edges.load_all_edges()? {
        *edge_counts.entry(edge.source).or_default() += 1;
    }
    for ns in &namespaces {
        let mut vectors = crate::storage::vectors::VectorStore::open(
            &stage.join(&ns.name),
            ns.embedding_dim as usize,
        )?;
        for (_, slot) in released
            .iter()
            .filter(|(namespace, _)| *namespace == ns.id.get())
        {
            vectors.free_slot(*slot)?;
        }
        vectors.sync()?;
    }
    let (mut text, text_id) = TextStore::create_generation(stage, new_store_id)?;
    for (id, record) in &mut desired {
        record.edge_count = u16::try_from(edge_counts.get(id).copied().unwrap_or(0))
            .map_err(|_| StorageError::InvalidControl("edge count overflow".into()))?;
        if record.text_offset != 0 || record.text_length != 0 {
            let retained = old_text.read(TextRef {
                file_offset: record.text_offset,
                length: record.text_length,
            })?;
            let reference = text.append(&retained)?;
            record.text_offset = reference.file_offset;
            record.text_length = reference.length;
        }
    }
    let descriptor = text.seal(new_store_id, text_id, 2)?;
    crate::storage::text::generation::write_rejection_gate(stage, &descriptor)?;
    let tx = db.begin_write()?;
    let mut control = snapshot.control.clone();
    control.store_id = new_store_id;
    control.committed_generation = Generation::default().next()?;
    control.restore_lineage_version = Some(1);
    lineage.validate(&control)?;
    tx.open_table(control::CONTROL)?
        .insert("state", control::encode(&control)?.as_slice())?;
    tx.open_table(lineage::LINEAGE)?
        .insert("state", control::encode(&lineage)?.as_slice())?;
    if let Some(target) = target {
        merge_rows(&tx, control::RECEIPTS, &target.receipts)?;
        merge_rows(&tx, control::KEYS, &target.keys)?;
    }
    tx.delete_table(META_TABLE)?;
    tx.delete_multimap_table(TAG_INDEX)?;
    tx.delete_multimap_table(NAMESPACE_INDEX)?;
    tx.delete_table(INDEX_TABLE)?;
    tx.open_table(INDEX_TABLE)?;
    {
        let mut table = tx.open_table(META_TABLE)?;
        let mut tags = tx.open_multimap_table(TAG_INDEX)?;
        let mut ns = tx.open_multimap_table(NAMESPACE_INDEX)?;
        for (id, record) in &desired {
            table.insert(id.as_bytes().as_slice(), record.encode()?.as_slice())?;
            if record.phase != DecayPhase::Tombstone {
                ns.insert(record.namespace_id, id.as_bytes().as_slice())?;
                for tag in &record.tags {
                    tags.insert(tag.as_str(), id.as_bytes().as_slice())?;
                }
            }
        }
    }
    tx.open_table(NAMESPACE_COUNTER)?.insert(
        "max_id",
        snapshot
            .namespace_counter
            .max(target.map_or(0, |t| t.namespace_counter)),
    )?;
    tx.delete_table(ledger::LEDGER)?;
    {
        let mut table = tx.open_table(ledger::LEDGER)?;
        for (key, entry) in &merged {
            table.insert(key.as_slice(), control::encode(entry)?.as_slice())?;
        }
    }
    let coverage = ledger::LedgerState {
        ledger_version: 1,
        tracking_started_at: GenerationRef {
            store_id: new_store_id,
            generation: Generation::default(),
        },
        prior_history: PriorHistory::Unknown,
    };
    tx.open_table(ledger::STATE)?
        .insert("state", control::encode(&coverage)?.as_slice())?;
    tx.delete_table(text_generation::CATALOG)?;
    {
        let mut table = tx.open_table(text_generation::CATALOG)?;
        table.insert(
            descriptor.id.to_string().as_str(),
            control::encode(&descriptor)?.as_slice(),
        )?;
        table.insert("active", control::encode(&descriptor.id)?.as_slice())?;
    }
    tx.commit()?;
    // Build the persisted derived text index from reconciled content before
    // publication; a partially rebuilt index must never become queryable.
    let fts = crate::search::FtsIndex::new(stage)
        .map_err(|e| StorageError::RecoveryRequired(e.to_string()))?;
    for (id, record) in &desired {
        if record.phase == DecayPhase::Tombstone {
            continue;
        }
        let retained = if record.text_length > 0 {
            Some(text.read(TextRef {
                file_offset: record.text_offset,
                length: record.text_length,
            })?)
        } else {
            None
        };
        let tags: Vec<_> = record.tags.iter().map(ToString::to_string).collect();
        fts.add(
            NamespaceId::new(record.namespace_id),
            *id,
            &record.summary,
            retained.as_deref(),
            &tags,
        )
        .map_err(|e| StorageError::RecoveryRequired(e.to_string()))?;
    }
    drop(fts); // Closing the only SQLite connection checkpoints its private WAL.
    std::fs::File::open(stage.join("fts.db"))?.sync_all()?;
    drop((old_text, text, edges, db));
    // The old generation is an unselected staging copy, never the source archive.
    std::fs::remove_file(stage.join(selected.filename))?;
    for file in crate::storage::validation::canonical_files(stage)? {
        std::fs::File::open(file)?.sync_all()?;
    }
    crate::storage::fsync::fsync_dir(stage)?;
    Ok(lineage)
}

//! Atomic text selection, offsets, generation and receipt. Payloads are synced first.
use super::*;
use crate::storage::control::Generation;
use crate::storage::text::{RefMapping, TextRef};
use redb::{MultimapTableHandle, TableHandle};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_AFTER_TEXT_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) const CATALOG: TableDefinition<&str, &[u8]> =
    TableDefinition::new("text_generations_v1");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct TextGeneration {
    pub id: Uuid,
    pub store_id: Uuid,
    pub filename: String,
    /// 1 is the offset-preserving, recoverable legacy-upgrade intermediate.
    pub format_version: u16,
    pub sealed_length: u64,
    pub sealed_sha256: [u8; 32],
}

pub(crate) fn generation_filename(id: Uuid) -> String {
    format!("fulltext.{id}.dat")
}

pub(crate) fn is_generation_filename(name: &str) -> bool {
    name.strip_prefix("fulltext.")
        .and_then(|s| s.strip_suffix(".dat"))
        .and_then(|s| Uuid::parse_str(s).ok())
        .is_some_and(|id| name == generation_filename(id))
}

impl TextGeneration {
    fn validate(&self, store_id: Uuid) -> Result<(), StorageError> {
        let header = match self.format_version {
            1 => 16,
            2 => 48,
            _ => 0,
        };
        if header == 0
            || self.id.is_nil()
            || self.store_id != store_id
            || self.filename != generation_filename(self.id)
            || self.sealed_length < header
        {
            return Err(StorageError::InvalidControl(
                "invalid text generation descriptor".into(),
            ));
        }
        Ok(())
    }
}

/// Absence means legacy, never 'choose the newest file'. Wrong table types fail.
pub(crate) fn read_selection(
    read: &redb::ReadTransaction,
) -> Result<Option<TextGeneration>, StorageError> {
    let present = read.list_tables()?.any(|t| t.name() == CATALOG.name());
    let wrong_type = read
        .list_multimap_tables()?
        .any(|t| t.name() == CATALOG.name());
    if wrong_type {
        return Err(StorageError::InvalidControl("multimap text catalog".into()));
    }
    if !present {
        return Ok(None);
    }
    let state = control::validate_existing(read)?
        .ok_or_else(|| StorageError::InvalidControl("text catalog without control".into()))?;
    let table = read.open_table(CATALOG)?;
    let active = table
        .get("active")?
        .ok_or_else(|| StorageError::InvalidControl("text catalog without selection".into()))?;
    let active: Uuid = control::decode(active.value())?;
    let mut selected = None;
    for entry in table.iter()? {
        let (key, value) = entry?;
        if key.value() == "active" {
            continue;
        }
        let descriptor: TextGeneration = control::decode(value.value())?;
        descriptor.validate(state.store_id)?;
        if key.value() != descriptor.id.to_string() {
            return Err(StorageError::InvalidControl(
                "text descriptor key mismatch".into(),
            ));
        }
        if descriptor.id == active {
            selected = Some(descriptor);
        }
    }
    selected
        .map(Some)
        .ok_or_else(|| StorageError::InvalidControl("selected text descriptor missing".into()))
}

impl MetadataStore {
    pub(crate) fn text_selection(&self) -> Result<Option<TextGeneration>, StorageError> {
        self.ensure_available()?;
        self.text_selection_for_recovery()
    }

    /// Constructor-only read before canonical effects replay; the engine has not escaped.
    pub(crate) fn text_selection_for_recovery(
        &self,
    ) -> Result<Option<TextGeneration>, StorageError> {
        read_selection(&self.db.begin_read()?)
    }

    /// The caller holds the exclusive engine gate through file and handle publication.
    pub(crate) fn select_text_generation(
        &self,
        expected: Generation,
        descriptor: &TextGeneration,
        refs: &RefMapping,
    ) -> Result<WriteReceipt, StorageError> {
        let _gate = self.mutation_guard()?;
        let tx = self.db.begin_write()?;
        let state = control::read_control(&tx)?;
        descriptor.validate(state.store_id)?;
        let ctx = MutationContext {
            store_id: Some(state.store_id),
            scope: MutationScope::Catalog,
            idempotency_key: None,
            expected_generation: Some(expected),
            contract_version: 1,
        };
        let bytes = control::encode(descriptor)?;
        // File UUID and seal are generated results, not semantic request identity.
        let operation = if descriptor.format_version == 1 {
            "registerLegacyText"
        } else {
            "compactText"
        };
        let hash = control::request_hash(&ctx, operation, &[]);
        control::replay_or_check(&tx, &ctx, hash)?;
        {
            let mut records = tx.open_table(META_TABLE)?;
            // Every retained extent must participate, including non-Full records.
            let retained =
                records
                    .iter()?
                    .try_fold(0usize, |n, row| -> Result<_, StorageError> {
                        let (_, value) = row?;
                        let r = DiskRecord::from_bytes(value.value())?;
                        Ok(n + usize::from(r.text_offset != 0 || r.text_length != 0))
                    })?;
            let mut seen = std::collections::HashSet::new();
            if retained != refs.len() {
                return Err(StorageError::InvalidMutation(
                    "incomplete text relocation".into(),
                ));
            }
            for (id, reference) in refs {
                let header_size = if descriptor.format_version == 1 {
                    16
                } else {
                    48
                };
                if !seen.insert(*id)
                    || !reference.is_some()
                    || reference.file_offset < header_size
                    || reference
                        .file_offset
                        .checked_add(8 + u64::from(reference.length))
                        .is_none_or(|end| end > descriptor.sealed_length)
                {
                    return Err(StorageError::InvalidTextRef);
                }
                let mut record = {
                    let old = records
                        .get(id.as_bytes().as_slice())?
                        .ok_or(StorageError::NotFound(**id))?;
                    DiskRecord::from_bytes(old.value())?
                };
                if !(TextRef {
                    file_offset: record.text_offset,
                    length: record.text_length,
                })
                .is_some()
                {
                    return Err(StorageError::InvalidTextRef);
                }
                if record.text_length != reference.length {
                    return Err(StorageError::InvalidTextRef);
                }
                record.text_offset = reference.file_offset;
                record.text_length = reference.length;
                records.insert(id.as_bytes().as_slice(), record.encode()?.as_slice())?;
            }
        }
        {
            let mut catalog = tx.open_table(CATALOG)?;
            let key = descriptor.id.to_string();
            if catalog.get(key.as_str())?.is_some() {
                return Err(StorageError::InvalidMutation(
                    "text generation reused".into(),
                ));
            }
            catalog.insert(key.as_str(), bytes.as_slice())?;
            catalog.insert("active", control::encode(&descriptor.id)?.as_slice())?;
        }
        let receipt = control::record_receipt(
            &tx,
            &ctx,
            hash,
            MutationOutcome::TextGenerationSelected {
                generation_id: descriptor.id,
                format_version: descriptor.format_version,
                entries_kept: refs.len() as u64,
            },
        )?;
        let committed = tx.commit();
        // Model an I/O error whose commit actually became durable. Tests must
        // reconcile the receipt after reopen, not reclaim its selected payload.
        #[cfg(test)]
        let committed = committed.and_then(|()| {
            if FAIL_AFTER_TEXT_COMMIT.with(|flag| flag.replace(false)) {
                Err(redb::CommitError::Storage(redb::StorageError::Io(
                    std::io::Error::other("injected unknown text commit"),
                )))
            } else {
                Ok(())
            }
        });
        committed.map_err(|e| {
            self.mark_unavailable();
            StorageError::CommitOutcomeUnknown {
                operation_id: receipt.operation_id,
                source: e,
            }
        })?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(store_id: Uuid) -> TextGeneration {
        let id = Uuid::new_v4();
        TextGeneration {
            id,
            store_id,
            filename: generation_filename(id),
            format_version: 2,
            sealed_length: 48,
            sealed_sha256: [0; 32],
        }
    }

    #[test]
    fn text_selection_receipt_reopens_and_stale_generation_cannot_replace_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.db");
        let store = MetadataStore::open(&path).unwrap();
        let before = store.control_state().unwrap();
        let first = descriptor(before.store_id);
        let receipt = store
            .select_text_generation(before.committed_generation, &first, &vec![])
            .unwrap();
        assert!(matches!(
            store.select_text_generation(
                before.committed_generation,
                &descriptor(before.store_id),
                &vec![]
            ),
            Err(StorageError::GenerationConflict { .. })
        ));
        assert_eq!(store.text_selection().unwrap(), Some(first.clone()));
        assert_eq!(
            store.control_state().unwrap().committed_generation,
            receipt.committed_generation
        );
        drop(store);
        let store = MetadataStore::open(&path).unwrap();
        assert_eq!(store.text_selection().unwrap(), Some(first));
        assert_eq!(
            store
                .lookup_operation(
                    before.store_id,
                    MutationScope::Catalog,
                    receipt.operation_id
                )
                .unwrap(),
            Some(receipt)
        );
    }

    #[test]
    fn text_catalog_partial_unknown_descriptor_and_unsafe_filename_fail_closed() {
        for fault in [
            "empty",
            "unknown-format",
            "unsafe-name",
            "missing-descriptor",
            "wrong-owner",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("meta.db");
            let store = MetadataStore::open(&path).unwrap();
            let mut d = descriptor(store.control_state().unwrap().store_id);
            match fault {
                "unknown-format" => d.format_version = 999,
                "unsafe-name" => d.filename = "../escape".into(),
                "wrong-owner" => d.store_id = Uuid::new_v4(),
                _ => {}
            }
            {
                let tx = store.db.begin_write().unwrap();
                {
                    let mut table = tx.open_table(CATALOG).unwrap();
                    if fault != "empty" {
                        table
                            .insert("active", control::encode(&d.id).unwrap().as_slice())
                            .unwrap();
                    }
                    if fault != "missing-descriptor" && fault != "empty" {
                        table
                            .insert(
                                d.id.to_string().as_str(),
                                control::encode(&d).unwrap().as_slice(),
                            )
                            .unwrap();
                    }
                }
                tx.commit().unwrap();
            }
            assert!(store.text_selection().is_err(), "{fault}");
            drop(store);
            assert!(MetadataStore::open(&path).is_err(), "{fault}");
        }
    }
}

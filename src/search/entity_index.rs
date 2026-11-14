//! Namespace-partitioned entity postings with complete per-memory replacement.
use std::collections::{HashMap, HashSet};

use crate::model::{CachedRecord, DecayPhase, DiskRecord, MemoryId, NamespaceId};

/// Derived entity index. Callers provide the complete accepted entity list;
/// legacy tags are not a lossless representation of multiword/Unicode names.
/// Namespace assignment is canonical and immutable at the storage boundary.
pub struct EntityIndex {
    index: HashMap<NamespaceId, HashMap<String, HashSet<MemoryId>>>,
    keys_by_memory: HashMap<(NamespaceId, MemoryId), HashSet<String>>,
}

impl EntityIndex {
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            index: HashMap::new(),
            keys_by_memory: HashMap::with_capacity(capacity),
        }
    }

    /// Build a fresh view for startup or committed-effect completion. Callers
    /// publish only after their existing canonical/readiness checks succeed.
    pub(crate) fn from_records(records: &[(MemoryId, DiskRecord)]) -> Self {
        let mut index = Self::with_capacity(records.len());
        for (id, record) in records {
            if matches!(record.phase, DecayPhase::Ghost | DecayPhase::Tombstone) {
                continue;
            }
            let cached = CachedRecord::from(record);
            index.add(cached.namespace_id, *id, &cached.entities);
        }
        index
    }

    /// Replace this namespace/UUID's complete postings. Repeating an update is
    /// idempotent; empty (or wholly unindexable) entities remove old postings.
    pub fn add(&mut self, namespace_id: NamespaceId, memory_id: MemoryId, entities: &[String]) {
        let keys: HashSet<_> = entities
            .iter()
            .flat_map(|entity| super::entity_normalize::canonicalize_entity(entity))
            .collect();
        if self.keys_by_memory.get(&(namespace_id, memory_id)) == Some(&keys) {
            return;
        }
        self.remove(namespace_id, memory_id);
        if keys.is_empty() {
            return;
        }
        let postings = self.index.entry(namespace_id).or_default();
        for key in &keys {
            postings.entry(key.clone()).or_default().insert(memory_id);
        }
        self.keys_by_memory.insert((namespace_id, memory_id), keys);
    }

    /// Remove all remembered postings without depending on caller-supplied
    /// labels, cached metadata, or tags stripped by a tombstone operation.
    pub fn remove(&mut self, namespace_id: NamespaceId, memory_id: MemoryId) -> bool {
        let Some(keys) = self.keys_by_memory.remove(&(namespace_id, memory_id)) else {
            return false;
        };
        if let Some(postings) = self.index.get_mut(&namespace_id) {
            for key in keys {
                if let Some(ids) = postings.get_mut(&key) {
                    ids.remove(&memory_id);
                    if ids.is_empty() {
                        postings.remove(&key);
                    }
                }
            }
            if postings.is_empty() {
                self.index.remove(&namespace_id);
            }
        }
        true
    }

    /// Also reaches stale entries whose canonical record has already vanished.
    pub fn remove_namespace(&mut self, namespace_id: NamespaceId) {
        self.index.remove(&namespace_id);
        self.keys_by_memory.retain(|(ns, _), _| *ns != namespace_id);
    }

    /// Matching canonical forms, descending count then ascending UUID bytes.
    /// Only this namespace's postings are read; aliases/query repeats count once.
    pub fn find_by_entities(
        &self,
        namespace_id: NamespaceId,
        entities: &[String],
        exclude_id: MemoryId,
    ) -> Vec<(MemoryId, usize)> {
        let Some(postings) = self.index.get(&namespace_id) else {
            return Vec::new();
        };
        let mut counts: HashMap<MemoryId, usize> = HashMap::new();
        let mut seen_keys: HashSet<String> = HashSet::new();
        for entity in entities {
            for key in super::entity_normalize::canonicalize_entity(entity) {
                if !seen_keys.insert(key.clone()) {
                    continue;
                }
                if let Some(ids) = postings.get(&key) {
                    for &id in ids {
                        if id != exclude_id {
                            *counts.entry(id).or_default() += 1;
                        }
                    }
                }
            }
        }
        let mut results: Vec<_> = counts.into_iter().collect();
        results.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        results
    }

    /// Distinct namespace/UUID entries with at least one normalized entity key.
    pub fn len(&self) -> usize {
        self.keys_by_memory.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys_by_memory.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns() -> NamespaceId {
        NamespaceId::new(1)
    }

    fn mid(n: u8) -> MemoryId {
        let mut bytes = [0u8; 16];
        bytes[15] = n;
        MemoryId::from_bytes(bytes)
    }

    // Codec-only canonical fixture: no schema-v2 serving or writer admission.
    fn canonical_record(
        n: u8,
        namespace: NamespaceId,
        entities: &[&str],
    ) -> (MemoryId, DiskRecord) {
        use crate::model::schema::{LabelProvenance, RecordSchemaV2, VectorReference};
        let id = mid(n);
        let mut record = DiskRecord {
            version: 1,
            schema: None,
            id: *id.as_bytes(),
            namespace_id: namespace.get(),
            created_at: 0,
            last_accessed_at: 0,
            phase: DecayPhase::Full,
            strength: 1.0,
            decay_strength: 1.0,
            stability: 1.0,
            difficulty: 5.0,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: "fixture".into(),
            tags: vec![crate::model::Tag::new("entity/retained-only").unwrap()],
            access_history: vec![],
            text_offset: 0,
            text_length: 0,
        };
        let mut schema = RecordSchemaV2::from_legacy(
            &record,
            0,
            Some(VectorReference {
                slot: 0,
                allocation_epoch: Default::default(),
                sha256: [0; 32],
            }),
        );
        schema.labels.entities = entities.iter().map(|s| (*s).to_owned()).collect();
        schema.labels.provenance = LabelProvenance::NativeV2;
        record.version = 2;
        record.schema = Some(schema);
        let decoded = DiskRecord::from_bytes(&record.encode().unwrap()).unwrap();
        (id, decoded)
    }

    #[test]
    fn entity_rebuild_uses_canonical_labels_and_replaces_the_complete_view() {
        let other = NamespaceId::new(2);
        let mut records = vec![
            canonical_record(1, ns(), &["Dr. John Smith", "JOHN SMITH", "José", "José"]),
            canonical_record(2, other, &["José"]),
            canonical_record(3, ns(), &[]), // retained tags are NOT a fallback
            canonical_record(4, ns(), &["日本語"]),
        ];
        let before: Vec<_> = records.iter().map(|(_, r)| r.encode().unwrap()).collect();
        let query = vec!["John Smith".into(), "José".into(), "José".into()];
        for _ in 0..3 {
            let index = EntityIndex::from_records(&records);
            assert_eq!(index.len(), 3);
            assert_eq!(
                index.find_by_entities(ns(), &query, mid(0)),
                vec![(mid(1), 3)]
            );
            assert_eq!(
                index.find_by_entities(other, &query, mid(0)),
                vec![(mid(2), 1)]
            );
            assert_eq!(
                index.find_by_entities(ns(), &["日本語".into()], mid(0)),
                vec![(mid(4), 1)]
            );
            assert!(
                index
                    .find_by_entities(ns(), &["retained-only".into()], mid(0))
                    .is_empty()
            );
        }
        assert_eq!(
            records
                .iter()
                .map(|(_, r)| r.encode().unwrap())
                .collect::<Vec<_>>(),
            before
        );

        // Replacement and disappearance must remove old keys and old UUIDs,
        // while authoritative empty does not recover the conflicting tag.
        records[0] = canonical_record(1, ns(), &["Alice's"]);
        records[1] = canonical_record(2, other, &[]);
        records.pop();
        let replacement = EntityIndex::from_records(&records);
        assert_eq!(replacement.len(), 1);
        assert!(
            replacement
                .find_by_entities(ns(), &query, mid(0))
                .is_empty()
        );
        assert!(
            replacement
                .find_by_entities(other, &query, mid(0))
                .is_empty()
        );
        assert!(
            replacement
                .find_by_entities(ns(), &["日本語".into()], mid(0))
                .is_empty()
        );
        assert_eq!(
            replacement.find_by_entities(ns(), &["Alice".into()], mid(0)),
            vec![(mid(1), 1)]
        );
    }

    #[test]
    fn entity_rebuild_preserves_retained_only_provenance_and_excludes_removed_records() {
        use crate::model::schema::LabelProvenance;
        let (id, mut legacy) = canonical_record(1, ns(), &[]);
        legacy.version = 1;
        legacy.schema = None;
        let mut retained = canonical_record(2, ns(), &["retained-only"]);
        retained.1.schema.as_mut().unwrap().labels.provenance = LabelProvenance::LegacyRetainedOnly;
        let mut ghost = canonical_record(3, ns(), &[]);
        ghost.1.phase = DecayPhase::Ghost;
        let mut tombstone = canonical_record(4, ns(), &[]);
        tombstone.1.phase = DecayPhase::Tombstone;
        tombstone.1.schema.as_mut().unwrap().vector = None;
        let records = vec![(id, legacy), retained, ghost, tombstone];
        let bytes: Vec<_> = records.iter().map(|(_, r)| r.encode().unwrap()).collect();
        let index = EntityIndex::from_records(&records);
        assert_eq!(index.len(), 2);
        assert_eq!(
            index.find_by_entities(ns(), &["retained-only".into()], mid(0)),
            vec![(mid(1), 1), (mid(2), 1)]
        );
        assert!(
            index
                .find_by_entities(ns(), &["José".into()], mid(0))
                .is_empty()
        );
        assert_eq!(
            records[1].1.schema.as_ref().unwrap().labels.provenance,
            LabelProvenance::LegacyRetainedOnly
        );
        assert_eq!(
            records
                .iter()
                .map(|(_, r)| r.encode().unwrap())
                .collect::<Vec<_>>(),
            bytes
        );
    }

    #[test]
    fn add_and_find() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &["Alice".into(), "Bob".into()]);
        idx.add(ns(), mid(2), &["Alice".into(), "Charlie".into()]);
        let results = idx.find_by_entities(ns(), &["Alice".into()], mid(1));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(2));
    }

    #[test]
    fn case_insensitive() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &["Alice".into()]);
        idx.add(ns(), mid(2), &["alice".into()]);
        let results = idx.find_by_entities(ns(), &["ALICE".into()], mid(1));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(2));
    }

    #[test]
    fn remove_memory() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &["Alice".into()]);
        idx.add(ns(), mid(2), &["Alice".into()]);
        idx.remove(ns(), mid(1));
        let results = idx.find_by_entities(ns(), &["Alice".into()], mid(3));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(2));
    }

    #[test]
    fn remove_cleans_empty_postings() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &["Alice".into()]);
        idx.remove(ns(), mid(1));
        assert!(idx.index.is_empty());
    }

    #[test]
    fn exclude_id() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &["Alice".into()]);
        let results = idx.find_by_entities(ns(), &["Alice".into()], mid(1));
        assert!(results.is_empty());
    }

    #[test]
    fn sorted_by_shared_count_desc() {
        let mut idx = EntityIndex::new();
        idx.add(
            ns(),
            mid(1),
            &["Alice".into(), "Bob".into(), "Charlie".into()],
        );
        idx.add(ns(), mid(2), &["Alice".into()]);
        idx.add(ns(), mid(3), &["Alice".into(), "Bob".into()]);
        let results = idx.find_by_entities(
            ns(),
            &["Alice".into(), "Bob".into(), "Charlie".into()],
            mid(1),
        );
        assert_eq!(results[0].0, mid(3)); // 2 shared
        assert_eq!(results[1].0, mid(2)); // 1 shared
    }

    #[test]
    fn empty_entities() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &[]);
        assert_eq!(idx.len(), 0);
        let results = idx.find_by_entities(ns(), &[], mid(1));
        assert!(results.is_empty());
    }
    #[test]
    fn replacement_and_removal_are_idempotent_and_clear_all_aliases() {
        let mut idx = EntityIndex::new();
        let labels = vec!["Dr. John Smith".into(), "JOHN SMITH".into(), "José".into()];
        for _ in 0..3 {
            idx.add(ns(), mid(1), &labels);
        }
        assert_eq!(idx.len(), 1);
        assert_eq!(
            idx.find_by_entities(ns(), &labels, mid(0)),
            vec![(mid(1), 3)]
        );
        idx.add(ns(), mid(1), &["Alice's".into()]);
        assert_eq!(idx.len(), 1);
        assert!(idx.find_by_entities(ns(), &labels, mid(0)).is_empty());
        assert_eq!(
            idx.find_by_entities(ns(), &["Alice".into(), "ALICE".into()], mid(0)),
            vec![(mid(1), 1)]
        );
        idx.add(ns(), mid(1), &[]);
        assert!(idx.is_empty());
        assert!(idx.index.is_empty());
        assert!(!idx.remove(ns(), mid(1)));
        idx.add(ns(), mid(1), &labels);
        idx.add(ns(), mid(1), &["   ".into()]);
        assert!(idx.is_empty());
        assert!(idx.index.is_empty());
    }

    #[test]
    fn namespaces_are_isolated_for_same_id_updates_removals_and_ties() {
        let mut idx = EntityIndex::new();
        let other = NamespaceId::new(2);
        let names = vec!["Alice".into()];
        for id in [mid(3), mid(1), mid(2)] {
            idx.add(ns(), id, &names);
        }
        idx.add(other, mid(1), &["Alice".into(), "Bob".into()]);
        assert_eq!(idx.len(), 4);
        assert_eq!(
            idx.find_by_entities(ns(), &names, mid(0)),
            vec![(mid(1), 1), (mid(2), 1), (mid(3), 1)]
        );
        idx.add(ns(), mid(1), &["Carol".into()]);
        assert_eq!(
            idx.find_by_entities(other, &names, mid(0)),
            vec![(mid(1), 1)]
        );
        assert!(idx.remove(ns(), mid(1)));
        assert!(!idx.remove(ns(), mid(1)));
        assert_eq!(idx.len(), 3);
        idx.remove_namespace(ns());
        idx.remove_namespace(ns());
        assert_eq!(idx.len(), 1);
        assert_eq!(
            idx.find_by_entities(other, &names, mid(0)),
            vec![(mid(1), 1)]
        );
        assert!(idx.find_by_entities(ns(), &names, mid(0)).is_empty());
        idx.remove_namespace(other);
        assert!(idx.is_empty());
        assert!(idx.index.is_empty());
    }
}

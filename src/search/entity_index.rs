//! Entity inverted index for entity-based memory recall.
//!
//! Maps entity names (case-insensitive) to sets of memory IDs,
//! enabling recall of memories that share named entities with
//! a query.

use std::collections::{HashMap, HashSet};

use crate::model::MemoryId;

/// Inverted index mapping entity names to the memories that reference them.
///
/// Supports case-insensitive entity lookups for entity-based recall
/// in the search pipeline (CS-29).
pub struct EntityIndex {
    index: HashMap<String, HashSet<MemoryId>>,
    memory_count: usize,
}

impl EntityIndex {
    /// Create a new empty entity index.
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
            memory_count: 0,
        }
    }

    /// Create a new entity index with pre-allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            index: HashMap::with_capacity(capacity),
            memory_count: 0,
        }
    }

    /// Add a memory's entities to the index.
    ///
    /// Each entity string is lowercased before indexing.
    /// No-op if `entities` is empty.
    pub fn add(&mut self, memory_id: MemoryId, entities: &[String]) {
        if entities.is_empty() {
            return;
        }
        let mut added = false;
        for entity in entities {
            let canonical_forms = super::entity_normalize::canonicalize_entity(entity);
            for key in canonical_forms {
                let set = self.index.entry(key).or_default();
                if set.insert(memory_id) {
                    added = true;
                }
            }
        }
        if added {
            self.memory_count += 1;
        }
    }

    /// Remove a memory's entities from the index.
    ///
    /// Cleans up empty posting lists after removal.
    pub fn remove(&mut self, memory_id: MemoryId, entities: &[String]) {
        let mut removed = false;
        for entity in entities {
            let canonical_forms = super::entity_normalize::canonicalize_entity(entity);
            for key in canonical_forms {
                if let Some(set) = self.index.get_mut(&key) {
                    if set.remove(&memory_id) {
                        removed = true;
                    }
                    if set.is_empty() {
                        self.index.remove(&key);
                    }
                }
            }
        }
        if removed && self.memory_count > 0 {
            self.memory_count -= 1;
        }
    }

    /// Find memories sharing entities with the given set, excluding `exclude_id`.
    /// Returns `(MemoryId, shared_count)` sorted by shared count descending.
    pub fn find_by_entities(
        &self,
        entities: &[String],
        exclude_id: MemoryId,
    ) -> Vec<(MemoryId, usize)> {
        let mut counts: HashMap<MemoryId, usize> = HashMap::new();
        let mut seen_keys: HashSet<String> = HashSet::new();
        for entity in entities {
            let canonical_forms = super::entity_normalize::canonicalize_entity(entity);
            for key in canonical_forms {
                if !seen_keys.insert(key.clone()) {
                    continue;
                }
                if let Some(set) = self.index.get(&key) {
                    for &mid in set {
                        if mid != exclude_id {
                            *counts.entry(mid).or_default() += 1;
                        }
                    }
                }
            }
        }
        let mut results: Vec<(MemoryId, usize)> = counts.into_iter().collect();
        results.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        results
    }

    /// Return the number of distinct memories in the index.
    pub fn len(&self) -> usize {
        self.memory_count
    }

    /// Whether the index contains no memories.
    pub fn is_empty(&self) -> bool {
        self.memory_count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(n: u8) -> MemoryId {
        let mut bytes = [0u8; 16];
        bytes[15] = n;
        MemoryId::from_bytes(bytes)
    }

    #[test]
    fn add_and_find() {
        let mut idx = EntityIndex::new();
        idx.add(mid(1), &["Alice".into(), "Bob".into()]);
        idx.add(mid(2), &["Alice".into(), "Charlie".into()]);
        let results = idx.find_by_entities(&["Alice".into()], mid(1));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(2));
    }

    #[test]
    fn case_insensitive() {
        let mut idx = EntityIndex::new();
        idx.add(mid(1), &["Alice".into()]);
        idx.add(mid(2), &["alice".into()]);
        let results = idx.find_by_entities(&["ALICE".into()], mid(1));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(2));
    }

    #[test]
    fn remove_memory() {
        let mut idx = EntityIndex::new();
        idx.add(mid(1), &["Alice".into()]);
        idx.add(mid(2), &["Alice".into()]);
        idx.remove(mid(1), &["Alice".into()]);
        let results = idx.find_by_entities(&["Alice".into()], mid(3));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(2));
    }

    #[test]
    fn remove_cleans_empty_postings() {
        let mut idx = EntityIndex::new();
        idx.add(mid(1), &["Alice".into()]);
        idx.remove(mid(1), &["Alice".into()]);
        assert!(idx.index.is_empty());
    }

    #[test]
    fn exclude_id() {
        let mut idx = EntityIndex::new();
        idx.add(mid(1), &["Alice".into()]);
        let results = idx.find_by_entities(&["Alice".into()], mid(1));
        assert!(results.is_empty());
    }

    #[test]
    fn sorted_by_shared_count_desc() {
        let mut idx = EntityIndex::new();
        idx.add(mid(1), &["Alice".into(), "Bob".into(), "Charlie".into()]);
        idx.add(mid(2), &["Alice".into()]);
        idx.add(mid(3), &["Alice".into(), "Bob".into()]);
        let results =
            idx.find_by_entities(&["Alice".into(), "Bob".into(), "Charlie".into()], mid(1));
        assert_eq!(results[0].0, mid(3)); // 2 shared
        assert_eq!(results[1].0, mid(2)); // 1 shared
    }

    #[test]
    fn empty_entities() {
        let mut idx = EntityIndex::new();
        idx.add(mid(1), &[]);
        assert_eq!(idx.len(), 0);
        let results = idx.find_by_entities(&[], mid(1));
        assert!(results.is_empty());
    }
}

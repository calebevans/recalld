//! FlatVectorIndex — brute-force SIMD scan over RAM-resident vectors.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    SearchFilter, VectorError, VectorIndex, VectorMetadata, VectorSearchResult, dot_product_simd,
};
use crate::model::{MemoryId, NamespaceId};

// ---------------------------------------------------------------------------
// TagInterner
// ---------------------------------------------------------------------------

/// Intern table for tag strings, mapping between strings and u16 indices.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TagInterner {
    tag_to_index: HashMap<String, u16>,
    index_to_tag: Vec<String>,
}

impl TagInterner {
    /// Intern a tag string, returning its index.
    pub fn intern(&mut self, tag: &str) -> u16 {
        if let Some(&idx) = self.tag_to_index.get(tag) {
            return idx;
        }
        let idx = self.index_to_tag.len() as u16;
        self.index_to_tag.push(tag.to_string());
        self.tag_to_index.insert(tag.to_string(), idx);
        idx
    }

    /// Resolve an index back to its tag string.
    #[allow(dead_code)]
    pub fn resolve(&self, idx: u16) -> Option<&str> {
        self.index_to_tag.get(idx as usize).map(|s| s.as_str())
    }

    /// Look up a tag string's index without interning it.
    pub fn lookup(&self, tag: &str) -> Option<u16> {
        self.tag_to_index.get(tag).copied()
    }
}

// ---------------------------------------------------------------------------
// FilterEntry
// ---------------------------------------------------------------------------

/// Pre-filter metadata stored alongside each vector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterEntry {
    /// The memory this entry belongs to.
    pub id: MemoryId,
    /// Namespace for namespace-scoped filtering.
    pub namespace_id: NamespaceId,
    /// Decay phase (1/2/3) for phase filtering.
    pub decay_phase: u8,
    /// Tags as interned indices.
    pub tag_indices: Vec<u16>,
}

// ---------------------------------------------------------------------------
// FlatVectorIndex
// ---------------------------------------------------------------------------

/// Brute-force vector index with SIMD-accelerated dot product.
///
/// Stores all vectors in a single contiguous `Vec<f32>` for sequential
/// scan performance. Vector `i` occupies `vectors[i*dim .. (i+1)*dim]`.
pub struct FlatVectorIndex {
    /// Dimensionality of all vectors in this index.
    dim: usize,
    /// Dense vector storage. Length = `num_vectors * dim`.
    vectors: Vec<f32>,
    /// Parallel metadata array.
    entries: Vec<FilterEntry>,
    /// MemoryId -> slot index for O(1) lookup and removal.
    id_to_slot: HashMap<MemoryId, usize>,
    /// Tag interning for efficient filter comparison.
    tags: TagInterner,
    /// Threshold at which `needs_rebuild` returns `true`.
    rebuild_threshold: usize,
}

impl FlatVectorIndex {
    /// Create a new empty flat vector index with the given dimensionality.
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            vectors: Vec::new(),
            entries: Vec::new(),
            id_to_slot: HashMap::new(),
            tags: TagInterner::default(),
            rebuild_threshold: 100_000,
        }
    }

    /// Create a new flat vector index with a custom rebuild threshold.
    pub fn with_rebuild_threshold(dim: usize, threshold: usize) -> Self {
        Self {
            rebuild_threshold: threshold,
            ..Self::new(dim)
        }
    }

    /// Pre-allocate capacity for the expected number of vectors.
    pub fn reserve(&mut self, additional: usize) {
        self.vectors.reserve(additional * self.dim);
        self.entries.reserve(additional);
        self.id_to_slot.reserve(additional);
    }

    /// Load a previously saved index from disk.
    pub fn load(path: &Path) -> Result<Self, VectorError> {
        let mut file = std::fs::File::open(path)?;

        // --- Header ---
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != b"CHVI" {
            return Err(VectorError::CorruptIndex(format!(
                "bad magic: expected CHVI, got {:?}",
                magic
            )));
        }

        let mut buf4 = [0u8; 4];
        file.read_exact(&mut buf4)?;
        let version = u32::from_le_bytes(buf4);
        if version != 1 {
            return Err(VectorError::CorruptIndex(format!(
                "unsupported version: {version}"
            )));
        }

        file.read_exact(&mut buf4)?;
        let dim = u32::from_le_bytes(buf4) as usize;

        let mut buf8 = [0u8; 8];
        file.read_exact(&mut buf8)?;
        let count = u64::from_le_bytes(buf8) as usize;

        // --- Vectors ---
        let total_floats = count * dim;
        let mut vectors = vec![0f32; total_floats];
        let vector_bytes: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(vectors.as_mut_ptr() as *mut u8, total_floats * 4)
        };
        file.read_exact(vector_bytes)?;

        // --- Metadata ---
        file.read_exact(&mut buf8)?;
        let meta_len = u64::from_le_bytes(buf8) as usize;
        let mut meta_bytes = vec![0u8; meta_len];
        file.read_exact(&mut meta_bytes)?;
        let entries: Vec<FilterEntry> = bincode::deserialize(&meta_bytes)
            .map_err(|e| VectorError::CorruptIndex(format!("metadata: {e}")))?;

        // --- Tag interner ---
        file.read_exact(&mut buf8)?;
        let tag_len = u64::from_le_bytes(buf8) as usize;
        let mut tag_bytes = vec![0u8; tag_len];
        file.read_exact(&mut tag_bytes)?;
        let index_to_tag: Vec<String> = bincode::deserialize(&tag_bytes)
            .map_err(|e| VectorError::CorruptIndex(format!("tags: {e}")))?;

        // Rebuild derived structures.
        let mut tag_to_index = HashMap::new();
        for (i, tag) in index_to_tag.iter().enumerate() {
            tag_to_index.insert(tag.clone(), i as u16);
        }

        let mut id_to_slot = HashMap::with_capacity(count);
        for (slot, entry) in entries.iter().enumerate() {
            id_to_slot.insert(entry.id, slot);
        }

        Ok(Self {
            dim,
            vectors,
            entries,
            id_to_slot,
            tags: TagInterner {
                tag_to_index,
                index_to_tag,
            },
            rebuild_threshold: 100_000,
        })
    }

    /// Check if a filter entry passes the given search filter.
    #[inline]
    fn passes_filter(entry: &FilterEntry, filter: &SearchFilter, tags: &TagInterner) -> bool {
        // Namespace check.
        if let Some(ns) = filter.namespace_id {
            if entry.namespace_id != ns {
                return false;
            }
        }

        // Decay phase check.
        if let Some(ref phases) = filter.decay_phases {
            if !phases.contains(&entry.decay_phase) {
                return false;
            }
        }

        // Tag inclusion: OR semantics — must have at least one.
        if !filter.include_tags.is_empty() {
            let has_any = filter.include_tags.iter().any(|tag| {
                tags.lookup(tag)
                    .is_some_and(|idx| entry.tag_indices.contains(&idx))
            });
            if !has_any {
                return false;
            }
        }

        // Tag exclusion: must not have any excluded tags.
        if !filter.exclude_tags.is_empty() {
            let has_excluded = filter.exclude_tags.iter().any(|tag| {
                tags.lookup(tag)
                    .is_some_and(|idx| entry.tag_indices.contains(&idx))
            });
            if has_excluded {
                return false;
            }
        }

        true
    }
}

impl VectorIndex for FlatVectorIndex {
    fn add(
        &mut self,
        id: MemoryId,
        vector: &[f32],
        metadata: VectorMetadata,
    ) -> Result<(), VectorError> {
        if vector.len() != self.dim {
            return Err(VectorError::DimensionMismatch {
                expected: self.dim,
                got: vector.len(),
            });
        }

        let tag_indices: Vec<u16> = metadata.tags.iter().map(|t| self.tags.intern(t)).collect();

        // If this ID already exists, overwrite in place.
        if let Some(&slot) = self.id_to_slot.get(&id) {
            let offset = slot * self.dim;
            self.vectors[offset..offset + self.dim].copy_from_slice(vector);
            self.entries[slot] = FilterEntry {
                id,
                namespace_id: metadata.namespace_id,
                decay_phase: metadata.decay_phase,
                tag_indices,
            };
            return Ok(());
        }

        // Append new vector.
        let slot = self.entries.len();
        self.vectors.extend_from_slice(vector);
        self.entries.push(FilterEntry {
            id,
            namespace_id: metadata.namespace_id,
            decay_phase: metadata.decay_phase,
            tag_indices,
        });
        self.id_to_slot.insert(id, slot);

        Ok(())
    }

    fn remove(&mut self, id: MemoryId) -> Result<bool, VectorError> {
        let Some(slot) = self.id_to_slot.remove(&id) else {
            return Ok(false);
        };

        let last_slot = self.entries.len() - 1;

        if slot != last_slot {
            // Swap-with-last to maintain gap-free contiguous buffer.
            let last_offset = last_slot * self.dim;
            let slot_offset = slot * self.dim;
            for i in 0..self.dim {
                self.vectors[slot_offset + i] = self.vectors[last_offset + i];
            }

            self.entries.swap(slot, last_slot);

            let swapped_id = self.entries[slot].id;
            self.id_to_slot.insert(swapped_id, slot);
        }

        self.vectors.truncate(last_slot * self.dim);
        self.entries.pop();

        Ok(true)
    }

    fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<VectorSearchResult>, VectorError> {
        if query.len() != self.dim {
            return Err(VectorError::DimensionMismatch {
                expected: self.dim,
                got: query.len(),
            });
        }

        if k == 0 || self.entries.is_empty() {
            return Ok(Vec::new());
        }

        let min_score = filter.min_score.unwrap_or(f32::NEG_INFINITY);

        // Min-heap of size K via reversed Ord on ScoredEntry.
        let mut heap: BinaryHeap<ScoredEntry> = BinaryHeap::with_capacity(k + 1);

        for (i, entry) in self.entries.iter().enumerate() {
            if !Self::passes_filter(entry, filter, &self.tags) {
                continue;
            }

            let offset = i * self.dim;
            let candidate = &self.vectors[offset..offset + self.dim];
            let score = dot_product_simd(query, candidate);

            if score < min_score {
                continue;
            }

            if heap.len() < k {
                heap.push(ScoredEntry { score, index: i });
            } else if let Some(worst) = heap.peek() {
                if score > worst.score {
                    heap.pop();
                    heap.push(ScoredEntry { score, index: i });
                }
            }
        }

        // Extract results sorted by descending score.
        let mut results: Vec<VectorSearchResult> = heap
            .into_sorted_vec()
            .into_iter()
            .rev()
            .map(|se| VectorSearchResult {
                id: self.entries[se.index].id,
                score: se.score,
                decay_phase: self.entries[se.index].decay_phase,
            })
            .collect();

        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));

        Ok(results)
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn save(&self, path: &Path) -> Result<(), VectorError> {
        use std::io::Write as _;
        let mut file = std::fs::File::create(path)?;

        // Header: magic, version, dimensions, count.
        file.write_all(b"CHVI")?;
        file.write_all(&1u32.to_le_bytes())?;
        file.write_all(&(self.dim as u32).to_le_bytes())?;
        file.write_all(&(self.entries.len() as u64).to_le_bytes())?;

        // Vectors: dense f32 array.
        let vector_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(self.vectors.as_ptr() as *const u8, self.vectors.len() * 4)
        };
        file.write_all(vector_bytes)?;

        // Metadata: bincode-serialized FilterEntry array.
        let metadata_bytes = bincode::serialize(&self.entries)
            .map_err(|e| VectorError::Serialization(format!("entries: {e}")))?;
        file.write_all(&(metadata_bytes.len() as u64).to_le_bytes())?;
        file.write_all(&metadata_bytes)?;

        // Tag interner: bincode-serialized Vec<String>.
        let tag_bytes = bincode::serialize(&self.tags.index_to_tag)
            .map_err(|e| VectorError::Serialization(format!("tags: {e}")))?;
        file.write_all(&(tag_bytes.len() as u64).to_le_bytes())?;
        file.write_all(&tag_bytes)?;

        file.flush()?;
        Ok(())
    }

    fn needs_rebuild(&self) -> bool {
        self.entries.len() >= self.rebuild_threshold
    }

    fn get_vector(&self, id: MemoryId) -> Option<Vec<f32>> {
        let &slot = self.id_to_slot.get(&id)?;
        let offset = slot * self.dim;
        let end = offset + self.dim;
        Some(self.vectors[offset..end].to_vec())
    }

    fn update_metadata(
        &mut self,
        id: MemoryId,
        metadata: VectorMetadata,
    ) -> Result<bool, VectorError> {
        let Some(&slot) = self.id_to_slot.get(&id) else {
            return Ok(false);
        };
        let tag_indices: Vec<u16> = metadata.tags.iter().map(|t| self.tags.intern(t)).collect();
        self.entries[slot].namespace_id = metadata.namespace_id;
        self.entries[slot].decay_phase = metadata.decay_phase;
        self.entries[slot].tag_indices = tag_indices;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// ScoredEntry (top-K heap element)
// ---------------------------------------------------------------------------

/// Internal scored entry for the top-K min-heap.
#[derive(Debug, Clone)]
struct ScoredEntry {
    score: f32,
    index: usize,
}

impl PartialEq for ScoredEntry {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score && self.index == other.index
    }
}

impl Eq for ScoredEntry {}

impl PartialOrd for ScoredEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // REVERSED: lower score = "greater" in heap ordering,
        // turning BinaryHeap into a min-heap by score.
        other
            .score
            .partial_cmp(&self.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| self.index.cmp(&other.index))
    }
}

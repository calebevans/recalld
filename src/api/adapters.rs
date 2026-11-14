//! Adapter implementations connecting API-layer traits to real subsystems.
//!
//! Each adapter wraps the concrete subsystem type and implements the
//! corresponding API trait from `state.rs`. These adapters bridge the
//! gap between the API server's DI traits and the concrete subsystem
//! types held by the `Recalld` struct.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::cache::CacheManager;
use crate::config::RecalldConfig;
use crate::embedding::EmbeddingProvider;
use crate::graph::SharedGraph;
use crate::model::{
    AccessKind, CachedRecord, DecayPhase, DiskRecord, MemoryId, NamespaceConfig, NamespaceId, Tag,
    parse_structured_tags,
};
use crate::search::{EntityIndex, FlatVectorIndex, FtsIndex};
use crate::storage::RedbStorageEngine;
// Import the StorageEngine trait so its methods are in scope.
use super::state;
use crate::storage::StorageEngine as StorageEngineTrait;

// ═══════════════════════════════════════════════════════════════════════
// LinkError -> AppError
// ═══════════════════════════════════════════════════════════════════════

/// A rejected `supersedes`/`parentId` target becomes a 422 naming the
/// offending field — 422 is exactly the "syntactically fine, semantically
/// impossible" case. The two internal-inconsistency variants are ours, not
/// the caller's, so they become 500s instead.
impl From<crate::graph::LinkError> for super::errors::AppError {
    fn from(e: crate::graph::LinkError) -> Self {
        use crate::graph::LinkError;
        let field = e.field().to_string();
        match e {
            LinkError::TargetNotFound { .. }
            | LinkError::NamespaceMismatch { .. }
            | LinkError::SelfReference { .. } => super::errors::AppError::UnprocessableEntity {
                message: e.to_string(),
                field: Some(field),
            },
            LinkError::TargetMissingFromGraph { .. } | LinkError::Lookup { .. } => {
                super::errors::AppError::Internal {
                    source: Box::new(e),
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// SearchPipelineAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts the real search subsystems to the API `SearchPipeline` trait.
pub struct SearchPipelineAdapter {
    query_engine: Arc<crate::search::QueryEngine>,
    embedding: Arc<dyn EmbeddingProvider>,
    vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
    fts_index: Arc<tokio::sync::Mutex<FtsIndex>>,
    entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
}

impl SearchPipelineAdapter {
    /// Creates a new adapter from the real search subsystem components.
    pub fn new(
        query_engine: Arc<crate::search::QueryEngine>,
        embedding: Arc<dyn EmbeddingProvider>,
        vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
        fts_index: Arc<tokio::sync::Mutex<FtsIndex>>,
        entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
    ) -> Self {
        Self {
            query_engine,
            embedding,
            vector_index,
            fts_index,
            entity_index,
        }
    }
}

#[async_trait]
impl state::SearchPipeline for SearchPipelineAdapter {
    async fn embed_text(
        &self,
        text: &str,
        _namespace_id: NamespaceId,
    ) -> Result<Vec<f32>, crate::embedding::EmbeddingError> {
        self.embedding.embed(text).await
    }

    async fn search(
        &self,
        query: state::SearchQuery,
    ) -> Result<Vec<state::ResolvedSearchResult>, crate::search::SearchError> {
        // Convert API-layer SearchQuery to the pipeline's SearchQuery.
        let text = match query.query {
            state::QueryInput::Text(t) => Some(t),
            state::QueryInput::Vector(_) => None,
        };

        // A label that cannot become a tag is an error, not a dropped
        // filter: dropping one from `includeTags` or `entities` WIDENS
        // the query, and a caller cannot tell "every memory in the
        // namespace" from a genuine match.
        let mut require_tags: Vec<Tag> =
            crate::model::parse_filter_tags("includeTags", "", &query.include_tags)
                .map_err(|e| crate::search::SearchError::InvalidFilter(e.to_string()))?;
        require_tags.extend(
            crate::model::parse_filter_tags(
                "entities",
                crate::model::constants::ENTITY_TAG_PREFIX,
                &query.entities,
            )
            .map_err(|e| crate::search::SearchError::InvalidFilter(e.to_string()))?,
        );
        // `excludeTags` is the safe direction — dropping one can only
        // ever return MORE than asked for in the same sense a missing
        // exclusion does, and it never masquerades as a match — so it
        // keeps the lossy parse.
        let exclude_tags: Vec<Tag> =
            crate::model::parse_tags_lossy("search.excludeTags", &query.exclude_tags);
        let phases: Vec<DecayPhase> = query
            .decay_phases
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .filter_map(|&p| match p {
                        0 => Some(DecayPhase::Full),
                        1 => Some(DecayPhase::Summary),
                        2 => Some(DecayPhase::Ghost),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let pipeline_query = crate::search::SearchQuery {
            text,
            fts_query: None,
            namespace: query.namespace_name.clone(), // Issue 1: pass actual namespace name
            filter: crate::search::PipelineSearchFilter {
                require_tags,
                exclude_tags,
                phases,
                min_strength: query.min_score, // Issue 2: pass min_strength
                ..Default::default()
            },
            limit: query.k,
            min_score: 0.0,
            include_ghosts: false,
            query_mode: crate::search::QueryMode::default(),
            graph_depth: (query.graph_depth as u8).min(3),
            time_range_start: query.time_range_start, // Issue 3: pass time range
            time_range_end: query.time_range_end,     // Issue 3: pass time range
            entities: query.entities.clone(),         // Issue 4: pass entities
        };
        let response = self.query_engine.search(pipeline_query).await?;
        Ok(response
            .results
            .into_iter()
            .map(|r| {
                // Issue 5: Parse entities from tags
                let metadata = parse_structured_tags(&r.tags);
                state::ResolvedSearchResult {
                    memory: CachedRecord {
                        id: r.memory_id,
                        namespace_id: query.namespace_id, // Issue 6: use actual namespace_id
                        created_at: r.created_at,
                        last_accessed_at: r.last_accessed_at,
                        phase: r.phase,
                        strength: r.retrievability,
                        decay_strength: r.effective_r,
                        stability: r.stability,
                        difficulty: 5.0,
                        is_permastore: r.is_permastore,
                        summary: r.summary.unwrap_or_default(),
                        tags: r.tags,
                        edge_count: r.edge_count,
                        vector_slot: 0,
                        entities: metadata.entities,
                    },
                    score: r.score.unwrap_or(0.0),
                }
            })
            .collect())
    }

    async fn near_duplicates(
        &self,
        embedding: &[f32],
        namespace_id: NamespaceId,
        threshold: f32,
        limit: usize,
    ) -> Result<Vec<(MemoryId, f32)>, super::errors::AppError> {
        let stamp = self.query_engine.effects_stamp().await?;
        let index = self.vector_index.read().await;
        let result =
            crate::search::find_near_duplicates(&index, embedding, namespace_id, threshold, limit);
        drop(index);
        if self.query_engine.effects_stamp().await? != stamp {
            return Err(crate::storage::StorageError::RecoveryRequired(
                "effects changed during advisory".into(),
            )
            .into());
        }
        Ok(result)
    }

    async fn index_memory(&self, id: MemoryId, embedding: &[f32], namespace_id: NamespaceId) {
        use crate::search::{VectorIndex, VectorMetadata};
        let mut index = self.vector_index.write().await;
        let metadata = VectorMetadata {
            namespace_id,
            decay_phase: DecayPhase::Full.as_u8(),
            tags: Vec::new(),
        };
        let _ = index.add(id, embedding, metadata);
    }

    async fn remove_from_index(&self, id: MemoryId) {
        use crate::search::VectorIndex;
        let mut index = self.vector_index.write().await;
        let _ = index.remove(id);
    }

    fn get_embedding(&self, id: MemoryId) -> Option<Vec<f32>> {
        use crate::search::VectorIndex;
        let index = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.vector_index.read())
        });
        index.get_vector(id)
    }

    fn get_embeddings_batch(
        &self,
        ids: &[MemoryId],
    ) -> std::collections::HashMap<MemoryId, Vec<f32>> {
        use crate::search::VectorIndex;
        // Acquire the lock once for the entire batch instead of
        // once per ID, amortizing the block_in_place overhead.
        let index = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.vector_index.read())
        });
        let mut result = std::collections::HashMap::with_capacity(ids.len());
        for &id in ids {
            if let Some(vec) = index.get_vector(id) {
                result.insert(id, vec);
            }
        }
        result
    }

    fn indexed_count(&self) -> usize {
        use crate::search::VectorIndex;
        let index = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.vector_index.read())
        });
        index.len()
    }

    async fn embedding_provider_healthy(&self) -> bool {
        // A simple health check: try to embed a trivial string.
        self.embedding.embed("health check").await.is_ok()
    }

    async fn fts_add(
        &self,
        namespace_id: NamespaceId,
        memory_id: MemoryId,
        summary: &str,
        full_text: Option<&str>,
        tags: &[String],
    ) {
        let fts = self.fts_index.lock().await;
        if let Err(e) = fts.add(namespace_id, memory_id, summary, full_text, tags) {
            tracing::warn!(
                memory_id = %memory_id,
                %e,
                "FTS5 indexing failed (non-fatal)"
            );
        }
    }

    async fn fts_remove(&self, memory_id: MemoryId) {
        let fts = self.fts_index.lock().await;
        if let Err(e) = fts.remove(memory_id) {
            tracing::warn!(
                memory_id = %memory_id,
                %e,
                "FTS5 removal failed (non-fatal)"
            );
        }
    }

    async fn entity_index_add(
        &self,
        namespace_id: NamespaceId,
        memory_id: MemoryId,
        entities: &[String],
    ) {
        self.entity_index
            .write()
            .await
            .add(namespace_id, memory_id, entities);
    }

    async fn entity_index_remove(&self, namespace_id: NamespaceId, memory_id: MemoryId) {
        self.entity_index
            .write()
            .await
            .remove(namespace_id, memory_id);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// StorageEngineAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `RedbStorageEngine` to the API `StorageEngine` trait.
pub struct StorageEngineAdapter {
    clock: Arc<dyn crate::time::Clock>,
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: Arc<CacheManager>,
    /// Held so `delete_memory` can clean the indexes storage cannot
    /// reach. Without them this adapter could tombstone a record and
    /// leave it indexed everywhere else.
    vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
    /// See [`vector_index`](Self::vector_index).
    fts_index: Arc<tokio::sync::Mutex<FtsIndex>>,
    /// See [`vector_index`](Self::vector_index).
    entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
    /// See [`vector_index`](Self::vector_index).
    graph: crate::graph::SharedGraph,
}

impl StorageEngineAdapter {
    /// Creates a new adapter wrapping the storage engine, the cache, and
    /// the four indexes a delete has to clean.
    pub fn new(
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        cache: Arc<CacheManager>,
        vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
        fts_index: Arc<tokio::sync::Mutex<FtsIndex>>,
        entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
        graph: crate::graph::SharedGraph,
    ) -> Self {
        cache.bind_storage(&storage);
        Self {
            clock: Arc::new(crate::time::SystemClock),
            storage,
            cache,
            vector_index,
            fts_index,
            entity_index,
            graph,
        }
    }
    /// Use the system instance's clock (or an isolated manual clock).
    pub fn with_clock(mut self, clock: Arc<dyn crate::time::Clock>) -> Self {
        self.clock = clock;
        self
    }
}

#[async_trait]
impl state::StorageEngine for StorageEngineAdapter {
    async fn publication_lease(
        &self,
    ) -> Result<Option<tokio::sync::OwnedRwLockReadGuard<()>>, crate::storage::StorageError> {
        Ok(Some(
            crate::derived_completion::publication_gate(&self.storage)
                .await?
                .read_owned()
                .await,
        ))
    }

    async fn effects_stamp(&self) -> Result<u64, crate::storage::StorageError> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            storage
                .read()
                .map_err(|e| crate::storage::StorageError::RecoveryRequired(e.to_string()))?
                .meta_store()
                .effects_stamp()
        })
        .await
        .map_err(|e| crate::storage::StorageError::RecoveryRequired(e.to_string()))?
    }

    async fn create_memory(
        &self,
        namespace_id: NamespaceId,
        summary: &str,
        full_text: Option<&str>,
        tags: &[String],
        embedding: &[f32],
        initial_stability: Option<f32>,
        created_at: Option<i64>,
    ) -> Result<CachedRecord, crate::storage::StorageError> {
        use crate::model::Tag;
        use crate::model::record::DiskRecord;

        let id = MemoryId::new();
        let now = self.clock.now_millis();
        let ts = created_at.unwrap_or(now);
        // Over-long tags cannot reach this point: the shared request
        // validator bounds every tag and reserves the `entity/` /
        // `topic/` / `emotion/` prefix out of the label limits. A drop
        // here therefore means a tag outside `Tag`'s alphabet, and it
        // is logged rather than swallowed.
        let parsed_tags: Vec<Tag> = crate::model::parse_tags_lossy("create_memory.tags", tags);

        let mut record = DiskRecord {
            schema: None,
            version: DiskRecord::CURRENT_VERSION,
            id: *id.as_bytes(),
            namespace_id: namespace_id.get(),
            created_at: ts,
            last_accessed_at: ts,
            phase: DecayPhase::Full,
            strength: 1.0,
            decay_strength: 1.0,
            stability: initial_stability.unwrap_or(3.7145),
            difficulty: 5.0,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: summary.to_string(),
            tags: parsed_tags,
            access_history: Vec::new(),
            text_offset: 0,
            text_length: 0,
        };

        let storage = self.storage.clone();
        let embedding_vec = embedding.to_vec();
        let full_text_owned = full_text.map(|s| s.to_string());

        let record = tokio::task::spawn_blocking(move || {
            let mut storage_w = storage.write().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_w.insert_memory(
                id,
                namespace_id,
                &mut record,
                &embedding_vec,
                full_text_owned.as_deref(),
            )?;
            Ok::<_, crate::storage::StorageError>(record)
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })??;

        let cached = CachedRecord::from(&record);
        self.cache.insert(id, cached.clone()).await;
        Ok(cached)
    }

    async fn get_record(&self, id: MemoryId) -> Option<DiskRecord> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().ok()?;
            storage_r.get_record(id).ok().flatten()
        })
        .await
        .ok()
        .flatten()
    }

    async fn delete_memory(&self, id: MemoryId) -> Result<bool, crate::storage::StorageError> {
        let storage = self.storage.clone();

        // Perform read, tombstone check, tombstone, and free_vector_slot
        // atomically under a single write lock to prevent TOCTOU races.
        // Without this, two concurrent deletes could both read the record
        // as non-tombstoned, both tombstone it, and both free the same
        // vector slot -- corrupting the free list into a self-referential
        // cycle.
        // Returns the PRE-tombstone record, not a bool: `tombstone`
        // erases the tag list, and the tag list is where the entity
        // names live. A bool here would leave the entity index with no
        // way to know what to unlink.
        let deleted = tokio::task::spawn_blocking(move || {
            let mut storage_w = storage.write().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;

            // Check if the record exists and is not already tombstoned.
            let existing_record = match storage_w.get_record(id)? {
                Some(r) => r,
                None => {
                    return Ok::<Option<crate::model::DiskRecord>, crate::storage::StorageError>(
                        None,
                    );
                }
            };

            if existing_record.phase == DecayPhase::Tombstone {
                return Ok(None);
            }

            // Tombstone the record.
            storage_w.tombstone_memory(id)?;

            // Free the vector slot.
            let ns_id = NamespaceId::new(existing_record.namespace_id);
            if let Err(e) = storage_w.free_vector_slot(ns_id, existing_record.vector_slot) {
                tracing::warn!(
                    memory_id = %id,
                    vector_slot = existing_record.vector_slot,
                    %e,
                    "vector slot free failed (non-fatal)"
                );
            }

            Ok(Some(existing_record))
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })??;

        let Some(record) = deleted else {
            return Ok(false);
        };

        // This used to stop at `cache.invalidate`, so an HTTP-deleted
        // memory kept its vector-index entry (burning a top-k slot in
        // every search and reading as a live autolink target), kept its
        // FTS row — which survives restart, because the FTS index is
        // only rebuilt when it is empty — and kept a graph node
        // reporting Full phase at strength 1.0, still propping up its
        // neighbours' decay resistance. Same helper as the MCP path now,
        // so the two cannot drift apart again.
        crate::index_cleanup::purge_from_indexes(
            &crate::index_cleanup::IndexHandles {
                cache: &self.cache,
                vector_index: &self.vector_index,
                fts_index: &self.fts_index,
                entity_index: &self.entity_index,
                graph: &self.graph,
            },
            std::slice::from_ref(&(id, record)),
            crate::index_cleanup::FtsAction::PerRecord,
            crate::index_cleanup::GraphAction::MarkTombstoned,
        )
        .await;

        Ok(true)
    }

    async fn namespace_stats(
        &self,
        namespace_id: NamespaceId,
    ) -> Result<state::NamespaceStats, crate::storage::StorageError> {
        let storage = self.storage.clone();

        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;

            let all_records = storage_r.scan_all()?;

            let mut memory_count: u64 = 0;
            let mut tombstone_count: u64 = 0;
            let mut phase_1_count: u64 = 0;
            let mut phase_2_count: u64 = 0;
            let mut phase_3_count: u64 = 0;
            let mut permastore_count: u64 = 0;
            let mut strength_sum: f64 = 0.0;
            let mut edge_count: u64 = 0;

            for (_id, record) in &all_records {
                if NamespaceId::new(record.namespace_id) != namespace_id {
                    continue;
                }

                // Mirrors the MCP path exactly: a tombstoned record is not
                // a memory, and counting it dragged avg_strength toward
                // zero while still contributing its (unzeroed) permastore
                // flag and edge count.
                if record.phase == DecayPhase::Tombstone {
                    tombstone_count += 1;
                    continue;
                }
                memory_count += 1;

                match record.phase {
                    DecayPhase::Full => phase_1_count += 1,
                    DecayPhase::Summary => phase_2_count += 1,
                    DecayPhase::Ghost => phase_3_count += 1,
                    // Unreachable: skipped above. An arm, not an
                    // `unreachable!()` -- this is a read path.
                    DecayPhase::Tombstone => {}
                }

                if record.is_permastore != 0 {
                    permastore_count += 1;
                }

                strength_sum += record.decay_strength as f64;
                edge_count += record.edge_count as u64;
            }

            let avg_strength = if memory_count > 0 {
                (strength_sum / memory_count as f64) as f32
            } else {
                0.0
            };

            Ok(state::NamespaceStats {
                memory_count,
                tombstone_count,
                phase_1_count,
                phase_2_count,
                phase_3_count,
                permastore_count,
                avg_strength,
                edge_count,
            })
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn list_memories(
        &self,
        filter: &crate::api::models::ListFilter,
    ) -> Result<Vec<CachedRecord>, crate::storage::StorageError> {
        let storage = self.storage.clone();
        let filter = filter.clone();

        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;

            let all_records = storage_r.scan_all()?;

            let filtered: Vec<CachedRecord> = all_records
                .into_iter()
                .filter(|(_id, record)| {
                    if record.phase == DecayPhase::Tombstone {
                        return false;
                    }
                    if let Some(ns_id) = filter.namespace_id {
                        if NamespaceId::new(record.namespace_id) != ns_id {
                            return false;
                        }
                    }
                    if let Some(phase) = filter.phase {
                        if record.phase != phase {
                            return false;
                        }
                    }
                    if !filter.tags.is_empty()
                        && !filter
                            .tags
                            .iter()
                            .all(|tag| record.tags.iter().any(|t| t.as_str() == tag))
                    {
                        return false;
                    }
                    if let Some(start) = filter.time_range_start {
                        if record.created_at < start {
                            return false;
                        }
                    }
                    if let Some(end) = filter.time_range_end {
                        if record.created_at > end {
                            return false;
                        }
                    }
                    true
                })
                .map(|(_id, disk_record)| CachedRecord::from(&disk_record))
                .collect();

            Ok(filtered)
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn get_full_text(
        &self,
        id: MemoryId,
    ) -> Result<Option<String>, crate::storage::StorageError> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            let record = match storage_r.get_record(id)? {
                Some(r) => r,
                None => return Ok(None),
            };
            if record.text_length == 0 {
                return Ok(None);
            }
            let text_ref = crate::storage::TextRef {
                file_offset: record.text_offset,
                length: record.text_length,
            };
            storage_r.get_text(text_ref)
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn list_memories_filtered(
        &self,
        namespace_id: NamespaceId,
        require_tags: &[Tag],
        time_range_start: Option<i64>,
        time_range_end: Option<i64>,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<(MemoryId, DiskRecord)>, u64), crate::storage::StorageError> {
        let storage = self.storage.clone();
        let tags_owned: Vec<Tag> = require_tags.to_vec();

        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_r.meta_store().list_memories_filtered(
                namespace_id,
                &tags_owned,
                time_range_start,
                time_range_end,
                offset,
                limit,
            )
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn ping(&self) -> bool {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || storage.read().is_ok())
            .await
            .unwrap_or(false)
    }

    async fn scan_all(&self) -> Result<Vec<(MemoryId, DiskRecord)>, crate::storage::StorageError> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_r.scan_all()
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn scan_phase_records(
        &self,
        phase: DecayPhase,
    ) -> Result<Vec<(MemoryId, DiskRecord)>, crate::storage::StorageError> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_r.scan_phase_records(phase)
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    async fn list_tags(&self) -> Result<Vec<(String, u64)>, crate::storage::StorageError> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let storage_r = storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            storage_r.list_tags()
        })
        .await
        .map_err(|e| {
            crate::storage::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            ))
        })?
    }

    fn storage_path(&self) -> Result<PathBuf, crate::storage::StorageError> {
        tokio::task::block_in_place(|| {
            let storage_r = self.storage.read().map_err(|e| {
                crate::storage::StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                ))
            })?;
            Ok(storage_r.db_path().to_path_buf())
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RecordCacheAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `CacheManager` to the API `RecordCache` trait.
pub struct RecordCacheAdapter {
    cache: Arc<CacheManager>,
}

impl RecordCacheAdapter {
    /// Creates a new adapter wrapping the cache manager.
    pub fn new(cache: Arc<CacheManager>) -> Self {
        Self { cache }
    }
}

#[async_trait]
impl state::RecordCache for RecordCacheAdapter {
    async fn get_or_load(
        &self,
        id: MemoryId,
        storage: &dyn state::StorageEngine,
    ) -> Option<Arc<CachedRecord>> {
        // Use the injected storage trait for loading on cache miss.
        // First check the cache directly for a fast-path hit.
        if let Some(arc) = self.cache.get(id).await {
            return Some(arc);
        }

        // Cache miss: load from the injected storage engine.
        match storage.get_record(id).await {
            Some(disk) => {
                let record = CachedRecord::from(&disk);
                self.cache.insert(id, record.clone()).await;
                // Re-fetch to get the Arc-wrapped version from moka.
                self.cache.get(id).await
            }
            None => None,
        }
    }

    async fn insert(&self, record: &CachedRecord) {
        self.cache.insert(record.id, record.clone()).await;
    }

    async fn remove(&self, id: MemoryId) {
        self.cache.invalidate(id).await;
    }

    fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    fn hit_rate(&self) -> f64 {
        // moka does not expose hit rate directly; return 0.0.
        0.0
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RelationshipGraphAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `SharedGraph` to the API `RelationshipGraph` trait.
pub struct RelationshipGraphAdapter {
    graph: SharedGraph,
    vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
    entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: Arc<CacheManager>,
    config: Arc<RecalldConfig>,
}

impl RelationshipGraphAdapter {
    /// Creates a new adapter wrapping the shared relationship graph and
    /// subsystems needed for post-creation linking.
    pub fn new(
        graph: SharedGraph,
        vector_index: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
        entity_index: Arc<tokio::sync::RwLock<EntityIndex>>,
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        cache: Arc<CacheManager>,
        config: Arc<RecalldConfig>,
    ) -> Self {
        Self {
            graph,
            vector_index,
            entity_index,
            storage,
            cache,
            config,
        }
    }
}

#[async_trait]
impl state::RelationshipGraph for RelationshipGraphAdapter {
    async fn check_link_target(
        &self,
        field: &'static str,
        new_id: Option<MemoryId>,
        target: MemoryId,
        expected_ns: NamespaceId,
    ) -> Result<(), crate::graph::LinkError> {
        crate::graph::check_link_target(
            field,
            new_id,
            target,
            expected_ns,
            &self.graph,
            &self.storage,
            &self.cache,
        )
        .await
    }

    async fn apply_supersedes_edge(
        &self,
        new_id: MemoryId,
        target: MemoryId,
    ) -> crate::model::SupersedesOutcome {
        crate::graph::apply_supersedes_edge(new_id, target, &self.graph, &self.storage, &self.cache)
            .await
    }

    async fn apply_parent_edge(&self, parent: MemoryId, child: MemoryId) {
        crate::graph::apply_parent_edge(parent, child, &self.graph, &self.storage, &self.cache)
            .await;
    }

    async fn tombstone_node(&self, id: MemoryId) -> Result<(), crate::graph::GraphError> {
        let mut graph = self.graph.write().await;
        graph.update_node_state(id, DecayPhase::Tombstone, 0.0)?;
        Ok(())
    }

    async fn add_node(
        &self,
        id: MemoryId,
        namespace_id: NamespaceId,
        phase: DecayPhase,
        strength: f32,
        vector_slot: u32,
    ) -> Result<(), crate::graph::GraphError> {
        let mut graph = self.graph.write().await;
        let _ = graph.add_node(id, namespace_id, phase, strength, vector_slot);
        Ok(())
    }

    async fn perform_post_creation_links(
        &self,
        memory_id: MemoryId,
        namespace_id: NamespaceId,
        embedding: &[f32],
        tags: &[String],
        entities: &[String],
        created_at: i64,
    ) {
        if !self.config.graph.autolink_enabled {
            return;
        }

        let threshold = self.config.graph.auto_link_threshold as f32;
        let max_links = self.config.graph.max_auto_links;

        // Autolink: discover and create edges to similar existing memories.
        if let Err(e) = crate::graph::perform_autolink(
            memory_id,
            namespace_id,
            embedding,
            tags,
            threshold,
            max_links,
            &self.vector_index,
            &self.graph,
            &self.storage,
            &self.cache,
        )
        .await
        {
            tracing::warn!(
                memory_id = %memory_id,
                %e,
                "autolink failed (non-fatal)"
            );
        }

        // Entity linking: connect memories sharing named entities.
        if !entities.is_empty() {
            let max_entity_links = self.config.graph.max_entity_links;
            let _ = crate::graph::perform_entity_link(
                memory_id,
                namespace_id,
                entities,
                max_entity_links,
                &self.entity_index,
                &self.graph,
                &self.storage,
                &self.cache,
            )
            .await;
        }

        // Temporal linking: connect to recently-stored memories.
        let temporal_window_ms = self.config.graph.temporal_window_ms;
        let max_temporal_links = self.config.graph.max_temporal_links;
        if temporal_window_ms > 0 {
            let recent_memories: Vec<(MemoryId, i64)> = self
                .cache
                .iter()
                .filter_map(|(mid, record)| {
                    if mid == memory_id {
                        return None;
                    }
                    if record.namespace_id != namespace_id {
                        return None;
                    }
                    let delta = (created_at - record.created_at).unsigned_abs();
                    if delta <= temporal_window_ms {
                        Some((mid, record.created_at))
                    } else {
                        None
                    }
                })
                .collect();

            if !recent_memories.is_empty() {
                if let Err(e) = crate::graph::perform_temporal_link(
                    memory_id,
                    namespace_id,
                    created_at,
                    temporal_window_ms,
                    max_temporal_links,
                    &recent_memories,
                    &self.graph,
                    &self.storage,
                    &self.cache,
                )
                .await
                {
                    tracing::warn!(
                        memory_id = %memory_id,
                        %e,
                        "temporal link failed (non-fatal)"
                    );
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// FsrsEngineAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts the decay subsystem to the API `FsrsEngine` trait.
pub struct FsrsEngineAdapter {
    completion: Option<crate::derived_completion::DerivedCompletion>,
    clock: Arc<dyn crate::time::Clock>,
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
    cache: Arc<CacheManager>,
    graph: crate::graph::SharedGraph,
    config: Arc<RecalldConfig>,
    sweep_alive: bool,
}

impl FsrsEngineAdapter {
    /// Creates a new adapter with the storage engine, cache, and sweep-thread liveness flag.
    pub fn new(
        storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
        cache: Arc<CacheManager>,
        graph: crate::graph::SharedGraph,
        config: Arc<RecalldConfig>,
        has_sweep: bool,
    ) -> Self {
        cache.bind_storage(&storage);
        Self {
            clock: Arc::new(crate::time::SystemClock),
            storage,
            cache,
            graph,
            config,
            sweep_alive: has_sweep,
            completion: None,
        }
    }
    /// Attach the live derived views so manual sweeps complete committed effects.
    pub fn with_indexes(
        mut self,
        vectors: Arc<tokio::sync::RwLock<FlatVectorIndex>>,
        fts: Arc<tokio::sync::Mutex<FtsIndex>>,
        entities: Arc<tokio::sync::RwLock<EntityIndex>>,
    ) -> Self {
        self.completion = Some(crate::derived_completion::DerivedCompletion {
            storage: self.storage.clone(),
            cache: self.cache.clone(),
            graph: self.graph.clone(),
            vectors,
            fts,
            entities,
        });
        self
    }

    /// Use the system instance's clock (or an isolated manual clock).
    pub fn with_clock(mut self, clock: Arc<dyn crate::time::Clock>) -> Self {
        self.clock = clock;
        self
    }
}

#[async_trait]
impl state::FsrsEngine for FsrsEngineAdapter {
    async fn record_access(&self, id: MemoryId, kind: AccessKind) {
        let now = self.clock.now_millis();
        let storage = self.storage.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(storage_w) = storage.write() {
                if let Ok(Some(record)) = storage_w.get_record(id) {
                    if record.phase != DecayPhase::Tombstone {
                        let timestamp = now.max(record.created_at).max(record.last_accessed_at);
                        let _ = storage_w.update_access(id, timestamp, kind);
                    }
                }
            }
        })
        .await;
    }

    async fn reinforce(
        &self,
        id: MemoryId,
        quality: u8,
    ) -> Result<state::ReinforceResult, Box<dyn std::error::Error + Send + Sync>> {
        let outcome = crate::decay::storage_adapter::reinforce(
            self.storage.clone(),
            id,
            quality,
            self.config.decay.decay_rate_multiplier,
            self.clock.now_millis(),
        )
        .await;
        // The first storage write may have committed even if the second failed.
        self.cache.invalidate(id).await;
        let plan = outcome?;
        Ok(state::ReinforceResult {
            strength: plan.strength,
            stability: plan.stability,
            phase: plan.phase,
            is_permastore: plan.is_permastore,
        })
    }

    fn sweep_thread_alive(&self) -> bool {
        self.sweep_alive
    }

    fn last_sweep_time(&self) -> Option<std::time::Instant> {
        None
    }

    async fn trigger_sweep(
        &self,
        as_of_millis: Option<i64>,
    ) -> Result<crate::decay::SweepResult, Box<dyn std::error::Error + Send + Sync>> {
        use crate::decay::sweep::{DecaySweepRunner, SweepConfig};
        use crate::graph::ActivationConfig;

        let sweep_config = SweepConfig::default();
        let decay_config = crate::decay::DecayConfig::default();
        let activation_config = ActivationConfig::default();
        let global_multiplier = self.config.decay.decay_rate_multiplier as f64;

        let result = DecaySweepRunner::execute_sweep_at(
            &sweep_config,
            &decay_config,
            &activation_config,
            &self.storage,
            &self.graph,
            &self.cache,
            global_multiplier,
            Some(as_of_millis.unwrap_or_else(|| self.clock.now_millis())),
        )
        .await;

        if let Some(completion) = &self.completion {
            completion.complete().await?;
        } else if self
            .storage
            .read()
            .map_err(|e| e.to_string())?
            .meta_store()
            .needs_effects_rebuild()
        {
            return Err("derived completion handles missing; reopen required".into());
        }
        Ok(result)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// NamespaceRegistryAdapter
// ═══════════════════════════════════════════════════════════════════════

/// Adapts `RedbStorageEngine` to the API `NamespaceRegistry` trait.
pub struct NamespaceRegistryAdapter {
    storage: Arc<std::sync::RwLock<RedbStorageEngine>>,
}

impl NamespaceRegistryAdapter {
    /// Creates a new adapter wrapping the storage engine for namespace operations.
    pub fn new(storage: Arc<std::sync::RwLock<RedbStorageEngine>>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl state::NamespaceRegistry for NamespaceRegistryAdapter {
    fn resolve(&self, name: &str) -> Option<NamespaceConfig> {
        tokio::task::block_in_place(|| {
            let storage_r = self.storage.read().ok()?;
            storage_r.get_namespace_by_name(name).ok().flatten()
        })
    }

    fn get_by_id(&self, id: u32) -> Option<NamespaceConfig> {
        tokio::task::block_in_place(|| {
            let storage_r = self.storage.read().ok()?;
            storage_r.get_namespace(NamespaceId::new(id)).ok().flatten()
        })
    }

    fn name_for(&self, id: NamespaceId) -> Option<String> {
        tokio::task::block_in_place(|| {
            let storage_r = self.storage.read().ok()?;
            storage_r
                .get_namespace(id)
                .ok()
                .flatten()
                .map(|ns| ns.name.clone())
        })
    }

    async fn list_all(&self) -> Vec<state::NamespaceListInfo> {
        let storage = self.storage.clone();

        tokio::task::spawn_blocking(move || {
            let storage_r = match storage.read() {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };

            let namespaces = match storage_r.list_namespaces() {
                Ok(ns) => ns,
                Err(_) => return Vec::new(),
            };

            // Live memories only. Counting tombstones here made every
            // namespace listing disagree with what a caller could actually
            // fetch from it.
            let mut counts = std::collections::HashMap::<u32, u64>::new();
            if let Ok(all_records) = storage_r.scan_all() {
                for (_id, record) in &all_records {
                    if record.phase == DecayPhase::Tombstone {
                        continue;
                    }
                    *counts.entry(record.namespace_id).or_default() += 1;
                }
            }

            namespaces
                .into_iter()
                .map(|ns| {
                    let memory_count = counts.get(&ns.id.get()).copied().unwrap_or(0);
                    state::NamespaceListInfo {
                        id: ns.id.get(),
                        name: ns.name.clone(),
                        embedding_dim: ns.embedding_dim,
                        memory_count,
                        created_at: ns.created_at,
                    }
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    async fn create(
        &self,
        name: &str,
        embedding_dim: Option<u32>,
        initial_stability: f32,
        desired_retention: f32,
        decay_rate_multiplier: Option<f32>,
    ) -> Result<NamespaceConfig, Box<dyn std::error::Error + Send + Sync>> {
        let now = chrono::Utc::now().timestamp_millis();
        let storage = self.storage.clone();
        let name_owned = name.to_string();

        tokio::task::spawn_blocking(move || {
            // Resolve embedding_dim: use provided value, or inherit from
            // the 'default' namespace, or fall back to 1536.
            let dim = embedding_dim.unwrap_or_else(|| {
                storage
                    .read()
                    .ok()
                    .and_then(|s| s.get_namespace_by_name("default").ok().flatten())
                    .map(|ns| ns.embedding_dim)
                    .unwrap_or(1536)
            });

            let ns_config = NamespaceConfig {
                schema: None,
                id: NamespaceId::UNSET,
                name: name_owned,
                embedding_dim: dim,
                initial_stability,
                default_difficulty: 5.0,
                phase_thresholds: crate::model::namespace::PhaseThresholds::default(),
                permastore_threshold: 1500.0,
                created_at: now,
                desired_retention,
                decay_rate_multiplier,
            };

            crate::decay::lifecycle::resolve_policy(
                &ns_config,
                &crate::decay::DecayConfig::default(),
                1.0,
            )?;

            let mut storage_w = storage.write().map_err(|e| {
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("storage lock poisoned: {e}"),
                )) as Box<dyn std::error::Error + Send + Sync>
            })?;
            let assigned_id = storage_w.create_namespace(&ns_config)?;
            Ok(NamespaceConfig {
                schema: None,
                id: assigned_id,
                ..ns_config
            })
        })
        .await
        .map_err(|e| {
            Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("blocking task join error: {e}"),
            )) as Box<dyn std::error::Error + Send + Sync>
        })?
    }
}

// ═══════════════════════════════════════════════════════════════════════
// NoopMetricsCollector
// ═══════════════════════════════════════════════════════════════════════

/// No-op metrics collector until a real metrics module is implemented.
pub struct NoopMetricsCollector;

#[async_trait]
impl state::MetricsCollector for NoopMetricsCollector {
    async fn render_prometheus(&self) -> String {
        String::new()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::state::StorageEngine as _;
    use crate::search::{VectorIndex as _, VectorMetadata};
    use crate::test_support::{DIM, Fixture, MemorySpec};

    /// A vector of the fixture's width, so it matches the index.
    fn unit_vector() -> Vec<f32> {
        let mut v = vec![0.0; DIM];
        v[0] = 1.0;
        v
    }

    fn adapter(fx: &Fixture) -> StorageEngineAdapter {
        StorageEngineAdapter::new(
            fx.storage.clone(),
            fx.cache.clone(),
            fx.vector_index.clone(),
            fx.fts_index.clone(),
            fx.entity_index.clone(),
            fx.graph.clone(),
        )
    }

    /// Put a memory into every index, which is the state the running
    /// system leaves after a successful store.
    async fn indexed_memory(fx: &Fixture) -> MemoryId {
        let ns = NamespaceId::new(1);
        let id = fx
            .insert_memory_with(MemorySpec {
                tags: &["entity/sarah"],
                ..MemorySpec::new(ns)
            })
            .await;

        fx.fts_index
            .lock()
            .await
            .add(ns, id, "a fact about Sarah", None, &[])
            .expect("fts add");
        fx.vector_index
            .write()
            .await
            .add(
                id,
                &unit_vector(),
                VectorMetadata {
                    namespace_id: ns,
                    decay_phase: 1,
                    tags: Vec::new(),
                },
            )
            .expect("vector add");
        fx.entity_index
            .write()
            .await
            .add(ns, id, &["Sarah".to_string()]);

        id
    }

    /// THE HTTP-delete regression. This path used to stop after
    /// `cache.invalidate`, leaving the memory in all four indexes: a
    /// vector entry burning a top-k slot in every search and reading as
    /// a live autolink target, an FTS row that survives restart forever,
    /// an entity entry, and a graph node still reporting Full phase at
    /// strength 1.0 and so still propping up its neighbours' decay
    /// resistance.
    #[tokio::test]
    async fn w35_http_delete_clears_every_index_and_marks_the_graph_node() {
        let fx = Fixture::new();
        let id = indexed_memory(&fx).await;

        assert!(adapter(&fx).delete_memory(id).await.expect("delete"));

        assert!(
            fx.fts_index.lock().await.is_empty().expect("fts"),
            "FTS row survived an HTTP delete"
        );
        assert_eq!(
            fx.vector_index.read().await.len(),
            0,
            "vector index entry survived an HTTP delete"
        );
        assert_eq!(
            fx.entity_index.read().await.len(),
            0,
            "entity index entry survived an HTTP delete"
        );
        assert_eq!(
            fx.graph
                .read()
                .await
                .get_node(&id)
                .expect("the node is kept, only re-phased")
                .decay_phase,
            DecayPhase::Tombstone
        );
    }

    /// A second delete is a no-op, not a second round of index churn.
    #[tokio::test]
    async fn w36_http_delete_of_an_already_tombstoned_memory_reports_false() {
        let fx = Fixture::new();
        let id = indexed_memory(&fx).await;
        let adapter = adapter(&fx);

        assert!(adapter.delete_memory(id).await.expect("first delete"));

        // Re-index it, so a second pass through the cleanup would be
        // visible rather than idempotent by accident.
        fx.fts_index
            .lock()
            .await
            .add(NamespaceId::new(1), id, "re-indexed", None, &[])
            .expect("fts add");

        assert!(
            !adapter.delete_memory(id).await.expect("second delete"),
            "an already-tombstoned memory must report false"
        );
        assert!(
            !fx.fts_index.lock().await.is_empty().expect("fts"),
            "the second delete ran the index cleanup anyway"
        );
    }
    #[tokio::test]
    async fn creation_adapters_reject_unusable_lifecycle_policy_before_persistence() {
        use crate::api::state::NamespaceRegistry as _;
        use crate::mcp::bridge::{BridgeError, CreateNamespaceInput, NamespaceRegistry as _};
        let f = Fixture::new();
        let http = NamespaceRegistryAdapter::new(f.storage.clone());
        let mcp = f.namespace_adapter();
        for (stability, multiplier) in [
            (0.0, None),
            (-1.0, None),
            (f32::NAN, None),
            (f32::INFINITY, None),
            (1.0, Some(-1.0)),
            (1.0, Some(f32::NAN)),
            (1.0, Some(f32::INFINITY)),
        ] {
            let error = http
                .create("invalid-http", Some(DIM as u32), stability, 0.9, multiplier)
                .await
                .unwrap_err();
            assert!(matches!(
                error.downcast_ref::<crate::decay::lifecycle::LifecycleError>(),
                Some(crate::decay::lifecycle::LifecycleError::InvalidInput(_))
            ));
            let error = mcp
                .create_namespace(CreateNamespaceInput {
                    name: "invalid-mcp".into(),
                    embedding_dim: Some(DIM as u16),
                    initial_stability: Some(stability),
                    desired_retention: None,
                    decay_rate_multiplier: multiplier,
                })
                .await
                .unwrap_err();
            assert!(matches!(error, BridgeError::InvalidInput(_)));
        }
        assert!(
            f.storage
                .read()
                .unwrap()
                .get_namespace_by_name("invalid-http")
                .unwrap()
                .is_none()
        );
        assert!(
            f.storage
                .read()
                .unwrap()
                .get_namespace_by_name("invalid-mcp")
                .unwrap()
                .is_none()
        );
        // Zero decay is valid, zero stability is not; inherited settings stay valid.
        for multiplier in [None, Some(0.0)] {
            let name = if multiplier.is_none() {
                "inherited"
            } else {
                "disabled"
            };
            let ns = http
                .create(name, Some(DIM as u32), 1.0, 0.9, multiplier)
                .await
                .unwrap();
            crate::decay::lifecycle::resolve_policy(
                &ns,
                &crate::decay::DecayConfig::default(),
                1.0,
            )
            .unwrap();
        }
    }
}

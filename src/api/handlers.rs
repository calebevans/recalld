//! Request handler functions for all API endpoints.
//!
//! Each handler is an `async fn` that receives axum extractors and
//! returns `Result<impl IntoResponse, AppError>`. All handlers follow
//! the same pattern: extract, validate, delegate to subsystem, convert
//! result, respond.

use std::time::Instant;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use tokio::sync::RwLock;
use uuid::Uuid;

use super::errors::AppError;
use super::models::*;
use super::state::{AppState, QueryInput, SearchQuery};
use crate::health::report as health_report_compute;
use crate::model::DecayPhase;
use crate::model::constants::{
    EMOTION_TAG_PREFIX, ENTITY_TAG_PREFIX, MAX_BATCH_MEMORIES, TOPIC_TAG_PREFIX,
};
use crate::model::id::{MemoryId, NamespaceId};
use crate::model::memory::AccessKind;
use crate::model::validation::{MemoryInputRef, validate_memory_input, validate_namespace_name};
use crate::serialization::{
    ApiResponse, MemoryResponse, NamespaceRequest, NamespaceResponse, SearchHit, SearchRequest,
    SearchResponse,
};

/// Duration for which a cached health report is considered fresh.
const HEALTH_REPORT_CACHE_TTL_SECS: u64 = 60;

/// Per-scope cached health reports.
///
/// Uses a lazily-initialized global to avoid threading the cache through
/// `AppState` (which would require a schema change beyond the scope of
/// this fix). The `RwLock` ensures concurrent readers don't block each
/// other and only one writer recomputes at a time.
fn health_report_cache()
-> &'static RwLock<std::collections::HashMap<String, (Instant, HealthReport)>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<RwLock<std::collections::HashMap<String, (Instant, HealthReport)>>> =
        OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(std::collections::HashMap::new()))
}

/// Build the near-duplicate advisory for a memory about to be created.
///
/// Shared by the single and batch create endpoints so the two HTTP doors
/// cannot drift, and mirroring `McpStorageAdapter::store_memory` so the
/// HTTP and MCP doors cannot either. `MemoryResponse.supersedes` exists
/// for exactly that reason.
///
/// **Call this BEFORE the memory is indexed.** `state.search.index_memory`
/// puts the new vector in the index; run after that and the memory matches
/// itself at score 1.0.
///
/// Never fails a create: every error path inside
/// [`find_near_duplicates`](crate::search::find_near_duplicates) yields no
/// matches.
///
/// The Full/Summary guarantee is enforced HERE, not by the
/// `decay_phases` filter `find_near_duplicates` hands the vector index:
/// `FlatVectorIndex` writes each entry's `decay_phase` once at add time
/// (always Full) and `VectorIndex::update_metadata` has no callers, so
/// that filter is inert in production. A candidate whose record is
/// missing or past Summary is dropped here instead.
async fn near_duplicate_report(
    state: &AppState,
    embedding: &[f32],
    namespace_id: NamespaceId,
    check_duplicates: Option<bool>,
) -> Option<crate::model::NearDuplicateReport> {
    use crate::model::constants::{
        DUPLICATE_SIMILARITY_THRESHOLD, MAX_NEAR_DUPLICATE_MATCHES,
        NEAR_DUPLICATE_SUMMARY_MAX_BYTES,
    };
    use crate::model::{DecayPhase, NearDuplicateMatch, NearDuplicateReport, truncate_summary};

    if !check_duplicates.unwrap_or(true) {
        return None;
    }

    let candidates = state
        .search
        .near_duplicates(
            embedding,
            namespace_id,
            DUPLICATE_SIMILARITY_THRESHOLD as f32,
            MAX_NEAR_DUPLICATE_MATCHES,
        )
        .await;

    let mut matches = Vec::with_capacity(candidates.len());
    for (id, score) in candidates {
        // `get_or_load` reads through the cache without recording an
        // access — the endpoints that count a read call `record_access`
        // separately afterwards. Loading a summary to warn about a
        // duplicate is not a retrieval of that memory.
        let Some(record) = state.cache.get_or_load(id, state.storage.as_ref()).await else {
            // Hard-deleted, and the vector index has not caught up.
            continue;
        };
        // Ghost has no summary left to show and Tombstone is deleted;
        // neither is something to be told it duplicates.
        if !matches!(record.phase, DecayPhase::Full | DecayPhase::Summary) {
            continue;
        }
        matches.push(NearDuplicateMatch {
            id,
            score,
            summary: truncate_summary(&record.summary, NEAR_DUPLICATE_SUMMARY_MAX_BYTES),
        });
    }

    NearDuplicateReport::from_matches(DUPLICATE_SIMILARITY_THRESHOLD, matches)
}

// ═══════════════════════════════════════════════════════════════════════
// Memory CRUD
// ═══════════════════════════════════════════════════════════════════════

/// POST /memories -- create a new memory.
///
/// Steps:
/// 1. Validate request content limits (see [`validate_memory_input`]);
///    all length limits are UTF-8 bytes, not characters.
/// 2. Resolve namespace by name -> NamespaceId.
/// 3. Validate `parent_id` / `supersedes` targets, BEFORE any write, so a
///    bad link returns 422 without leaving an orphaned memory behind.
/// 4. Generate embedding if not provided (calls embedding provider).
/// 5. Validate embedding dimensionality against namespace config.
/// 6. Persist to storage (meta.db, fulltext.dat, vectors.dat).
/// 7. Insert into RAM cache and vector index.
/// 8. Create the `parent_id` and `supersedes` edges in the graph.
/// 9. Return 201 with the created memory.
pub async fn create_memory(
    State(state): State<AppState>,
    Json(req): Json<CreateMemoryApiRequest>,
) -> Result<(StatusCode, Json<ApiResponse<MemoryResponse>>), AppError> {
    let start = Instant::now();

    // --- Validation ---
    validate_memory_input(MemoryInputRef {
        summary: &req.summary,
        full_text: req.full_text.as_deref(),
        tags: &req.tags,
        entities: &req.entities,
        topics: &req.topics,
        emotions: &req.emotions,
    })?;

    // --- Resolve namespace ---
    let ns = state
        .namespaces
        .resolve(&req.namespace)
        .ok_or_else(|| AppError::NotFound {
            resource: "namespace",
            id: req.namespace.clone(),
        })?;

    // --- Validate caller-specified links BEFORE anything is written ---
    //
    // The memory is persisted further down; a link failure discovered after
    // that would leave an orphaned record behind a 4xx (which is exactly
    // what the parent edge used to do). Checking here also spares an
    // embedding round-trip on a request that cannot succeed.
    //
    // `None` for the new ID: it does not exist until `create_memory`
    // returns, so the self-reference check has nothing to compare against.
    for (field, target) in [("parentId", req.parent_id), ("supersedes", req.supersedes)] {
        if let Some(target) = target {
            state
                .graph
                .check_link_target(field, None, target, ns.id)
                .await?;
        }
    }

    // --- Merge entities/topics/emotions into tags (Issue 8) ---
    // The prefixes come from `constants` because the label length limits
    // reserve room for exactly these bytes.
    let mut merged_tags = req.tags.clone();
    for entity in &req.entities {
        let tag = format!("{ENTITY_TAG_PREFIX}{}", entity.to_lowercase());
        if !merged_tags.contains(&tag) {
            merged_tags.push(tag);
        }
    }
    for topic in &req.topics {
        let tag = format!("{TOPIC_TAG_PREFIX}{}", topic.to_lowercase());
        if !merged_tags.contains(&tag) {
            merged_tags.push(tag);
        }
    }
    for emotion in &req.emotions {
        let tag = format!("{EMOTION_TAG_PREFIX}{}", emotion.to_lowercase());
        if !merged_tags.contains(&tag) {
            merged_tags.push(tag);
        }
    }

    // --- Embedding (Issue 9: embed summary + full_text + tags) ---
    let embedding = match req.embedding {
        Some(ref vec) => {
            if vec.len() != ns.embedding_dim as usize {
                return Err(AppError::UnprocessableEntity {
                    message: format!(
                        "embedding has {} dimensions, namespace '{}' requires {}",
                        vec.len(),
                        ns.name,
                        ns.embedding_dim
                    ),
                    field: Some("embedding".into()),
                });
            }
            vec.clone()
        }
        None => {
            // Build embedding text from summary + full_text + tags (matches MCP)
            let mut embed_text = match &req.full_text {
                Some(ft) => format!("{}\n\n{}", req.summary, ft),
                None => req.summary.clone(),
            };
            if !merged_tags.is_empty() {
                embed_text = format!("{} {}", embed_text, merged_tags.join(" "));
            }
            state.search.embed_text(&embed_text, ns.id).await?
        }
    };

    // --- Near-duplicate advisory ---
    // Before the write, so the new memory is not yet in the vector index
    // and cannot match itself. See `near_duplicate_report`.
    let near_duplicates =
        near_duplicate_report(&state, &embedding, ns.id, req.check_duplicates).await;

    // --- Persist ---
    // Resolve initial stability: use caller-provided value, or fall back
    // to the namespace's configured initial_stability (matching the MCP
    // reference implementation).
    let resolved_stability = req.initial_stability.unwrap_or(ns.initial_stability);
    let memory = state
        .storage
        .create_memory(
            ns.id,
            &req.summary,
            req.full_text.as_deref(),
            &merged_tags,
            &embedding,
            Some(resolved_stability),
            req.created_at,
        )
        .await?;

    // --- Cache + vector index ---
    state.cache.insert(&memory).await;
    state
        .search
        .index_memory(memory.id, &embedding, ns.id)
        .await;

    // --- FTS5 indexing (Issue 10) ---
    state
        .search
        .fts_add(
            ns.id,
            memory.id,
            &req.summary,
            req.full_text.as_deref(),
            &merged_tags,
        )
        .await;

    // --- Entity index (Issue 10) ---
    state
        .search
        .entity_index_add(memory.id, &req.entities)
        .await;

    // --- Graph node (Issue 11: must happen before edges) ---
    let _ = state
        .graph
        .add_node(
            memory.id,
            ns.id,
            crate::model::DecayPhase::Full,
            1.0,
            memory.vector_slot,
        )
        .await;

    // --- Parent edge ---
    // Both explicit edges were validated before the record was written, so
    // neither can fail the request from here — which is the point. A `?` at
    // this line would return a 4xx with the memory already created. Both go
    // through the shared `explicit_links` helpers, so an HTTP-created parent
    // edge counts toward `edge_count` exactly as an MCP-created one does.
    if let Some(parent_id) = req.parent_id {
        state.graph.apply_parent_edge(parent_id, memory.id).await;
    }

    // --- Supersedes edge (Issue 8) ---
    let supersedes_outcome = match req.supersedes {
        Some(old_id) => Some(state.graph.apply_supersedes_edge(memory.id, old_id).await),
        None => None,
    };

    // --- Autolink, entity-link, temporal-link (Issue 12) ---
    let created_at = req
        .created_at
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    state
        .graph
        .perform_post_creation_links(
            memory.id,
            ns.id,
            &embedding,
            &merged_tags,
            &req.entities,
            created_at,
        )
        .await;

    let mut response = MemoryResponse::from_cached(&memory, ns.name.clone());
    response.full_text = req.full_text;
    response.supersedes = supersedes_outcome;
    response.near_duplicates = near_duplicates;
    let took = start.elapsed().as_micros() as u64;

    Ok((
        StatusCode::CREATED,
        Json(ApiResponse {
            data: response,
            took_us: Some(took),
        }),
    ))
}

/// GET /memories/:id -- retrieve a single memory.
///
/// Steps:
/// 1. Parse UUID from path.
/// 2. Look up in cache (hit) or load from storage (miss).
/// 3. Record an access event (`DirectRetrieval`).
/// 4. Return 200 with memory, or 404 if not found.
pub async fn get_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(options): Query<GetMemoryQuery>,
) -> Result<Json<ApiResponse<MemoryResponse>>, AppError> {
    let start = Instant::now();

    let memory_id = parse_memory_id(&id)?;

    let record = if options.record_access {
        state
            .cache
            .get_or_load(memory_id, state.storage.as_ref())
            .await
    } else {
        state
            .storage
            .get_record(memory_id)
            .await
            .map(|r| std::sync::Arc::new(crate::model::CachedRecord::from(&r)))
    }
    .ok_or_else(|| AppError::NotFound {
        resource: "memory",
        id: id.clone(),
    })?;

    let ns_name = state
        .namespaces
        .name_for(record.namespace_id)
        .unwrap_or_else(|| "unknown".to_string());

    let mut response = MemoryResponse::from_cached(&record, ns_name);
    response.full_text = state.storage.get_full_text(memory_id).await?;
    // Record access for decay tracking
    if options.record_access && record.phase != DecayPhase::Tombstone {
        state
            .decay
            .record_access(memory_id, AccessKind::DirectRetrieval)
            .await?;
    }

    let took = start.elapsed().as_micros() as u64;

    Ok(Json(ApiResponse {
        data: response,
        took_us: Some(took),
    }))
}

/// DELETE /memories/:id -- delete a memory.
///
/// Steps:
/// 1. Parse UUID from path.
/// 2. Remove from storage (meta.db, fulltext.dat pointer, vectors.dat slot).
/// 3. Remove from cache and vector index.
/// 4. Clean up graph edges (both directions).
/// 5. Return 200 with deletion confirmation.
pub async fn delete_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DeleteResponse>>, AppError> {
    let start = Instant::now();

    let memory_id = parse_memory_id(&id)?;

    // Read record first for entity index cleanup (Issue 14)
    let existing_record = state.storage.get_record(memory_id).await;

    let existed = state.storage.delete_memory(memory_id).await?;
    if existed {
        state.cache.remove(memory_id).await;
        state.search.remove_from_index(memory_id).await;

        // Issue 14: Clean up FTS5 index
        state.search.fts_remove(memory_id).await;

        // Issue 14: Clean up entity index
        if let Some(ref record) = existing_record {
            let metadata = crate::model::parse_structured_tags(&record.tags);
            state
                .search
                .entity_index_remove(memory_id, &metadata.entities)
                .await;
        }

        // Issue 13: Tombstone the graph node instead of removing it
        if let Err(e) = state.graph.tombstone_node(memory_id).await {
            tracing::warn!(
                memory_id = %memory_id,
                %e,
                "graph tombstone failed (non-fatal)"
            );
        }
    }

    let took = start.elapsed().as_micros() as u64;

    Ok(Json(ApiResponse {
        data: DeleteResponse {
            id: memory_id,
            deleted: existed,
        },
        took_us: Some(took),
    }))
}

/// POST /memories/:id/reinforce -- manual reinforcement.
///
/// Steps:
/// 1. Parse UUID from path.
/// 2. Verify memory exists.
/// 3. Apply FSRS reinforcement with the given quality rating.
/// 4. Update storage and cache with new strength/stability.
/// 5. Return updated decay parameters.
pub async fn reinforce_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ReinforceRequest>,
) -> Result<Json<ApiResponse<ReinforceResponse>>, AppError> {
    let start = Instant::now();

    let memory_id = parse_memory_id(&id)?;

    // Validate quality rating
    if !(1..=4).contains(&req.quality) {
        return Err(AppError::BadRequest {
            message: "quality must be 1-4 (again/hard/good/easy)".into(),
            field: Some("quality".into()),
        });
    }

    let updated = state
        .decay
        .reinforce(memory_id, req.quality)
        .await
        .map_err(|e| feedback_error(e, &id))?;

    let took = start.elapsed().as_micros() as u64;

    Ok(Json(ApiResponse {
        data: ReinforceResponse {
            id: memory_id,
            strength: updated.strength,
            stability: updated.stability,
            phase: format!("{:?}", updated.phase),
            is_permastore: updated.is_permastore,
        },
        took_us: Some(took),
    }))
}

/// Preserve typed lifecycle outcomes instead of classifying display strings.
fn feedback_error(error: Box<dyn std::error::Error + Send + Sync>, id: &str) -> AppError {
    use crate::decay::lifecycle::LifecycleError;
    match error.downcast_ref::<LifecycleError>() {
        Some(LifecycleError::Observation(error)) => AppError::Observation(error.clone()),
        Some(LifecycleError::ContentUnavailable) => AppError::Conflict {
            message: error.to_string(),
        },
        Some(LifecycleError::NotFound) => AppError::NotFound {
            resource: "memory",
            id: id.into(),
        },
        Some(LifecycleError::InvalidInput(_)) => AppError::BadRequest {
            message: error.to_string(),
            field: None,
        },
        _ => AppError::Internal { source: error },
    }
}

/// GET /memories -- list memories with filtering and pagination.
///
/// Steps:
/// 1. Parse and validate query parameters.
/// 2. Resolve namespace if specified.
/// 3. Request filtered records from storage adapter.
/// 4. Sort results in memory.
/// 5. Apply pagination (offset + limit).
/// 6. Convert to MemoryResponse objects.
/// 7. Return ListMemoriesResponse with pagination metadata.
pub async fn list_memories(
    State(state): State<AppState>,
    Query(params): Query<ListMemoriesQuery>,
) -> Result<Json<ApiResponse<ListMemoriesResponse>>, AppError> {
    let start = Instant::now();

    // Validate limit (cap at 1000)
    let limit = params.limit.unwrap_or(50).min(1000) as usize;
    let offset = params.offset.unwrap_or(0) as usize;

    // Validate phase
    if let Some(ref phase) = params.phase {
        if !["full", "summary", "ghost"].contains(&phase.as_str()) {
            return Err(AppError::BadRequest {
                message: format!("invalid phase '{phase}', must be: full, summary, ghost"),
                field: Some("phase".into()),
            });
        }
    }

    // Validate sort field
    let sort_field = params.sort.as_deref().unwrap_or("created");
    if !["created", "accessed", "strength", "stability"].contains(&sort_field) {
        return Err(AppError::BadRequest {
            message: format!(
                "invalid sort field '{sort_field}', must be: created, accessed, strength, stability"
            ),
            field: Some("sort".into()),
        });
    }

    // Validate order
    let order = params.order.as_deref().unwrap_or("desc");
    if !["asc", "desc"].contains(&order) {
        return Err(AppError::BadRequest {
            message: format!("invalid order '{order}', must be: asc, desc"),
            field: Some("order".into()),
        });
    }

    // Resolve namespace if specified
    let namespace_id = if let Some(ref ns_name) = params.namespace {
        let ns = state
            .namespaces
            .resolve(ns_name)
            .ok_or_else(|| AppError::NotFound {
                resource: "namespace",
                id: ns_name.clone(),
            })?;
        Some(ns.id)
    } else {
        None
    };

    // Issue 19: Use list_memories_filtered() for server-side pagination when namespace is given.
    // Build require_tags from tags + entities (Issue 21).
    let mut require_tags: Vec<crate::model::Tag> =
        crate::model::parse_tags_lossy("list_memories.tags", &params.tags);
    for entity in &params.entities {
        if let Ok(tag) = crate::model::Tag::new(&format!("entity/{}", entity.to_lowercase())) {
            require_tags.push(tag);
        }
    }

    if let Some(ns_id) = namespace_id {
        // Server-side filtered pagination (Issue 19)
        let (page, total) = state
            .storage
            .list_memories_filtered(
                ns_id,
                &require_tags,
                params.time_range_start,
                params.time_range_end,
                offset,
                limit,
            )
            .await?;

        let mut memories: Vec<MemoryResponse> = Vec::with_capacity(page.len());
        for (mid, record) in page {
            let ns_name = state
                .namespaces
                .name_for(NamespaceId::new(record.namespace_id))
                .unwrap_or_else(|| "unknown".to_string());
            let cached = crate::model::CachedRecord::from(&record);
            let mut mem = MemoryResponse::from_cached(&cached, ns_name);
            mem.full_text = state.storage.get_full_text(mid).await.unwrap_or(None);
            memories.push(mem);
        }

        let has_more = (offset + memories.len()) < total as usize;

        let response = ListMemoriesResponse {
            memories,
            total,
            limit: limit as u32,
            offset: offset as u32,
            has_more,
        };

        let took = start.elapsed().as_micros() as u64;

        return Ok(Json(ApiResponse {
            data: response,
            took_us: Some(took),
        }));
    }

    // Fallback: no namespace specified, use in-memory filtering
    let phase_filter = params.phase.as_ref().map(|p| match p.as_str() {
        "full" => crate::model::DecayPhase::Full,
        "summary" => crate::model::DecayPhase::Summary,
        "ghost" => crate::model::DecayPhase::Ghost,
        _ => crate::model::DecayPhase::Full,
    });

    let filter = ListFilter {
        namespace_id,
        phase: phase_filter,
        tags: {
            let mut all_tags = params.tags;
            for entity in &params.entities {
                all_tags.push(format!("entity/{}", entity.to_lowercase()));
            }
            all_tags
        },
        time_range_start: params.time_range_start,
        time_range_end: params.time_range_end,
    };

    let mut records = state.storage.list_memories(&filter).await?;

    match (sort_field, order) {
        ("created", "asc") => records.sort_by_key(|r| r.created_at),
        ("created", "desc") => {
            records.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        }
        ("accessed", "asc") => records.sort_by_key(|r| r.last_accessed_at),
        ("accessed", "desc") => {
            records.sort_by_key(|r| std::cmp::Reverse(r.last_accessed_at));
        }
        ("strength", "asc") => records.sort_by(|a, b| {
            a.decay_strength
                .partial_cmp(&b.decay_strength)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ("strength", "desc") => records.sort_by(|a, b| {
            b.decay_strength
                .partial_cmp(&a.decay_strength)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ("stability", "asc") => records.sort_by(|a, b| {
            a.stability
                .partial_cmp(&b.stability)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ("stability", "desc") => records.sort_by(|a, b| {
            b.stability
                .partial_cmp(&a.stability)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        _ => {}
    }

    let total = records.len() as u64;

    let page_records: Vec<_> = records.into_iter().skip(offset).take(limit).collect();

    let mut memories: Vec<MemoryResponse> = Vec::with_capacity(page_records.len());
    for record in page_records {
        let ns_name = state
            .namespaces
            .name_for(record.namespace_id)
            .unwrap_or_else(|| "unknown".to_string());
        let mut mem = MemoryResponse::from_cached(&record, ns_name);
        mem.full_text = state.storage.get_full_text(record.id).await.unwrap_or(None);
        memories.push(mem);
    }

    let has_more = (offset + memories.len()) < total as usize;

    let response = ListMemoriesResponse {
        memories,
        total,
        limit: limit as u32,
        offset: offset as u32,
        has_more,
    };

    let took = start.elapsed().as_micros() as u64;

    Ok(Json(ApiResponse {
        data: response,
        took_us: Some(took),
    }))
}

/// Helper: parse a UUID string from a path segment.
fn parse_memory_id(s: &str) -> Result<MemoryId, AppError> {
    Uuid::parse_str(s)
        .map(MemoryId::from)
        .map_err(|_| AppError::BadRequest {
            message: format!("invalid UUID: '{s}'"),
            field: Some("id".into()),
        })
}

// ═══════════════════════════════════════════════════════════════════════
// Search Handlers
// ═══════════════════════════════════════════════════════════════════════

/// POST /search -- multi-modal search.
///
/// Steps:
/// 1. Validate: at least one of `query` or `embedding` must be provided
///    (unless tag-only metadata search).
/// 2. Resolve namespace.
/// 3. Build `SearchQuery` from request fields.
/// 4. Delegate to `SearchPipeline::search()`.
/// 5. Convert results to `SearchHit`s.
/// 6. Return `SearchResponse` with scores and timing.
pub async fn search_memories(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, AppError> {
    let start = Instant::now();

    // Validate: need at least one search signal
    if req.query.is_none() && req.embedding.is_none() && req.tags.is_empty() {
        return Err(AppError::BadRequest {
            message: "at least one of 'query', 'embedding', or 'tags' must be provided".into(),
            field: None,
        });
    }

    // Validate: query and embedding are mutually exclusive
    if req.query.is_some() && req.embedding.is_some() {
        return Err(AppError::UnprocessableEntity {
            message: "'query' and 'embedding' are mutually exclusive".into(),
            field: Some("embedding".into()),
        });
    }

    // Validate limit
    let limit = req.limit.min(100) as usize;

    // Resolve namespace
    let ns = state
        .namespaces
        .resolve(&req.namespace)
        .ok_or_else(|| AppError::NotFound {
            resource: "namespace",
            id: req.namespace.clone(),
        })?;

    // Validate embedding dimensions if provided
    if let Some(ref emb) = req.embedding {
        if emb.len() != ns.embedding_dim as usize {
            return Err(AppError::UnprocessableEntity {
                message: format!(
                    "embedding has {} dimensions, namespace '{}' requires {}",
                    emb.len(),
                    ns.name,
                    ns.embedding_dim
                ),
                field: Some("embedding".into()),
            });
        }
    }

    // Merge entities/topics/emotions into tag filters (matching create_memory behaviour)
    let mut include_tags = req.tags.clone();
    for topic in &req.topics {
        let tag = format!("topic/{}", topic.to_lowercase());
        if !include_tags.contains(&tag) {
            include_tags.push(tag);
        }
    }
    for emotion in &req.emotions {
        let tag = format!("emotion/{}", emotion.to_lowercase());
        if !include_tags.contains(&tag) {
            include_tags.push(tag);
        }
    }

    let query_input = if let Some(ref text) = req.query {
        Some(QueryInput::Text(text.clone()))
    } else {
        req.embedding
            .as_ref()
            .map(|vec| QueryInput::Vector(vec.clone()))
    };

    let search_query = SearchQuery {
        record_access: false,
        query: query_input.unwrap_or(QueryInput::Text(String::new())),
        namespace_id: ns.id,
        namespace_name: ns.name.clone(),
        k: limit,
        include_tags,
        exclude_tags: vec![],
        decay_phases: None,
        min_score: req.min_strength,
        graph_depth: req.depth as usize,
        apply_rif: true,
        time_range_start: req.time_range_start,
        time_range_end: req.time_range_end,
        entities: req.entities.clone(),
    };

    let results = state.search.search(search_query).await?;

    // Batch-load embeddings with a single lock acquisition if requested.
    let embeddings = if req.include_embeddings {
        let ids: Vec<MemoryId> = results.iter().map(|r| r.memory.id).collect();
        state.search.get_embeddings_batch(&ids)
    } else {
        std::collections::HashMap::new()
    };

    // Convert to API response, loading full_text from storage
    let mut hits: Vec<SearchHit> = Vec::with_capacity(results.len());
    let mut exposures = Vec::with_capacity(results.len());
    for r in results {
        exposures.push((r.memory.id, r.access_kind));
        let ns_name = state
            .namespaces
            .name_for(r.memory.namespace_id)
            .unwrap_or_else(|| "unknown".to_string());
        let mut mem = MemoryResponse::from_cached(&r.memory, ns_name);
        mem.full_text = state.storage.get_full_text(r.memory.id).await?;
        if req.include_embeddings {
            mem.embedding = embeddings.get(&r.memory.id).cloned();
        }
        hits.push(SearchHit {
            memory: mem,
            score: r.score,
        });
    }

    if req.record_access {
        state.search.record_exposures(&exposures).await?;
    }

    let total = hits.len() as u64;
    let took = start.elapsed().as_micros() as u64;

    Ok(Json(SearchResponse {
        hits,
        total,
        took_us: Some(took),
    }))
}

/// POST /similar/:id -- find memories similar to an existing memory.
///
/// Steps:
/// 1. Parse source memory UUID.
/// 2. Load source memory's embedding from the vector index.
/// 3. Execute vector search using that embedding as the query.
/// 4. Exclude the source memory from results.
/// 5. Return scored results.
pub async fn find_similar(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<FindSimilarRequest>,
) -> Result<Json<SearchResponse>, AppError> {
    let start = Instant::now();

    let memory_id = parse_memory_id(&id)?;

    // Selection and hydration are read-only. Record the final exposed set only
    // after all fallible hydration, never the source or filtered-out matches.
    let results = state
        .search
        .find_similar(memory_id, req.limit.min(100) as usize, req.min_score, false)
        .await?;
    let ids: Vec<_> = results.iter().map(|r| r.memory_id).collect();
    let embeddings = if req.include_embeddings {
        state.search.get_embeddings_batch(&ids)
    } else {
        std::collections::HashMap::new()
    };
    let mut hits = Vec::with_capacity(results.len());
    for result in results {
        let Some(record) = state.storage.get_record(result.memory_id).await else {
            continue;
        };
        if matches!(record.phase, DecayPhase::Ghost | DecayPhase::Tombstone) {
            continue;
        }
        let ns_name = state
            .namespaces
            .name_for(NamespaceId::new(record.namespace_id))
            .unwrap_or_else(|| "unknown".into());
        let cached = crate::model::CachedRecord::from(&record);
        let mut memory = MemoryResponse::from_cached(&cached, ns_name);
        memory.full_text = state.storage.get_full_text(result.memory_id).await?;
        if req.include_embeddings {
            memory.embedding = embeddings.get(&result.memory_id).cloned();
        }
        hits.push(SearchHit {
            memory,
            score: result.score.unwrap_or_default(),
        });
    }
    if req.record_access {
        let ids: Vec<_> = hits.iter().map(|hit| hit.memory.id).collect();
        state.search.record_similar_accesses(&ids).await?;
    }

    let total = hits.len() as u64;
    let took = start.elapsed().as_micros() as u64;

    Ok(Json(SearchResponse {
        hits,
        total,
        took_us: Some(took),
    }))
}

// ═══════════════════════════════════════════════════════════════════════
// Namespace Handlers
// ═══════════════════════════════════════════════════════════════════════

/// GET /namespaces -- list all namespaces.
pub async fn list_namespaces(
    State(state): State<AppState>,
) -> Result<Json<ApiResponse<Vec<NamespaceListItem>>>, AppError> {
    let start = Instant::now();

    let namespaces = state.namespaces.list_all().await;

    let items: Vec<NamespaceListItem> = namespaces
        .into_iter()
        .map(|ns| NamespaceListItem {
            id: ns.id,
            name: ns.name,
            embedding_dim: ns.embedding_dim,
            memory_count: ns.memory_count,
            created_at: ns.created_at,
        })
        .collect();

    let took = start.elapsed().as_micros() as u64;

    Ok(Json(ApiResponse {
        data: items,
        took_us: Some(took),
    }))
}

/// POST /namespaces -- create a new namespace.
///
/// Steps:
/// 1. Validate name format (1-64 UTF-8 bytes, ASCII alphanumeric +
///    hyphens + underscores).
/// 2. Check for duplicate name.
/// 3. Register namespace with fixed embedding dimensionality.
/// 4. Return 201 with namespace details.
pub async fn create_namespace(
    State(state): State<AppState>,
    Json(req): Json<NamespaceRequest>,
) -> Result<(StatusCode, Json<ApiResponse<NamespaceResponse>>), AppError> {
    let start = Instant::now();

    // Validate name format
    validate_namespace_name(&req.name)?;

    // Validate embedding dimensions if provided
    if let Some(dim) = req.embedding_dim {
        if dim == 0 || dim > 8192 {
            return Err(AppError::BadRequest {
                message: "embedding_dim must be between 1 and 8192".into(),
                field: Some("embeddingDim".into()),
            });
        }
    }

    // Create namespace (registry checks for duplicates)
    let ns = state
        .namespaces
        .create(
            &req.name,
            req.embedding_dim,
            req.initial_stability,
            req.desired_retention,
            req.decay_rate_multiplier,
        )
        .await
        .map_err(|e| {
            if matches!(
                e.downcast_ref::<crate::decay::lifecycle::LifecycleError>(),
                Some(crate::decay::lifecycle::LifecycleError::InvalidInput(_))
            ) {
                AppError::BadRequest {
                    message: e.to_string(),
                    field: None,
                }
            } else {
                AppError::Conflict {
                    message: format!("namespace '{}' already exists", req.name),
                }
            }
        })?;

    let response = NamespaceResponse {
        id: ns.id.get(),
        name: ns.name,
        embedding_dim: ns.embedding_dim,
        initial_stability: ns.initial_stability,
        default_difficulty: ns.default_difficulty,
        permastore_threshold: ns.permastore_threshold,
        desired_retention: ns.desired_retention,
        created_at: ns.created_at,
        memory_count: 0,
    };

    let took = start.elapsed().as_micros() as u64;

    Ok((
        StatusCode::CREATED,
        Json(ApiResponse {
            data: response,
            took_us: Some(took),
        }),
    ))
}

/// GET /namespaces/:id/stats -- namespace statistics.
///
/// Accepts either namespace name (string) or integer ID.
pub async fn namespace_stats(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<NamespaceStatsResponse>>, AppError> {
    let start = Instant::now();

    // Try parsing as integer ID first, then fall back to name lookup
    let ns = if let Ok(numeric_id) = id.parse::<u32>() {
        state.namespaces.get_by_id(numeric_id)
    } else {
        state.namespaces.resolve(&id)
    }
    .ok_or_else(|| AppError::NotFound {
        resource: "namespace",
        id: id.clone(),
    })?;

    let stats = state.storage.namespace_stats(ns.id).await?;

    let response = NamespaceStatsResponse {
        name: ns.name,
        id: ns.id.get(),
        memory_count: stats.memory_count,
        tombstone_count: stats.tombstone_count,
        phase_counts: PhaseCounts {
            full: stats.phase_1_count,
            summary: stats.phase_2_count,
            ghost: stats.phase_3_count,
        },
        permastore_count: stats.permastore_count,
        avg_strength: stats.avg_strength,
        edge_count: stats.edge_count,
        embedding_dim: ns.embedding_dim,
        vector_bytes: stats.memory_count * ns.embedding_dim as u64 * 4,
    };

    let took = start.elapsed().as_micros() as u64;

    Ok(Json(ApiResponse {
        data: response,
        took_us: Some(took),
    }))
}

// ═══════════════════════════════════════════════════════════════════════
// Operational Handlers
// ═══════════════════════════════════════════════════════════════════════

/// GET /health -- health check with subsystem status.
///
/// Returns 200 with status `"healthy"` if all subsystems report up.
/// Returns 200 with status `"degraded"` if non-critical subsystems are
/// down (e.g. embedding provider). Returns 503 if critical subsystems
/// are down (storage, cache).
pub async fn health_check(State(state): State<AppState>) -> Result<Json<HealthResponse>, AppError> {
    let uptime = state.started_at.elapsed().as_secs();

    let storage_health = probe_component("storage", || async { state.storage.ping().await }).await;

    let cache_health = ComponentHealth {
        status: "up".to_string(),
        message: Some(format!(
            "entries: {}, hit_rate: {:.1}%",
            state.cache.entry_count(),
            state.cache.hit_rate() * 100.0
        )),
        latency_us: None,
    };

    let vector_health = ComponentHealth {
        status: "up".to_string(),
        message: Some(format!("indexed: {}", state.search.indexed_count())),
        latency_us: None,
    };

    let embedding_health = probe_component("embedding", || async {
        state.search.embedding_provider_healthy().await
    })
    .await;

    let decay_health = ComponentHealth {
        status: if state.decay.sweep_thread_alive() {
            "up"
        } else {
            "down"
        }
        .to_string(),
        message: state
            .decay
            .last_sweep_time()
            .map(|t| format!("last sweep: {}s ago", t.elapsed().as_secs())),
        latency_us: None,
    };

    let subsystems = SubsystemHealth {
        storage: storage_health,
        cache: cache_health,
        vector_index: vector_health,
        embedding: embedding_health,
        decay: decay_health,
    };

    // Determine overall status
    let status = if subsystems.storage.status == "down" || subsystems.cache.status == "down" {
        HealthStatus::Unhealthy
    } else if subsystems.embedding.status == "down" || subsystems.decay.status == "down" {
        HealthStatus::Degraded
    } else {
        HealthStatus::Healthy
    };

    Ok(Json(HealthResponse {
        status,
        uptime_secs: uptime,
        subsystems,
    }))
}

/// Helper: probe a component and measure latency.
async fn probe_component<F, Fut>(name: &str, f: F) -> ComponentHealth
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let probe_start = Instant::now();
    let is_up = f().await;
    let latency = probe_start.elapsed().as_micros() as u64;

    ComponentHealth {
        status: if is_up { "up" } else { "down" }.to_string(),
        message: if !is_up {
            Some(format!("{name} health check failed"))
        } else {
            None
        },
        latency_us: Some(latency),
    }
}

/// GET /health/report -- comprehensive decay health report.
///
/// Computes decay forecast, at-risk memories, storage breakdown,
/// age distribution, and tag statistics. Optionally scoped to a
/// single namespace via `?namespace=<name>`.
///
/// Results are cached for 60 seconds per scope key to avoid repeated
/// `scan_all()` calls that load the entire database into memory.
pub async fn health_report(
    State(state): State<AppState>,
    Query(params): Query<HealthReportQuery>,
) -> Result<Json<ApiResponse<HealthReport>>, AppError> {
    let start = Instant::now();

    // Resolve optional namespace filter (validate before cache check)
    let namespace_filter: Option<NamespaceId> = if let Some(ref ns_name) = params.namespace {
        Some(
            state
                .namespaces
                .resolve(ns_name)
                .ok_or_else(|| AppError::NotFound {
                    resource: "namespace",
                    id: ns_name.clone(),
                })?
                .id,
        )
    } else {
        None
    };

    let scope = params.namespace.unwrap_or_else(|| "all".to_string());

    // --- Check cache ---
    {
        let cache = health_report_cache().read().await;
        if let Some((generated_at, cached_report)) = cache.get(&scope) {
            if generated_at.elapsed().as_secs() < HEALTH_REPORT_CACHE_TTL_SECS {
                let took = start.elapsed().as_micros() as u64;
                return Ok(Json(ApiResponse {
                    data: cached_report.clone(),
                    took_us: Some(took),
                }));
            }
        }
    }

    // --- Cache miss: compute the report ---
    let all_records = state.storage.scan_all().await?;

    // Filter records by namespace if applicable
    let filtered: Vec<_> = all_records
        .iter()
        .filter(|(_, r)| {
            if let Some(ns_id) = namespace_filter {
                r.namespace_id == ns_id.get()
            } else {
                true
            }
        })
        .collect();

    let overview = health_report_compute::compute_overview(&filtered);
    let decay_forecast =
        health_report_compute::compute_decay_forecast(&filtered, state.namespaces.as_ref());
    let at_risk = health_report_compute::compute_at_risk(&filtered, state.namespaces.as_ref());
    let age_distribution = health_report_compute::compute_age_distribution(&filtered);
    let storage = health_report_compute::compute_storage_breakdown(
        state.storage.as_ref(),
        state.namespaces.as_ref(),
        namespace_filter,
    )
    .await;
    let metadata =
        health_report_compute::compute_metadata_stats(state.storage.as_ref(), namespace_filter)
            .await
            .map_err(|e| AppError::Internal { source: e })?;

    let report = HealthReport {
        scope: scope.clone(),
        overview,
        decay_forecast,
        at_risk,
        age_distribution,
        storage,
        metadata,
    };

    // --- Store in cache ---
    {
        let mut cache = health_report_cache().write().await;
        cache.insert(scope, (Instant::now(), report.clone()));
    }

    let took = start.elapsed().as_micros() as u64;

    Ok(Json(ApiResponse {
        data: report,
        took_us: Some(took),
    }))
}

/// POST /memories/batch -- batch create memories (Issue 16).
///
/// Creates multiple memories in sequence. Each memory goes through the
/// same validation and creation pipeline as the single-create endpoint.
pub async fn batch_store(
    State(state): State<AppState>,
    Json(req): Json<BatchStoreRequest>,
) -> Result<(StatusCode, Json<ApiResponse<BatchStoreResponse>>), AppError> {
    let start = Instant::now();

    if req.memories.is_empty() {
        return Err(AppError::BadRequest {
            message: "memories array must not be empty".into(),
            field: Some("memories".into()),
        });
    }

    if req.memories.len() > MAX_BATCH_MEMORIES {
        return Err(AppError::BadRequest {
            message: format!("batch size exceeds the maximum of {MAX_BATCH_MEMORIES}"),
            field: Some("memories".into()),
        });
    }

    let mut created = Vec::with_capacity(req.memories.len());
    let mut failed: Vec<BatchStoreFailure> = Vec::new();

    for (index, mem_req) in req.memories.into_iter().enumerate() {
        // A bad item skips rather than failing the whole batch, but it no
        // longer skips silently: every `continue` below records why. The
        // limits themselves are the shared ones, which keeps an oversized
        // summary from reaching the u16-prefixed on-disk record encoder.
        if let Err(e) = validate_memory_input(MemoryInputRef {
            summary: &mem_req.summary,
            full_text: mem_req.full_text.as_deref(),
            tags: &mem_req.tags,
            entities: &mem_req.entities,
            topics: &mem_req.topics,
            emotions: &mem_req.emotions,
        }) {
            failed.push(BatchStoreFailure {
                index,
                error: e.to_string(),
                field: e.field().map(str::to_string),
            });
            continue;
        }

        let ns = match state.namespaces.resolve(&mem_req.namespace) {
            Some(ns) => ns,
            None => {
                failed.push(BatchStoreFailure {
                    index,
                    error: format!("namespace '{}' not found", mem_req.namespace),
                    field: Some("namespace".into()),
                });
                continue;
            }
        };

        // Validate caller-specified links before creating anything, so a
        // rejected link leaves no orphaned memory behind.
        let mut link_error: Option<crate::graph::LinkError> = None;
        for (field, target) in [
            ("parentId", mem_req.parent_id),
            ("supersedes", mem_req.supersedes),
        ] {
            if let Some(target) = target {
                if let Err(e) = state
                    .graph
                    .check_link_target(field, None, target, ns.id)
                    .await
                {
                    link_error = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = link_error {
            failed.push(BatchStoreFailure {
                index,
                error: e.to_string(),
                field: Some(e.field().into()),
            });
            continue;
        }

        // Merge entities/topics/emotions into tags
        let mut merged_tags = mem_req.tags.clone();
        for entity in &mem_req.entities {
            let tag = format!("{ENTITY_TAG_PREFIX}{}", entity.to_lowercase());
            if !merged_tags.contains(&tag) {
                merged_tags.push(tag);
            }
        }
        for topic in &mem_req.topics {
            let tag = format!("{TOPIC_TAG_PREFIX}{}", topic.to_lowercase());
            if !merged_tags.contains(&tag) {
                merged_tags.push(tag);
            }
        }
        for emotion in &mem_req.emotions {
            let tag = format!("{EMOTION_TAG_PREFIX}{}", emotion.to_lowercase());
            if !merged_tags.contains(&tag) {
                merged_tags.push(tag);
            }
        }

        // Embedding
        let embedding = match mem_req.embedding {
            Some(ref vec) => {
                if vec.len() != ns.embedding_dim as usize {
                    failed.push(BatchStoreFailure {
                        index,
                        error: format!(
                            "embedding has {} dimensions, namespace '{}' requires {}",
                            vec.len(),
                            ns.name,
                            ns.embedding_dim
                        ),
                        field: Some("embedding".into()),
                    });
                    continue;
                }
                vec.clone()
            }
            None => {
                let mut embed_text = match &mem_req.full_text {
                    Some(ft) => format!("{}\n\n{}", mem_req.summary, ft),
                    None => mem_req.summary.clone(),
                };
                if !merged_tags.is_empty() {
                    embed_text = format!("{} {}", embed_text, merged_tags.join(" "));
                }
                match state.search.embed_text(&embed_text, ns.id).await {
                    Ok(emb) => emb,
                    Err(e) => {
                        failed.push(BatchStoreFailure {
                            index,
                            error: format!("failed to generate embedding: {e}"),
                            field: None,
                        });
                        continue;
                    }
                }
            }
        };

        // Near-duplicate advisory, before this item is indexed. Items are
        // created in order and each is indexed before the next is checked,
        // so an intra-batch duplicate is reported too — same property the
        // MCP batch tool has.
        let near_duplicates =
            near_duplicate_report(&state, &embedding, ns.id, mem_req.check_duplicates).await;

        // Persist — resolve initial stability from namespace config (matching MCP)
        let resolved_stability = mem_req.initial_stability.unwrap_or(ns.initial_stability);
        let memory = match state
            .storage
            .create_memory(
                ns.id,
                &mem_req.summary,
                mem_req.full_text.as_deref(),
                &merged_tags,
                &embedding,
                Some(resolved_stability),
                mem_req.created_at,
            )
            .await
        {
            Ok(m) => m,
            Err(e) => {
                failed.push(BatchStoreFailure {
                    index,
                    error: e.to_string(),
                    field: None,
                });
                continue;
            }
        };

        // Cache + index
        state.cache.insert(&memory).await;
        state
            .search
            .index_memory(memory.id, &embedding, ns.id)
            .await;

        // FTS
        state
            .search
            .fts_add(
                ns.id,
                memory.id,
                &mem_req.summary,
                mem_req.full_text.as_deref(),
                &merged_tags,
            )
            .await;

        // Entity index
        state
            .search
            .entity_index_add(memory.id, &mem_req.entities)
            .await;

        // Graph node
        let _ = state
            .graph
            .add_node(
                memory.id,
                ns.id,
                crate::model::DecayPhase::Full,
                1.0,
                memory.vector_slot,
            )
            .await;

        // Explicit links. Both targets were validated before this item was
        // created, so failures here are durability details, not rejections.
        // Both use the shared `explicit_links` helpers, so `edge_count` moves
        // the same way it does on the single-store and MCP paths.
        if let Some(parent_id) = mem_req.parent_id {
            state.graph.apply_parent_edge(parent_id, memory.id).await;
        }

        let supersedes_outcome = match mem_req.supersedes {
            Some(old_id) => Some(state.graph.apply_supersedes_edge(memory.id, old_id).await),
            None => None,
        };

        // Post-creation links
        let created_at = mem_req
            .created_at
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        state
            .graph
            .perform_post_creation_links(
                memory.id,
                ns.id,
                &embedding,
                &merged_tags,
                &mem_req.entities,
                created_at,
            )
            .await;

        created.push(BatchStoreResult {
            id: memory.id,
            namespace: ns.name.clone(),
            supersedes: supersedes_outcome,
            near_duplicates,
        });
    }

    let total = created.len() as u64;
    let took = start.elapsed().as_micros() as u64;

    Ok((
        StatusCode::CREATED,
        Json(ApiResponse {
            data: BatchStoreResponse {
                created,
                total,
                failed,
            },
            took_us: Some(took),
        }),
    ))
}

/// POST /namespaces/:name/duplicates -- scan for duplicate memories (Issue 16).
///
/// Uses vector similarity to find clusters of near-duplicate memories
/// within a namespace.
pub async fn scan_duplicates(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<ScanDuplicatesRequest>,
) -> Result<Json<ApiResponse<ScanDuplicatesResponse>>, AppError> {
    let start = Instant::now();

    let ns = state
        .namespaces
        .resolve(&name)
        .ok_or_else(|| AppError::NotFound {
            resource: "namespace",
            id: name.clone(),
        })?;

    let threshold = req.threshold.clamp(0.0, 1.0);
    let max_memories = req.max_memories.min(10_000);

    // Get memory IDs in this namespace
    let filter = ListFilter {
        namespace_id: Some(ns.id),
        phase: None,
        tags: vec![],
        time_range_start: None,
        time_range_end: None,
    };
    let records = state.storage.list_memories(&filter).await?;
    let memory_ids: Vec<MemoryId> = records.iter().take(max_memories).map(|r| r.id).collect();

    if memory_ids.len() < 2 {
        let took = start.elapsed().as_micros() as u64;
        return Ok(Json(ApiResponse {
            data: ScanDuplicatesResponse {
                clusters: vec![],
                total: 0,
            },
            took_us: Some(took),
        }));
    }

    // Load embeddings
    let embeddings = state.search.get_embeddings_batch(&memory_ids);
    let id_embeddings: Vec<(MemoryId, Vec<f32>)> = memory_ids
        .into_iter()
        .filter_map(|mid| embeddings.get(&mid).map(|emb| (mid, emb.clone())))
        .collect();

    if id_embeddings.len() < 2 {
        let took = start.elapsed().as_micros() as u64;
        return Ok(Json(ApiResponse {
            data: ScanDuplicatesResponse {
                clusters: vec![],
                total: 0,
            },
            took_us: Some(took),
        }));
    }

    // Union-Find based clustering
    let n = id_embeddings.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }

    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[rb] = ra;
        }
    }

    let mut max_sim: std::collections::HashMap<usize, f32> = std::collections::HashMap::new();

    // Compare each memory's embedding against its nearest neighbors
    for (i, (mid_i, _emb_i)) in id_embeddings.iter().enumerate() {
        // Find similar via the search pipeline's get_embedding + manual cosine
        if let Some(emb) = state.search.get_embedding(*mid_i) {
            // Use pairwise comparison within the loaded set
            for (j, (_mid_j, emb_j)) in id_embeddings.iter().enumerate() {
                if i >= j {
                    continue;
                }
                let score = crate::search::dot_product_simd(&emb, emb_j);
                if score >= threshold {
                    union(&mut parent, i, j);
                    let root = find(&mut parent, i);
                    let entry = max_sim.entry(root).or_insert(0.0);
                    if score > *entry {
                        *entry = score;
                    }
                }
            }
        }
    }

    // Group into clusters
    let mut clusters_map: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        clusters_map.entry(root).or_default().push(i);
    }

    let mut clusters: Vec<DuplicateCluster> = Vec::new();
    for (root, members) in &clusters_map {
        if members.len() < 2 {
            continue;
        }
        let entries: Vec<DuplicateEntry> = members
            .iter()
            .map(|&idx| {
                let mid = id_embeddings[idx].0;
                let summary = records
                    .iter()
                    .find(|r| r.id == mid)
                    .map(|r| r.summary.clone())
                    .unwrap_or_default();
                DuplicateEntry {
                    id: mid.to_string(),
                    summary,
                }
            })
            .collect();
        let max_similarity = max_sim.get(root).copied().unwrap_or(0.0);
        clusters.push(DuplicateCluster {
            memories: entries,
            max_similarity,
        });
    }

    clusters.sort_by(|a, b| {
        b.max_similarity
            .partial_cmp(&a.max_similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total = clusters.len() as u64;
    let took = start.elapsed().as_micros() as u64;

    Ok(Json(ApiResponse {
        data: ScanDuplicatesResponse { clusters, total },
        took_us: Some(took),
    }))
}

/// GET /metrics -- Prometheus exposition format.
///
/// Returns `text/plain` with metrics lines. Not JSON.
/// Uses the `MetricsCollector` to gather counters and gauges.
pub async fn metrics(
    State(state): State<AppState>,
) -> Result<
    (
        StatusCode,
        [(axum::http::HeaderName, axum::http::HeaderValue); 1],
        String,
    ),
    AppError,
> {
    let output = state.metrics.render_prometheus().await;

    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        output,
    ))
}

/// POST /decay/sweep -- trigger an immediate decay sweep.
///
/// Accepts an optional `as_of` field (milliseconds since epoch) to run
/// the sweep as if the current time were the given timestamp. This is
/// used by the replay driver to simulate time-appropriate decay during
/// historical seeding.
pub async fn trigger_decay_sweep(
    State(state): State<AppState>,
    Json(req): Json<TriggerSweepRequest>,
) -> Result<Json<ApiResponse<TriggerSweepResponse>>, AppError> {
    let result = state
        .decay
        .trigger_sweep(req.as_of)
        .await
        .map_err(|e| AppError::Internal {
            source: format!("sweep failed: {e}").into(),
        })?;

    Ok(Json(ApiResponse {
        data: TriggerSweepResponse {
            memories_scanned: result.memories_scanned,
            full_to_summary: result.full_to_summary,
            summary_to_ghost: result.summary_to_ghost,
            deletions: result.deletions,
            saved_by_connection_bonus: result.saved_by_connection_bonus,
            duration_ms: result.duration.as_millis() as u64,
        },
        took_us: Some(result.duration.as_micros() as u64),
    }))
}

#[cfg(test)]
mod lifecycle_error_tests {
    use super::*;
    use crate::api::FsrsEngine as _;
    use crate::model::DecayPhase;
    use crate::test_support::{Fixture, MemorySpec};
    use axum::response::IntoResponse;

    /// Exercise the handler's mapping with real adapter errors, including terminal content.
    #[tokio::test]
    async fn feedback_handler_maps_terminal_missing_and_invalid_inputs_by_type() {
        let f = Fixture::new();
        let adapter = crate::api::adapters::FsrsEngineAdapter::new(
            f.storage.clone(),
            f.cache.clone(),
            f.graph.clone(),
            std::sync::Arc::new(crate::config::RecalldConfig::default()),
            false,
        );
        for phase in [
            DecayPhase::Ghost,
            DecayPhase::Tombstone,
            DecayPhase::Summary,
        ] {
            let id = f
                .insert_memory_with(MemorySpec {
                    phase: if phase == DecayPhase::Tombstone {
                        DecayPhase::Full
                    } else {
                        phase
                    },
                    summary: "",
                    ..MemorySpec::new(NamespaceId::new(1))
                })
                .await;
            if phase == DecayPhase::Tombstone {
                f.tombstone(id);
            }
            let error = adapter.reinforce(id, 3).await.unwrap_err();
            let response = feedback_error(error, &id.to_string()).into_response();
            assert_eq!(response.status(), StatusCode::CONFLICT);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"], "CONFLICT");
            assert_eq!(f.record(id).unwrap().phase, phase);
            assert_eq!(f.record(id).unwrap().last_accessed_at, 0);
        }
        let missing = MemoryId::new();
        let error = adapter.reinforce(missing, 3).await.unwrap_err();
        assert_eq!(
            feedback_error(error, &missing.to_string())
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
        let id = f.insert_memory().await;
        let error = adapter.reinforce(id, 0).await.unwrap_err();
        assert_eq!(
            feedback_error(error, &id.to_string())
                .into_response()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}

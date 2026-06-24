//! Request handler functions for all API endpoints.
//!
//! Each handler is an `async fn` that receives axum extractors and
//! returns `Result<impl IntoResponse, AppError>`. All handlers follow
//! the same pattern: extract, validate, delegate to subsystem, convert
//! result, respond.

use std::path::Path as FilePath;
use std::time::Instant;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use uuid::Uuid;

use super::errors::AppError;
use super::models::*;
use super::state::{AppState, QueryInput, SearchQuery};
use crate::decay::config::DecayConfig;
use crate::decay::fsrs::FsrsEngine as FsrsCalculator;
use crate::model::decay::DecayPhase;
use crate::model::id::{MemoryId, NamespaceId};
use crate::model::memory::AccessKind;
use crate::serialization::{
    ApiResponse, MemoryResponse, NamespaceRequest, NamespaceResponse, SearchHit, SearchRequest,
    SearchResponse,
};

// ═══════════════════════════════════════════════════════════════════════
// Memory CRUD
// ═══════════════════════════════════════════════════════════════════════

/// POST /memories -- create a new memory.
///
/// Steps:
/// 1. Validate request: summary non-empty, tags <= 64, namespace exists.
/// 2. Resolve namespace by name -> NamespaceId.
/// 3. Generate embedding if not provided (calls embedding provider).
/// 4. Validate embedding dimensionality against namespace config.
/// 5. Persist to storage (meta.db, text.log, vectors.dat).
/// 6. Insert into RAM cache and vector index.
/// 7. If `parent_id` provided, create parent->child edge in graph.
/// 8. Return 201 with the created memory.
pub async fn create_memory(
    State(state): State<AppState>,
    Json(req): Json<CreateMemoryApiRequest>,
) -> Result<(StatusCode, Json<ApiResponse<MemoryResponse>>), AppError> {
    let start = Instant::now();

    // --- Validation ---
    if req.summary.is_empty() {
        return Err(AppError::BadRequest {
            message: "summary must not be empty".into(),
            field: Some("summary".into()),
        });
    }
    if req.summary.len() > 2000 {
        return Err(AppError::BadRequest {
            message: "summary exceeds 2,000 byte limit".into(),
            field: Some("summary".into()),
        });
    }
    if let Some(ref text) = req.full_text {
        if text.len() > 1_048_576 {
            return Err(AppError::BadRequest {
                message: "full_text exceeds 1 MB limit".into(),
                field: Some("fullText".into()),
            });
        }
    }
    if req.tags.len() > 64 {
        return Err(AppError::BadRequest {
            message: "too many tags (max 64)".into(),
            field: Some("tags".into()),
        });
    }

    // --- Resolve namespace ---
    let ns = state
        .namespaces
        .resolve(&req.namespace)
        .ok_or_else(|| AppError::NotFound {
            resource: "namespace",
            id: req.namespace.clone(),
        })?;

    // --- Embedding ---
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
        None => state.search.embed_text(&req.summary, ns.id).await?,
    };

    // --- Persist ---
    let memory = state
        .storage
        .create_memory(
            ns.id,
            &req.summary,
            req.full_text.as_deref(),
            &req.tags,
            &embedding,
            req.initial_stability,
        )
        .await?;

    // --- Parent edge ---
    if let Some(parent_id) = req.parent_id {
        state.graph.add_edge(parent_id, memory.id, "parent").await?;
    }

    // --- Cache + vector index ---
    state.cache.insert(&memory).await;
    state
        .search
        .index_memory(memory.id, &embedding, ns.id)
        .await;

    let response = MemoryResponse::from_cached(&memory, ns.name.clone());
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
) -> Result<Json<ApiResponse<MemoryResponse>>, AppError> {
    let start = Instant::now();

    let memory_id = parse_memory_id(&id)?;

    let record = state
        .cache
        .get_or_load(memory_id, state.storage.as_ref())
        .await
        .ok_or_else(|| AppError::NotFound {
            resource: "memory",
            id: id.clone(),
        })?;

    // Record access for decay tracking
    state
        .decay
        .record_access(memory_id, AccessKind::DirectRetrieval)
        .await;

    let ns_name = state
        .namespaces
        .name_for(record.namespace_id)
        .unwrap_or_else(|| "unknown".to_string());

    let response = MemoryResponse::from_cached(&record, ns_name);
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
/// 2. Remove from storage (meta.db, text.log pointer, vectors.dat slot).
/// 3. Remove from cache and vector index.
/// 4. Clean up graph edges (both directions).
/// 5. Return 200 with deletion confirmation.
pub async fn delete_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DeleteResponse>>, AppError> {
    let start = Instant::now();

    let memory_id = parse_memory_id(&id)?;

    let existed = state.storage.delete_memory(memory_id).await?;
    if existed {
        state.cache.remove(memory_id).await;
        state.search.remove_from_index(memory_id).await;
        state.graph.remove_all_edges(memory_id).await?;
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
        .map_err(|e| {
            if e.to_string().contains("not found") {
                AppError::NotFound {
                    resource: "memory",
                    id: id.clone(),
                }
            } else {
                AppError::Internal { source: e }
            }
        })?;

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
                message: format!(
                    "invalid phase '{phase}', must be: full, summary, ghost"
                ),
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

    // Convert phase string to DecayPhase enum
    let phase_filter = params.phase.as_ref().map(|p| match p.as_str() {
        "full" => crate::model::DecayPhase::Full,
        "summary" => crate::model::DecayPhase::Summary,
        "ghost" => crate::model::DecayPhase::Ghost,
        _ => crate::model::DecayPhase::Full, // unreachable due to validation above
    });

    // Build filter struct
    let filter = ListFilter {
        namespace_id,
        phase: phase_filter,
        tags: params.tags,
    };

    // Get filtered records from storage
    let mut records = state.storage.list_memories(&filter).await?;

    // Sort in memory
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
        _ => {} // unreachable
    }

    let total = records.len() as u64;

    // Apply pagination
    let page_records: Vec<_> = records.into_iter().skip(offset).take(limit).collect();

    // Convert to MemoryResponse objects
    let memories: Vec<MemoryResponse> = page_records
        .into_iter()
        .map(|record| {
            let ns_name = state
                .namespaces
                .name_for(record.namespace_id)
                .unwrap_or_else(|| "unknown".to_string());
            MemoryResponse::from_cached(&record, ns_name)
        })
        .collect();

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

    // Build internal query
    let query_input = if let Some(ref text) = req.query {
        Some(QueryInput::Text(text.clone()))
    } else {
        req.embedding
            .as_ref()
            .map(|vec| QueryInput::Vector(vec.clone()))
    };

    let search_query = SearchQuery {
        query: query_input.unwrap_or(QueryInput::Text(String::new())),
        namespace_id: ns.id,
        k: limit,
        include_tags: req.tags.clone(),
        exclude_tags: vec![],
        decay_phases: None,
        min_score: req.min_strength,
        graph_depth: req.depth as usize,
        apply_rif: true,
    };

    let results = state.search.search(search_query).await?;

    // Convert to API response
    let hits: Vec<SearchHit> = results
        .into_iter()
        .map(|r| {
            let ns_name = state
                .namespaces
                .name_for(r.memory.namespace_id)
                .unwrap_or_else(|| "unknown".to_string());
            let mut mem = MemoryResponse::from_cached(&r.memory, ns_name);
            if req.include_embeddings {
                // Load embedding from vector index
                mem.embedding = state.search.get_embedding(r.memory.id);
            }
            // Note: access_history is not held in CachedRecord (too
            // large for the hot cache). If req.include_history is true,
            // a full load from storage would be needed. Deferred to v2.
            SearchHit {
                memory: mem,
                score: r.score,
            }
        })
        .collect();

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

    // Load source memory to get namespace
    let source = state
        .cache
        .get_or_load(memory_id, state.storage.as_ref())
        .await
        .ok_or_else(|| AppError::NotFound {
            resource: "memory",
            id: id.clone(),
        })?;

    let source_embedding =
        state
            .search
            .get_embedding(memory_id)
            .ok_or_else(|| AppError::Internal {
                source: "source memory has no indexed embedding".into(),
            })?;

    let limit = req.limit.min(100) as usize;

    // Search using source embedding, requesting limit+1 to account
    // for the source memory appearing in its own results.
    let search_query = SearchQuery {
        query: QueryInput::Vector(source_embedding),
        namespace_id: source.namespace_id,
        k: limit + 1,
        include_tags: vec![],
        exclude_tags: vec![],
        decay_phases: None,
        min_score: req.min_score,
        graph_depth: 0,
        apply_rif: false,
    };

    let results = state.search.search(search_query).await?;

    // Exclude source memory from results
    let hits: Vec<SearchHit> = results
        .into_iter()
        .filter(|r| r.memory.id != memory_id)
        .take(limit)
        .map(|r| {
            let ns_name = state
                .namespaces
                .name_for(r.memory.namespace_id)
                .unwrap_or_else(|| "unknown".to_string());
            let mut mem = MemoryResponse::from_cached(&r.memory, ns_name);
            if req.include_embeddings {
                mem.embedding = state.search.get_embedding(r.memory.id);
            }
            SearchHit {
                memory: mem,
                score: r.score,
            }
        })
        .collect();

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
/// 1. Validate name format (1-64 chars, alphanumeric + hyphens + underscores).
/// 2. Check for duplicate name.
/// 3. Register namespace with fixed embedding dimensionality.
/// 4. Return 201 with namespace details.
pub async fn create_namespace(
    State(state): State<AppState>,
    Json(req): Json<NamespaceRequest>,
) -> Result<(StatusCode, Json<ApiResponse<NamespaceResponse>>), AppError> {
    let start = Instant::now();

    // Validate name format
    if req.name.is_empty() || req.name.len() > 64 {
        return Err(AppError::BadRequest {
            message: "namespace name must be 1-64 characters".into(),
            field: Some("name".into()),
        });
    }
    if !req
        .name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AppError::BadRequest {
            message:
                "namespace name may only contain alphanumeric characters, hyphens, and underscores"
                    .into(),
            field: Some("name".into()),
        });
    }

    // Validate embedding dimensions
    if req.embedding_dim == 0 || req.embedding_dim > 8192 {
        return Err(AppError::BadRequest {
            message: "embedding_dim must be between 1 and 8192".into(),
            field: Some("embeddingDim".into()),
        });
    }

    // Create namespace (registry checks for duplicates)
    let ns = state
        .namespaces
        .create(
            &req.name,
            req.embedding_dim,
            req.initial_stability,
            req.desired_retention,
        )
        .await
        .map_err(|_e| AppError::Conflict {
            message: format!("namespace '{}' already exists", req.name),
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
pub async fn health_report(
    State(state): State<AppState>,
    Query(params): Query<HealthReportQuery>,
) -> Result<Json<ApiResponse<HealthReport>>, AppError> {
    let start = Instant::now();

    // Resolve optional namespace filter
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

    // Scan all records once and reuse across sections
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

    let overview = compute_overview(&filtered);
    let decay_forecast = compute_decay_forecast(&filtered, &state);
    let at_risk = compute_at_risk(&filtered, &state);
    let age_distribution = compute_age_distribution(&filtered);
    let storage = compute_storage_breakdown(&state, namespace_filter).await;
    let metadata = compute_metadata_stats(&state, namespace_filter).await?;

    let scope = params.namespace.unwrap_or_else(|| "all".to_string());

    let report = HealthReport {
        scope,
        overview,
        decay_forecast,
        at_risk,
        age_distribution,
        storage,
        metadata,
    };

    let took = start.elapsed().as_micros() as u64;

    Ok(Json(ApiResponse {
        data: report,
        took_us: Some(took),
    }))
}

/// Build a DecayConfig from a NamespaceConfig's phase thresholds.
fn decay_config_for_namespace(
    ns: &crate::model::namespace::NamespaceConfig,
) -> DecayConfig {
    DecayConfig {
        initial_stability: ns.initial_stability,
        phase_1_threshold: ns.phase_thresholds.full_threshold,
        phase_2_threshold: ns.phase_thresholds.summary_threshold,
        phase_3_threshold: ns.phase_thresholds.ghost_threshold,
        permastore_threshold: ns.permastore_threshold,
        ..DecayConfig::default()
    }
}

/// Compute overview section from pre-filtered records.
fn compute_overview(
    records: &[&(MemoryId, crate::model::record::DiskRecord)],
) -> HealthOverview {
    let mut full = 0u64;
    let mut summary = 0u64;
    let mut ghost = 0u64;
    let mut permastore = 0u64;

    for (_, r) in records {
        match DecayPhase::from_u8(r.phase) {
            Some(DecayPhase::Full) => full += 1,
            Some(DecayPhase::Summary) => summary += 1,
            Some(DecayPhase::Ghost) => ghost += 1,
            None => {}
        }
        if r.is_permastore != 0 {
            permastore += 1;
        }
    }

    HealthOverview {
        total_memories: records.len() as u64,
        phase_counts: PhaseCounts { full, summary, ghost },
        permastore_count: permastore,
    }
}

/// Compute decay forecast from pre-filtered records.
fn compute_decay_forecast(
    records: &[&(MemoryId, crate::model::record::DiskRecord)],
    state: &AppState,
) -> DecayForecast {
    let now_millis = chrono::Utc::now().timestamp_millis();
    let mut t7 = TransitionCounts::default();
    let mut t30 = TransitionCounts::default();
    let mut t90 = TransitionCounts::default();

    for (_, record) in records {
        // Skip permastore -- they never decay
        if record.is_permastore != 0 {
            continue;
        }

        // Get namespace config for thresholds
        let ns_config = match state.namespaces.get_by_id(record.namespace_id) {
            Some(c) => c,
            None => continue,
        };
        let dc = decay_config_for_namespace(&ns_config);
        let engine = FsrsCalculator::new(&dc);

        // Compute elapsed time
        let elapsed_millis = (now_millis - record.last_accessed_at).max(0) as f64;
        let elapsed_days = (elapsed_millis / 86_400_000.0) as f32;

        // Current retrievability for forecasting
        let current_r = engine.retrievability(elapsed_days, record.stability, 1.0);

        // Determine current phase and next threshold
        let current_phase = match DecayPhase::from_u8(record.phase) {
            Some(p) => p,
            None => continue,
        };

        let (phase_label, threshold) = match current_phase {
            DecayPhase::Full => (DecayPhase::Full, dc.phase_1_threshold),
            DecayPhase::Summary => (DecayPhase::Summary, dc.phase_2_threshold),
            DecayPhase::Ghost => (DecayPhase::Ghost, dc.phase_3_threshold),
        };

        if current_r <= threshold {
            // Already below threshold -- will transition on next sweep
            count_transition(phase_label, 0.0, &mut t7, &mut t30, &mut t90);
        } else {
            let days_until = engine.days_until_threshold(record.stability, threshold);
            let remaining = days_until - elapsed_days;
            if remaining > 0.0 {
                count_transition(phase_label, remaining, &mut t7, &mut t30, &mut t90);
            } else {
                count_transition(phase_label, 0.0, &mut t7, &mut t30, &mut t90);
            }
        }
    }

    DecayForecast {
        transitions_7d: t7,
        transitions_30d: t30,
        transitions_90d: t90,
    }
}

/// Increment the appropriate transition counter based on phase and horizon.
fn count_transition(
    phase: DecayPhase,
    days: f32,
    t7: &mut TransitionCounts,
    t30: &mut TransitionCounts,
    t90: &mut TransitionCounts,
) {
    let increment = |tc: &mut TransitionCounts| match phase {
        DecayPhase::Full => tc.full_to_summary += 1,
        DecayPhase::Summary => tc.summary_to_ghost += 1,
        DecayPhase::Ghost => tc.ghost_to_deleted += 1,
    };

    if days <= 7.0 {
        increment(t7);
        increment(t30);
        increment(t90);
    } else if days <= 30.0 {
        increment(t30);
        increment(t90);
    } else if days <= 90.0 {
        increment(t90);
    }
}

/// Compute at-risk memories (Ghost phase, closest to deletion).
fn compute_at_risk(
    records: &[&(MemoryId, crate::model::record::DiskRecord)],
    state: &AppState,
) -> Vec<AtRiskMemory> {
    let now_millis = chrono::Utc::now().timestamp_millis();
    let mut candidates: Vec<AtRiskMemory> = Vec::new();

    for (id, record) in records {
        // Only Ghost phase, non-permastore
        if record.phase != DecayPhase::Ghost.as_u8() || record.is_permastore != 0 {
            continue;
        }

        let ns_config = match state.namespaces.get_by_id(record.namespace_id) {
            Some(c) => c,
            None => continue,
        };
        let dc = decay_config_for_namespace(&ns_config);
        let engine = FsrsCalculator::new(&dc);

        let elapsed_millis = (now_millis - record.last_accessed_at).max(0) as f64;
        let elapsed_days = (elapsed_millis / 86_400_000.0) as f32;
        let current_r = engine.retrievability(elapsed_days, record.stability, 1.0);

        let days_until_deletion = if current_r <= dc.phase_3_threshold {
            0.0
        } else {
            let days_until = engine.days_until_threshold(record.stability, dc.phase_3_threshold);
            (days_until - elapsed_days).max(0.0)
        };

        let id_str = id.to_string();
        let short_id = if id_str.len() >= 8 {
            id_str[..8].to_string()
        } else {
            id_str.clone()
        };

        candidates.push(AtRiskMemory {
            id: short_id,
            summary: record.summary.chars().take(100).collect(),
            strength: current_r,
            days_until_deletion,
            phase: "ghost".to_string(),
        });
    }

    // Sort by days_until_deletion ascending, take top 10
    candidates.sort_by(|a, b| {
        a.days_until_deletion
            .partial_cmp(&b.days_until_deletion)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(10);

    candidates
}

/// Compute age distribution from pre-filtered records.
fn compute_age_distribution(
    records: &[&(MemoryId, crate::model::record::DiskRecord)],
) -> AgeDistribution {
    if records.is_empty() {
        return AgeDistribution {
            oldest_created_at: None,
            newest_created_at: None,
            avg_age_days: 0.0,
            median_stability: 0.0,
        };
    }

    let now = chrono::Utc::now().timestamp_millis();

    let mut oldest = i64::MAX;
    let mut newest = i64::MIN;
    let mut total_age_ms = 0i64;
    let mut stabilities: Vec<f32> = Vec::with_capacity(records.len());

    for (_, r) in records {
        if r.created_at < oldest {
            oldest = r.created_at;
        }
        if r.created_at > newest {
            newest = r.created_at;
        }
        total_age_ms += now - r.created_at;
        stabilities.push(r.stability);
    }

    let avg_age_days = (total_age_ms as f64 / records.len() as f64) / 86_400_000.0;

    stabilities.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_stability = stabilities[stabilities.len() / 2];

    AgeDistribution {
        oldest_created_at: Some(oldest),
        newest_created_at: Some(newest),
        avg_age_days: avg_age_days as f32,
        median_stability,
    }
}

/// Compute storage breakdown from file sizes on disk.
async fn compute_storage_breakdown(
    state: &AppState,
    namespace_filter: Option<NamespaceId>,
) -> StorageBreakdown {
    let db_path = state.storage.storage_path();

    let meta_db_bytes = file_size(&db_path.join("meta.db")).unwrap_or(0);
    let edges_db_bytes = file_size(&db_path.join("edges.db")).unwrap_or(0);
    let text_log_bytes = file_size(&db_path.join("text.log")).unwrap_or(0);

    let namespaces = state.namespaces.list_all().await;
    let mut vector_files = Vec::new();

    for ns in &namespaces {
        // Apply namespace filter if present
        if let Some(filter_id) = namespace_filter {
            if ns.id != filter_id.get() {
                continue;
            }
        }

        let vector_path = db_path.join("vectors").join(format!("{}.dat", ns.name));
        let bytes = file_size(&vector_path).unwrap_or(0);
        vector_files.push(VectorFileSize {
            namespace: ns.name.clone(),
            bytes,
        });
    }

    let total_bytes = meta_db_bytes
        + edges_db_bytes
        + text_log_bytes
        + vector_files.iter().map(|v| v.bytes).sum::<u64>();

    StorageBreakdown {
        total_bytes,
        meta_db_bytes,
        edges_db_bytes,
        text_log_bytes,
        vector_files,
    }
}

/// Get file size in bytes, returning an io::Error on failure.
fn file_size(path: &FilePath) -> Result<u64, std::io::Error> {
    std::fs::metadata(path).map(|m| m.len())
}

/// Compute metadata/tag statistics.
async fn compute_metadata_stats(
    state: &AppState,
    _namespace_filter: Option<NamespaceId>,
) -> Result<MetadataStats, AppError> {
    // list_tags returns all tags sorted by count descending
    let all_tags = state.storage.list_tags().await?;

    let unique_tags = all_tags.len() as u64;
    let top_tags: Vec<TagCount> = all_tags
        .into_iter()
        .take(10)
        .map(|(tag, count)| TagCount { tag, count })
        .collect();

    Ok(MetadataStats {
        top_tags,
        unique_tags,
    })
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

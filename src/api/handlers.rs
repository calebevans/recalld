//! Request handler functions for all API endpoints.
//!
//! Each handler is an `async fn` that receives axum extractors and
//! returns `Result<impl IntoResponse, AppError>`. All handlers follow
//! the same pattern: extract, validate, delegate to subsystem, convert
//! result, respond.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use std::time::Instant;
use uuid::Uuid;

use super::errors::AppError;
use super::models::*;
use super::state::{AppState, QueryInput, SearchQuery};
use crate::model::id::MemoryId;
use crate::model::memory::AccessKind;
use crate::serialization::{
    ApiResponse, MemoryResponse, NamespaceRequest, NamespaceResponse,
    SearchHit, SearchRequest, SearchResponse,
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
        None => {
            state
                .search
                .embed_text(&req.summary, ns.id)
                .await?
        }
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
        state
            .graph
            .add_edge(parent_id, memory.id, "parent")
            .await?;
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
            message: "at least one of 'query', 'embedding', or 'tags' must be provided"
                .into(),
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
        state.search.get_embedding(memory_id).ok_or_else(|| {
            AppError::Internal {
                source: "source memory has no indexed embedding".into(),
            }
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
            message: "namespace name may only contain alphanumeric characters, hyphens, and underscores".into(),
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
pub async fn health_check(
    State(state): State<AppState>,
) -> Result<Json<HealthResponse>, AppError> {
    let uptime = state.started_at.elapsed().as_secs();

    let storage_health = probe_component("storage", || async {
        state.storage.ping().await
    })
    .await;

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
        message: Some(format!(
            "indexed: {}",
            state.search.indexed_count()
        )),
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
        message: state.decay.last_sweep_time().map(|t| {
            format!("last sweep: {}s ago", t.elapsed().as_secs())
        }),
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
    let status = if subsystems.storage.status == "down"
        || subsystems.cache.status == "down"
    {
        HealthStatus::Unhealthy
    } else if subsystems.embedding.status == "down"
        || subsystems.decay.status == "down"
    {
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
            axum::http::HeaderValue::from_static(
                "text/plain; charset=utf-8",
            ),
        )],
        output,
    ))
}

//! Route definitions -- maps URL paths to handler functions.
//!
//! The [`router`] function constructs the complete axum `Router` with
//! all routes and middleware layers applied.

use std::time::Duration;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{delete, get, post},
};
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, timeout::TimeoutLayer, trace::TraceLayer};
use tracing::warn;

use super::ApiConfig;
use super::handlers;
use super::middleware::request_id_middleware;
use super::state::AppState;

/// Constructs the complete axum Router with all routes and middleware.
///
/// # Route table
///
/// | Method | Path                      | Handler                  | Purpose                    |
/// |--------|---------------------------|--------------------------|----------------------------|
/// | GET    | /memories                 | list_memories            | List all memories (paginated) |
/// | POST   | /memories                 | create_memory            | Create a new memory        |
/// | GET    | /memories/:id             | get_memory               | Retrieve a single memory   |
/// | DELETE | /memories/:id             | delete_memory            | Delete a memory            |
/// | POST   | /memories/:id/reinforce   | reinforce_memory         | Manual reinforcement       |
/// | POST   | /search                   | search_memories          | Multi-modal search         |
/// | POST   | /similar/:id              | find_similar             | Similar memories by ID     |
/// | GET    | /namespaces               | list_namespaces          | List all namespaces        |
/// | POST   | /namespaces               | create_namespace         | Create a namespace         |
/// | GET    | /namespaces/:id/stats     | namespace_stats          | Namespace statistics       |
/// | GET    | /health                   | health_check             | Health + subsystem status  |
/// | GET    | /health/report            | health_report            | Decay health report        |
/// | GET    | /metrics                  | metrics                  | Prometheus metrics export  |
pub fn router(state: AppState, config: &ApiConfig) -> Router {
    let addr = config.bind_address.as_str();
    if addr != "127.0.0.1" && addr != "::1" && addr != "localhost" {
        warn!(
            bind_address = addr,
            "API server binding to non-loopback address -- the API is exposed to the network without authentication"
        );
    }

    let memory_routes = Router::new()
        .route("/", get(handlers::list_memories))
        .route("/", post(handlers::create_memory))
        .route("/batch", post(handlers::batch_store))
        .route("/{id}", get(handlers::get_memory))
        .route("/{id}", delete(handlers::delete_memory))
        .route("/{id}/reinforce", post(handlers::reinforce_memory));

    let search_routes = Router::new()
        .route("/search", post(handlers::search_memories))
        .route("/similar/{id}", post(handlers::find_similar));

    let namespace_routes = Router::new()
        .route("/", get(handlers::list_namespaces))
        .route("/", post(handlers::create_namespace))
        .route("/{id}/stats", get(handlers::namespace_stats))
        .route("/{name}/duplicates", post(handlers::scan_duplicates));

    let ops_routes = Router::new()
        .route("/health", get(handlers::health_check))
        .route("/health/report", get(handlers::health_report))
        .route("/metrics", get(handlers::metrics))
        .route("/decay/sweep", post(handlers::trigger_decay_sweep));

    let cors = if config.cors_permissive {
        CorsLayer::permissive()
    } else {
        CorsLayer::new()
    };

    Router::new()
        .nest("/memories", memory_routes)
        .merge(search_routes)
        .nest("/namespaces", namespace_routes)
        .merge(ops_routes)
        .layer(
            ServiceBuilder::new()
                .layer(axum::middleware::from_fn(request_id_middleware))
                .layer(TraceLayer::new_for_http())
                .layer(TimeoutLayer::with_status_code(
                    axum::http::StatusCode::REQUEST_TIMEOUT,
                    Duration::from_secs(config.request_timeout_secs),
                ))
                .layer(cors)
                .layer(DefaultBodyLimit::max(config.max_body_size)),
        )
        .with_state(state)
}

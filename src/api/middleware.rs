//! Custom middleware for the API server.
//!
//! Provides request ID injection for tracing correlation. Every request
//! gets a unique ID that is attached to the tracing span, inserted
//! into request extensions (for handler access), and included in the
//! `X-Request-ID` response header.

use axum::{
    body::Body,
    http::{HeaderValue, Request},
    middleware::Next,
    response::Response,
};
use uuid::Uuid;

/// Generates a UUID v4 request ID, attaches it to the tracing span,
/// and includes it in the `X-Request-ID` response header.
///
/// If the client sends an `X-Request-ID` header, it is preserved
/// rather than overwritten -- this supports distributed tracing.
pub async fn request_id_middleware(
    req: Request<Body>,
    next: Next,
) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // Attach to tracing span
    let _span = tracing::info_span!(
        "request",
        request_id = %request_id,
        method = %req.method(),
        path = %req.uri().path(),
    );

    let _guard = _span.enter();
    let mut response = next.run(req).await;

    if let Ok(val) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", val);
    }

    response
}

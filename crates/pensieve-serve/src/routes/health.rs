//! Health check endpoints.

use axum::Json;
use serde::Serialize;

/// Health check response.
#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

/// Public health check endpoint.
///
/// Returns basic service health without authentication.
/// Use this for load balancer health probes.
pub async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// Authenticated ping endpoint.
///
/// Returns a simple response confirming authentication works.
/// Useful for API clients to verify their token is valid.
#[derive(Debug, Clone, Serialize)]
pub struct PingResponse {
    message: &'static str,
}

pub async fn authenticated_ping() -> Json<PingResponse> {
    Json(PingResponse {
        message: "pong",
    })
}


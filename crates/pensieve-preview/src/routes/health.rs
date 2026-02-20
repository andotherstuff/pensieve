//! Health check endpoint.

use axum::Json;
use serde::Serialize;

/// Health check response.
#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

/// Public health check endpoint.
///
/// Returns basic service health for load balancer probes.
pub async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "pensieve-preview",
        version: env!("CARGO_PKG_VERSION"),
    })
}

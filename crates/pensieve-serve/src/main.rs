//! Pensieve Serve - HTTP API server for Nostr analytics.
//!
//! This binary starts the API server that provides analytics queries against
//! the ClickHouse event store.

use axum::http::Request;
use clap::Parser;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use pensieve_serve::{AppState, Config, router};

/// Pensieve API server for Nostr analytics.
#[derive(Parser, Debug)]
#[command(name = "pensieve-serve")]
#[command(about = "HTTP API server for Nostr analytics", long_about = None)]
struct Args {
    /// Path to .env file (optional).
    #[arg(long, env = "DOTENV_PATH", default_value = ".env")]
    dotenv: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse CLI arguments
    let args = Args::parse();

    // Load .env file if it exists
    if std::path::Path::new(&args.dotenv).exists() {
        dotenvy::from_path(&args.dotenv)?;
        eprintln!("Loaded environment from {}", args.dotenv);
    }

    // Initialize tracing
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load configuration
    let config = Config::from_env()?;
    let bind_addr = config.bind_addr.clone();

    // Create application state
    let state = AppState::new(config);

    // Build router with middleware
    let app = router(state)
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                tracing::span!(
                    Level::INFO,
                    "http_request",
                    method = %request.method(),
                    path = %request.uri().path(),
                    query = request.uri().query().unwrap_or("")
                )
            }),
        )
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    // Start server
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(addr = %bind_addr, "starting server");

    axum::serve(listener, app).await?;

    Ok(())
}

//! Pensieve Preview - HTTP server for static Nostr event preview pages.
//!
//! Serves HTML preview pages with Open Graph tags for any Nostr event,
//! designed to be placed behind a CDN for edge caching.

use axum::http::Request;
use clap::Parser;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use pensieve_preview::{AppState, Config, router};

/// Pensieve Preview - static HTML preview pages for Nostr events.
#[derive(Parser, Debug)]
#[command(name = "pensieve-preview")]
#[command(about = "Static HTML preview server for Nostr events", long_about = None)]
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
    tracing::info!(addr = %bind_addr, "starting preview server");

    axum::serve(listener, app).await?;

    Ok(())
}

//! memory-box server: router + state untuk dipakai binary dan integration test.

pub mod api;
pub mod embed;
pub mod indexer;
pub mod parser;
pub mod state;
pub mod store;
pub mod watcher;

use axum::Router;
use state::AppState;
use std::sync::Arc;

/// Router utama — dipakai main.rs dan test endpoint (/v1/*).
pub fn router(app_state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", axum::routing::get(api::healthz))
        .route("/v1/projects/open", axum::routing::post(api::projects_open))
        .route("/v1/index/scan", axum::routing::post(api::index_scan))
        .route("/v1/index/remove", axum::routing::post(api::index_remove))
        .route("/v1/index/status", axum::routing::get(api::index_status))
        .route("/v1/retrieve", axum::routing::post(api::retrieve))
        .route("/v1/graph/context", axum::routing::post(api::graph_context))
        .route("/v1/symbols/lookup", axum::routing::post(api::symbols_lookup))
        .route("/v1/memory/write", axum::routing::post(api::memory_write))
        .route("/v1/memory/search", axum::routing::post(api::memory_search))
        .route("/v1/memory/forget", axum::routing::post(api::memory_forget))
        .layer(axum::middleware::from_fn(
            |req: axum::http::Request<axum::body::Body>,
             next: axum::middleware::Next| async move {
                let started = std::time::Instant::now();
                let method = req.method().clone();
                let path = req.uri().path().to_string();
                let res = next.run(req).await;
                tracing::info!(%method, %path, status = %res.status(), ms = started.elapsed().as_millis(), "request");
                res
            },
        ))
        .with_state(app_state)
}

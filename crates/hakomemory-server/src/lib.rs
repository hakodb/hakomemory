//! memory-box server: router + state untuk dipakai binary dan integration test.
//! Engine logic lives in `hakomemory-core`; modules are re-exported here so
//! existing `/v1/*` handlers and downstream imports keep working unchanged.

pub use hakomemory_core::{embed, indexer, parser, store, watcher};
pub mod api;
pub mod auth;
pub mod state;

use axum::response::IntoResponse;
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
        .route("/v1/documents/upsert", axum::routing::post(api::documents_upsert))
        .route("/v1/documents/batch", axum::routing::post(api::documents_batch))
        .route("/v1/documents/get", axum::routing::get(api::documents_get))
        .route("/v1/documents/list", axum::routing::get(api::documents_list))
        .route("/v1/documents/delete", axum::routing::post(api::documents_delete))
        .route("/v1/documents/scan", axum::routing::post(api::documents_scan))
        .route("/v1/admin/migrate", axum::routing::post(api::admin_migrate))
        .route("/v1/symbols/lookup", axum::routing::post(api::symbols_lookup))
        .route("/v1/memory/write", axum::routing::post(api::memory_write))
        .route("/v1/memory/search", axum::routing::post(api::memory_search))
        .route("/v1/memory/forget", axum::routing::post(api::memory_forget))
        .route(
            "/v1/admin/backfill",
            axum::routing::post(api::admin_backfill_owner),
        )
        .route("/v1/admin/revoke", axum::routing::post(api::admin_revoke))
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
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            |axum::extract::State(state): axum::extract::State<Arc<AppState>>,
             mut req: axum::http::Request<axum::body::Body>,
             next: axum::middleware::Next| async move {
                if req.uri().path() == "/healthz" {
                    return next.run(req).await;
                }
                let revoked = state.revoked.lock().unwrap().clone();
                match state.auth.authenticate(req.headers(), &revoked) {
                    Ok(ctx) => {
                        if !state.rate.allow(&ctx.sub) {
                            return axum::http::StatusCode::TOO_MANY_REQUESTS.into_response();
                        }
                        req.extensions_mut().insert(ctx);
                        next.run(req).await
                    }
                    Err(code) => code.into_response(),
                }
            },
        ))
        .with_state(app_state)
}

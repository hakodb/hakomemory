mod api;
mod embed;
mod indexer;
mod parser;
mod state;
mod store;
mod watcher;

use anyhow::Result;
use state::AppState;

#[tokio::main]async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let data_dir = std::env::var("MEMORYBOX_DATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let mut h = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            h.push_str("/.memory-box");
            std::path::PathBuf::from(h)
        });
    let port: u16 = std::env::var("MEMORYBOX_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7412);

    // PENTING (lihat PLAN.md §0.2): Hako::open harus inisialisasi pool
    // rayon global PERTAMA sebelum embedder (candle/tokenizers) aktif.
    std::fs::create_dir_all(&data_dir)?;
    let app_state = AppState::open(&data_dir)?;

    let app = axum::Router::new()
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
        .layer(axum::middleware::from_fn(|req: axum::http::Request<axum::body::Body>, next: axum::middleware::Next| async move {
            let started = std::time::Instant::now();
            let method = req.method().clone();
            let path = req.uri().path().to_string();
            let res = next.run(req).await;
            tracing::info!(%method, %path, status = %res.status(), ms = started.elapsed().as_millis(), "request");
            res
        }))
        .with_state(app_state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("memory-box listening on http://{addr} (data: {})", data_dir.display());
    axum::serve(listener, app).await?;
    Ok(())
}

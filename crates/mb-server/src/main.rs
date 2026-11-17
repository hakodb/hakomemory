use anyhow::Result;
use mb_server::router;
use mb_server::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
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

    // PENTING (lihat docs/PLAN.md §0.2): Hako::open harus inisialisasi pool
    // rayon global PERTAMA sebelum embedder (candle/tokenizers) aktif.
    std::fs::create_dir_all(&data_dir)?;
    let app_state = AppState::open(&data_dir)?;
    let app = router(app_state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("memory-box listening on http://{addr} (data: {})", data_dir.display());
    axum::serve(listener, app).await?;
    Ok(())
}

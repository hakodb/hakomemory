//! State aplikasi: dibuka HAKO-DULU (pemilik pool rayon global), baru
//! embedder candle — urutan ini terkunci, lihat docs/PLAN.md §0.2.

use crate::auth::AuthConfig;
use crate::embed::SharedEmbedder;
use crate::store::ProjectStore;
use crate::watcher;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Home + nama subdirektori cache (khusus unix).
fn dirs_or_default(sub: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".cache").join(sub).join("hub"))
}

pub struct AppState {
    pub store: Arc<ProjectStore>,
    pub embedder: Arc<SharedEmbedder>,
    pub watches: watcher::WatchRegistry,
    pub auth: Arc<AuthConfig>,
}

impl AppState {
    pub fn open(data_dir: &Path) -> Result<Arc<Self>> {
        // 1. pemilik pool rayon: open pertama HakoDB di sini.
        let store = Arc::new(ProjectStore::new(data_dir.to_path_buf()));

        // 2. baru embedder (candle + tokenizers mungkin init rayon sendiri).
        // cache model: ikuti MEMORYBOX_MODEL_CACHE, default cache HF standar
        // (~/.cache/huggingface) agar model dipakai bersama antar proses
        let cache = std::env::var("MEMORYBOX_MODEL_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs_or_default("huggingface").unwrap_or_else(|| data_dir.join("model-cache"))
            });
        let embedder = Arc::new(SharedEmbedder::load(&cache)?);
        // Gagal cepat di startup, bukan panic di tengah indexing: dimensi
        // model harus 384 karena skema index & data yang sudah ada.
        if embedder.dim() != crate::store::DIM {
            bail!(
                "dimensi model {} != {} — memory-box terikat 384d; \
                 gunakan model 384d atau wipe data sebelum ganti model",
                embedder.dim(),
                crate::store::DIM
            );
        }
        Ok(Self::with_embedder(store, embedder, Arc::new(AuthConfig::from_env())))
    }

    /// Konstruktor test: embedder mock, tanpa model/unduhan.
    pub fn open_mock(data_dir: &Path) -> Result<Arc<Self>> {
        Self::open_mock_with_auth(data_dir, AuthConfig::Disabled)
    }

    pub fn open_mock_with_auth(data_dir: &Path, auth: AuthConfig) -> Result<Arc<Self>> {
        let store = Arc::new(ProjectStore::new(data_dir.to_path_buf()));
        let embedder = Arc::new(SharedEmbedder::mock(crate::store::DIM));
        Ok(Self::with_embedder(store, embedder, Arc::new(auth)))
    }

    fn with_embedder(
        store: Arc<ProjectStore>,
        embedder: Arc<SharedEmbedder>,
        auth: Arc<AuthConfig>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            embedder,
            watches: watcher::WatchRegistry::default(),
            auth,
        })
    }
}

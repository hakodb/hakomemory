//! State aplikasi: dibuka HAKO-DULU (pemilik pool rayon global), baru
//! embedder candle — urutan ini terkunci, lihat PLAN.md §0.2.

use crate::embed::SharedEmbedder;
use crate::store::ProjectStore;
use crate::watcher;
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

pub struct AppState {
    pub store: Arc<ProjectStore>,
    pub embedder: Arc<SharedEmbedder>,
    pub watches: watcher::WatchRegistry,
}

impl AppState {
    pub fn open(data_dir: &Path) -> Result<Arc<Self>> {
        // 1. pemilik pool rayon: open pertama HakoDB di sini.
        let store = Arc::new(ProjectStore::new(data_dir.to_path_buf()));

        // 2. baru embedder (candle + tokenizers mungkin init rayon sendiri).
        let cache = data_dir.join("model-cache");
        let embedder = Arc::new(SharedEmbedder::load(&cache)?);

        Ok(Arc::new(Self { store, embedder, watches: watcher::WatchRegistry::default() }))
    }
}

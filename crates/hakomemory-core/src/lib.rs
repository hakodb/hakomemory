//! hakomemory-core — library-first facade for the local knowledge engine.
//!
//! No axum, no `MEMORYBOX_*` environment variables, no server process. The
//! embedder is injected by the caller (use `SharedEmbedder::mock` for offline
//! tests), and every project is a HakoDB directory owned by one writer.
//!
//! The server crate re-exports the engine modules and builds its HTTP adapter
//! on top of this facade, so both paths share exactly the same logic.

pub mod config;
pub mod docs;
pub mod embed;
pub mod error;
pub mod graph;
pub mod indexer;
pub mod parser;
pub mod retrieval;
pub mod sources;
pub mod store;
pub mod watcher;

use crate::embed::SharedEmbedder;
use crate::error::{Error, Result};
use crate::store::{EmbeddingSpec, ProjectStore};
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::Hako;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Configuration injected by a library caller. Nothing is read from env.
pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub embedder: Arc<SharedEmbedder>,
    pub spec: EmbeddingSpec,
    pub max_projects: usize,
    pub idle_ttl: Duration,
}

impl EngineConfig {
    pub fn new(data_dir: impl Into<PathBuf>, embedder: Arc<SharedEmbedder>) -> Self {
        let spec = EmbeddingSpec {
            model_id: "library".into(),
            dim: embedder.dim(),
            metric: "Cosine".into(),
            provider: "local".into(),
        };
        Self {
            data_dir: data_dir.into(),
            embedder,
            spec,
            max_projects: 8,
            idle_ttl: Duration::from_secs(1800),
        }
    }

    pub fn with_spec(mut self, spec: EmbeddingSpec) -> Self {
        self.spec = spec;
        self
    }
}

/// The engine handle. Clone-free: share it with `Arc` across threads.
pub struct Engine {
    store: Arc<ProjectStore>,
    embedder: Arc<SharedEmbedder>,
}

impl Engine {
    pub fn open(config: EngineConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir)
            .map_err(|e| Error::Storage(format!("cannot create data dir: {e}")))?;
        let store = Arc::new(ProjectStore::with_limits_and_spec(
            config.data_dir,
            config.max_projects,
            config.idle_ttl,
            config.spec,
        ));
        Ok(Self { store, embedder: config.embedder })
    }

    /// Wrap an existing store (used by the HTTP adapter so both paths share
    /// one HakoDB pool and one exclusive lock per project).
    pub fn from_store(store: Arc<ProjectStore>, embedder: Arc<SharedEmbedder>) -> Self {
        Self { store, embedder }
    }

    /// Open (or attach to) a project by its canonical filesystem path.
    pub fn project(&self, canonical_path: &str) -> Result<ProjectHandle> {
        let (pid, db) = self.store.open_project(canonical_path)?;
        Ok(ProjectHandle { pid, db, embedder: self.embedder.clone() })
    }

    /// Attach to an already-registered project id (survives restarts).
    pub fn project_by_id(&self, pid: &str) -> Result<ProjectHandle> {
        let db = self.store.get(pid)?;
        Ok(ProjectHandle { pid: pid.to_string(), db, embedder: self.embedder.clone() })
    }

    pub fn store(&self) -> Arc<ProjectStore> {
        self.store.clone()
    }

    pub fn embedder(&self) -> Arc<SharedEmbedder> {
        self.embedder.clone()
    }
}

/// One open project. Methods are synchronous: call from a worker thread if
/// your async runtime needs to stay responsive.
pub struct ProjectHandle {
    pid: String,
    db: Arc<Hako>,
    embedder: Arc<SharedEmbedder>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetrievalHit {
    pub id: String,
    pub score: f64,
    pub path: String,
    pub content: String,
    pub start_line: i64,
}

impl ProjectHandle {
    pub fn pid(&self) -> &str {
        &self.pid
    }

    pub fn db(&self) -> &Hako {
        &self.db
    }

    pub fn embedder_handle(&self) -> Arc<SharedEmbedder> {
        self.embedder.clone()
    }

    /// Index a directory/file into this project, same semantics as
    /// `POST /v1/index/scan` (paths relative to the project root).
    pub fn scan(&self, path: &Path) -> Result<indexer::ScanReport> {
        let root = store::read_root(&self.db)?
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        indexer::scan(&self.db, &self.embedder, Path::new(&root), path)
            .map_err(Error::from)
    }

    /// Hybrid FTS+vector retrieval over `chunks`, with filter pushdown
    /// reserved for the richer retrieval API (issue #7).
    pub fn retrieve(&self, query: &str, k: usize, rrf_k: usize) -> Result<Vec<RetrievalHit>> {
        let vector = self.embedder.embed_one(query).map_err(|e| Error::Embed(e.to_string()))?;
        let rows = self
            .db
            .hybrid_search("chunks", "content", query, "emb", &vector, k, rrf_k)
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(rows
            .iter()
            .map(|(id, score, doc)| RetrievalHit {
                id: id.clone(),
                score: *score,
                path: str_field(doc, "path"),
                content: str_field(doc, "content"),
                start_line: int_field(doc, "start_line"),
            })
            .collect())
    }
}

fn str_field(doc: &HakoDoc, field: &str) -> String {
    match doc.get(field) {
        Some(Value::String(s)) => s.clone(),
        Some(value) => format!("{value:?}"),
        None => String::new(),
    }
}

fn int_field(doc: &HakoDoc, field: &str) -> i64 {
    match doc.get(field) {
        Some(Value::Int(n)) => *n,
        Some(Value::Float(n)) => *n as i64,
        _ => 0,
    }
}


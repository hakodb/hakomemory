//! Manager instance HakoDB per project + skema koleksi/index + LRU.

use anyhow::{anyhow, Context, Result};
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::{BatchMutation, Hako, IndexSchema};
use hakodb::index::vector::Metric;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_DIM: u32 = 384;

/// Spesifikasi embedding yang dipersisten per project. Dimensi bukan konstanta
/// kompile — model apa pun dengan dim 384..=4096 bisa dipakai (batas HakoDB).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddingSpec {
    pub model_id: String,
    pub dim: u32,
    /// "Cosine" | "L2" | "InnerProduct"
    pub metric: String,
    /// "local" | "http"
    pub provider: String,
}

impl EmbeddingSpec {
    pub fn local(dim: u32) -> Self {
        Self {
            model_id: hakomemory_embed::model_from_env(),
            dim,
            metric: "Cosine".into(),
            provider: "local".into(),
        }
    }

    pub fn from_env(dim: u32) -> Self {
        let http = std::env::var("MEMORYBOX_EMBED_URL")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let mut spec = Self::local(dim);
        if http {
            spec.provider = "http".into();
            spec.model_id = std::env::var("MEMORYBOX_EMBED_MODEL")
                .unwrap_or_else(|_| "http-embedder".into());
        }
        spec
    }

    pub fn metric(&self) -> Result<Metric> {
        match self.metric.as_str() {
            "Cosine" => Ok(Metric::Cosine),
            "L2" => Ok(Metric::L2),
            "InnerProduct" => Ok(Metric::InnerProduct),
            other => Err(anyhow!("metric embedding tidak dikenal: {other}")),
        }
    }

    /// Semua atribut yang menentukan ruang vektor. Membandingkan hanya
    /// `dim`/`metric` tidak cukup: model lain dengan dimensi sama tetap
    /// menghasilkan vektor yang tidak kompatibel.
    pub fn drift(&self, other: &EmbeddingSpec) -> Option<String> {
        let mut changes = Vec::new();
        if self.model_id != other.model_id {
            changes.push(format!("model '{}' -> '{}'", self.model_id, other.model_id));
        }
        if self.provider != other.provider {
            changes.push(format!("provider '{}' -> '{}'", self.provider, other.provider));
        }
        if self.dim != other.dim {
            changes.push(format!("{}d -> {}d", self.dim, other.dim));
        }
        if self.metric != other.metric {
            changes.push(format!("metric '{}' -> '{}'", self.metric, other.metric));
        }
        if changes.is_empty() {
            None
        } else {
            Some(changes.join(", "))
        }
    }

    /// Pesan error tunggal untuk drift; dipakai saat open maupun persist.
    pub fn drift_error(existing: &EmbeddingSpec, requested: &EmbeddingSpec) -> anyhow::Error {
        anyhow!(
            "spesifikasi embedding berubah ({}). Vektor lama tidak bisa dipakai \
             dengan model/provider baru. Lakukan migrasi field/index + re-embed \
             eksplisit, atau kembalikan konfigurasi lama.",
            existing.drift(requested).unwrap_or_default()
        )
    }
}

/// Semua koleksi yang perlu vector+FTS index (koleksi memory).
pub const MEMORY_COLLECTIONS: [&str; 3] = ["mem_episodic", "mem_semantic", "mem_procedural"];

pub fn project_id(canonical_path: &str) -> String {
    let h = Sha256::digest(canonical_path.as_bytes());
    hex12(&h)
}

fn hex12(d: &[u8]) -> String {
    d.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

struct Entry {
    db: Arc<Hako>,
    last: Instant,
}

pub struct ProjectStore {
    data_dir: PathBuf,
    entries: Mutex<HashMap<String, Entry>>,
    /// maksimal instance aktif; 0 = tidak ada eviksi
    capacity: usize,
    /// project harus idle minimal ini sebelum bisa di-eviksi; 0 = langsung
    idle_ttl: Duration,
    default_spec: EmbeddingSpec,
}

impl ProjectStore {
    pub fn new(data_dir: PathBuf, default_spec: EmbeddingSpec) -> Self {
        let capacity = std::env::var("MEMORYBOX_MAX_PROJECTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);
        let idle_ttl = std::env::var("MEMORYBOX_PROJECT_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(1800));
        Self::with_limits_and_spec(data_dir, capacity, idle_ttl, default_spec)
    }

    /// Konstruktor test: spec lokal 384d default.
    pub fn with_limits(data_dir: PathBuf, capacity: usize, idle_ttl: Duration) -> Self {
        Self::with_limits_and_spec(data_dir, capacity, idle_ttl, EmbeddingSpec::local(DEFAULT_DIM))
    }

    pub fn with_limits_and_spec(
        data_dir: PathBuf,
        capacity: usize,
        idle_ttl: Duration,
        default_spec: EmbeddingSpec,
    ) -> Self {
        Self {
            data_dir,
            entries: Mutex::new(HashMap::new()),
            capacity,
            idle_ttl,
            default_spec,
        }
    }

    /// Jumlah project yang saat ini terbuka di memory.
    pub fn active_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn active_pids(&self) -> Vec<String> {
        let mut out: Vec<String> = self.entries.lock().unwrap().keys().cloned().collect();
        out.sort();
        out
    }

    fn open_db(&self, dir: &PathBuf) -> Result<Arc<Hako>> {
        // v0.21.0: lock advisory pada hako.lock — mencegah dua instance
        // (proses/instance berbeda) membuka data dir yang sama.
        Ok(Arc::new(Hako::open_exclusive(
            dir,
            hakodb::config::HakoConfig::default(),
        )?))
    }

    /// Eviksi entry paling lama dipakai yang sudah idle dan tidak masih
    /// dipegang Arc oleh request/watcher lain (strong_count == 1).
    fn sweep(&self, exclude: Option<&str>) {
        if self.capacity == 0 {
            return;
        }
        let mut map = self.entries.lock().unwrap();
        if map.len() <= self.capacity {
            return;
        }
        let mut order: Vec<(String, Instant)> = map
            .iter()
            .map(|(pid, e)| (pid.clone(), e.last))
            .collect();
        order.sort_by_key(|(_, last)| *last);
        let mut evicted = 0usize;
        for (pid, last) in order {
            if map.len() <= self.capacity {
                break;
            }
            if exclude == Some(pid.as_str()) {
                continue;
            }
            if !self.idle_ttl.is_zero() && last.elapsed() < self.idle_ttl {
                continue;
            }
            // jangan eviksi kalau masih dipakai (request/watcher dalam flight)
            if Arc::strong_count(&map[&pid].db) != 1 {
                continue;
            }
            map.remove(&pid);
            evicted += 1;
            tracing::info!(pid = %pid, "LRU: project di-eviksi (idle)");
        }
        if evicted > 0 {
            tracing::info!(evicted, active = map.len(), "LRU sweep selesai");
        }
    }

    /// Validasi spec terhadap data lama; simpan bila project baru.
    fn apply_spec(&self, db: &Hako) -> Result<()> {
        if let Some(existing) = read_embed_spec(db)? {
            if existing.drift(&self.default_spec).is_some() {
                return Err(EmbeddingSpec::drift_error(&existing, &self.default_spec));
            }
        }
        let spec = self.default_spec.clone();
        ensure_schema(db, &spec)?;
        persist_embed_spec(db, &spec)?;
        Ok(())
    }

    /// Buka (atau ambil) instance project. Idempotent dalam proses.
    pub fn open_project(&self, canonical_path: &str) -> Result<(String, Arc<Hako>)> {
        let pid = project_id(canonical_path);
        {
            let mut map = self.entries.lock().unwrap();
            if let Some(entry) = map.get_mut(&pid) {
                entry.last = Instant::now();
                return Ok((pid, entry.db.clone()));
            }
        }
        let dir = self.data_dir.join("projects").join(&pid);
        std::fs::create_dir_all(&dir)?;
        let db = self.open_db(&dir)?;
        self.apply_spec(&db)?;
        persist_root(&db, canonical_path)?;
        self.entries.lock().unwrap().insert(
            pid.clone(),
            Entry {
                db: db.clone(),
                last: Instant::now(),
            },
        );
        self.sweep(Some(&pid));
        Ok((pid, db))
    }

    /// Ambil instance project; lazy-open dari data dir jika belum ada
    /// (robust terhadap restart server — project_id adalah nama direktori).
    pub fn get(&self, pid: &str) -> Result<Arc<Hako>> {
        {
            let mut map = self.entries.lock().unwrap();
            if let Some(entry) = map.get_mut(pid) {
                entry.last = Instant::now();
                return Ok(entry.db.clone());
            }
        }
        let dir = self.data_dir.join("projects").join(pid);
        if !dir.is_dir() {
            return Err(anyhow!("project tidak dikenal: {pid}"));
        }
        let db = self.open_db(&dir)?;
        self.apply_spec(&db)?;
        // recovery index async — tunggu sebelum dipakai untuk query
        let _ = db.await_quiescent(std::time::Duration::from_secs(600));
        self.entries.lock().unwrap().insert(
            pid.to_string(),
            Entry {
                db: db.clone(),
                last: Instant::now(),
            },
        );
        self.sweep(Some(pid));
        Ok(db)
    }
}

/// Simpan root project ke koleksi `meta` (id "root") — dasar rel scan.
/// Koleksi meta tanpa index (hanya get by id).
pub fn persist_root(db: &Hako, canonical_path: &str) -> Result<()> {
    if db.get("meta", "root")?.is_some() {
        return Ok(());
    }
    let mut d = HakoDoc::default();
    d.insert("path", Value::String(canonical_path.to_string()));
    db.write_batch(vec![BatchMutation::Put {
        collection: "meta".into(),
        doc_id: "root".into(),
        doc: d,
    }])?;
    Ok(())
}

/// Baca root project (canonical) dari meta; None jika project lama.
pub fn read_root(db: &Hako) -> Result<Option<String>> {
    Ok(db.get("meta", "root")?.and_then(|d| match d.get("path") {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }))
}

/// Baca spesifikasi embedding yang tersimpan; None untuk project lama.
pub fn read_embed_spec(db: &Hako) -> Result<Option<EmbeddingSpec>> {
    let Some(doc) = db.get("meta", "embedding")? else { return Ok(None) };
    let field = |name: &str| -> Result<String> {
        match doc.get(name) {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(anyhow!("meta embedding field {name} tidak valid")),
        }
    };
    let dim = match doc.get("dim") {
        Some(Value::Int(n)) if *n > 0 => *n as u32,
        _ => return Err(anyhow!("meta embedding dim tidak valid")),
    };
    Ok(Some(EmbeddingSpec {
        model_id: field("model_id")?,
        dim,
        metric: field("metric")?,
        provider: field("provider")?,
    }))
}

/// Simpan spesifikasi embedding (idempotent); konflik dimensi = error keras.
pub fn persist_embed_spec(db: &Hako, spec: &EmbeddingSpec) -> Result<()> {
    if let Some(existing) = read_embed_spec(db)? {
        if existing.drift(spec).is_some() {
            return Err(EmbeddingSpec::drift_error(&existing, spec));
        }
        return Ok(());
    }
    let mut doc = HakoDoc::default();
    doc.insert("model_id", Value::String(spec.model_id.clone()));
    doc.insert("dim", Value::Int(spec.dim as i64));
    doc.insert("metric", Value::String(spec.metric.clone()));
    doc.insert("provider", Value::String(spec.provider.clone()));
    db.write_batch(vec![BatchMutation::Put {
        collection: "meta".into(),
        doc_id: "embedding".into(),
        doc,
    }])?;
    Ok(())
}

/// Buat semua index SEBELUM tulis pertama (kontrak hakodb).
/// Idempotent: aman dipanggil setiap kali instance dibuka.
pub fn ensure_schema(db: &Hako, spec: &EmbeddingSpec) -> Result<()> {
    // v0.21.0: deklarasi skema sekali, idempotent (def sama = no-op).
    let metric = spec.metric()?;
    let schema = IndexSchema {
        fts: std::iter::once(("chunks".to_string(), "content".to_string()))
            .chain(MEMORY_COLLECTIONS.iter().map(|c| (c.to_string(), "text".to_string())))
            .collect(),
        vector: std::iter::once((
            "chunks".to_string(),
            "emb".to_string(),
            spec.dim,
            metric,
        ))
        .chain(MEMORY_COLLECTIONS.iter().map(|c| {
            (c.to_string(), "emb".to_string(), spec.dim, metric)
        }))
        .collect(),
        tracked: vec!["symbols".into(), "edges".into()],
    };
    db.ensure_indexes(&schema).context("ensure_indexes")?;
    // create_edge_index juga membuat index _from/_to (tidak termasuk
    // IndexSchema.tracked). Di v0.21 create_index sudah idempotent.
    db.create_edge_index("edges").context("edge edges")?;
    Ok(())
}

//! Manager instance HakoDB per project + skema koleksi/index + LRU.

use anyhow::{anyhow, Context, Result};
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::{BatchMutation, Hako, IndexSchema};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DIM: u32 = 384;

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
}

impl ProjectStore {
    pub fn new(data_dir: PathBuf) -> Self {
        let capacity = std::env::var("MEMORYBOX_MAX_PROJECTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);
        let idle_ttl = std::env::var("MEMORYBOX_PROJECT_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(1800));
        Self::with_limits(data_dir, capacity, idle_ttl)
    }

    pub fn with_limits(data_dir: PathBuf, capacity: usize, idle_ttl: Duration) -> Self {
        Self {
            data_dir,
            entries: Mutex::new(HashMap::new()),
            capacity,
            idle_ttl,
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
        ensure_schema(&db)?;
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
        ensure_schema(&db)?;
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

/// Buat semua index SEBELUM tulis pertama (kontrak hakodb).
/// Idempotent: aman dipanggil setiap kali instance dibuka.
pub fn ensure_schema(db: &Hako) -> Result<()> {
    // v0.21.0: deklarasi skema sekali, idempotent (def sama = no-op).
    use hakodb::index::vector::Metric;
    let schema = IndexSchema {
        fts: std::iter::once(("chunks".to_string(), "content".to_string()))
            .chain(MEMORY_COLLECTIONS.iter().map(|c| (c.to_string(), "text".to_string())))
            .collect(),
        vector: std::iter::once((
            "chunks".to_string(),
            "emb".to_string(),
            DIM,
            Metric::Cosine,
        ))
        .chain(MEMORY_COLLECTIONS.iter().map(|c| {
            (c.to_string(), "emb".to_string(), DIM, Metric::Cosine)
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

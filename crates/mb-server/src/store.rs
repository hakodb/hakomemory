//! Manager instance HakoDB per project + skema koleksi/index.

use anyhow::{anyhow, Context, Result};
use hakodb::engine::Hako;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

pub struct ProjectStore {
    data_dir: PathBuf,
    instances: Mutex<HashMap<String, Arc<Hako>>>,
}

impl ProjectStore {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir, instances: Mutex::new(HashMap::new()) }
    }

    /// Buka (atau ambil) instance project. Idempotent dalam proses.
    pub fn open_project(&self, canonical_path: &str) -> Result<(String, Arc<Hako>)> {
        let pid = project_id(canonical_path);
        {
            let map = self.instances.lock().unwrap();
            if let Some(h) = map.get(&pid) {
                return Ok((pid, h.clone()));
            }
        }
        let dir = self.data_dir.join("projects").join(&pid);
        std::fs::create_dir_all(&dir)?;
        let db = Arc::new(Hako::open(&dir, hakodb::config::HakoConfig::default())?);
        ensure_schema(&db)?;
        self.instances.lock().unwrap().insert(pid.clone(), db.clone());
        Ok((pid, db))
    }

    /// Ambil instance project; lazy-open dari data dir jika belum ada
    /// (robust terhadap restart server — project_id adalah nama direktori).
    pub fn get(&self, pid: &str) -> Result<Arc<Hako>> {
        {
            let map = self.instances.lock().unwrap();
            if let Some(h) = map.get(pid) {
                return Ok(h.clone());
            }
        }
        let dir = self.data_dir.join("projects").join(pid);
        if !dir.is_dir() {
            return Err(anyhow!("project tidak dikenal: {pid}"));
        }
        let db = Arc::new(Hako::open(&dir, hakodb::config::HakoConfig::default())?);
        ensure_schema(&db)?;
        // recovery index async — tunggu sebelum dipakai untuk query
        let _ = db.await_quiescent(std::time::Duration::from_secs(600));
        self.instances.lock().unwrap().insert(pid.to_string(), db.clone());
        Ok(db)
    }


}

/// Buat semua index SEBELUM tulis pertama (kontrak hakodb).
/// Idempotent: aman dipanggil setiap kali instance dibuka.
pub fn ensure_schema(db: &Hako) -> Result<()> {
    db.create_fts_index("chunks", "content")
        .context("fts chunks")?;
    db.create_vector_index("chunks", "emb", DIM, hakodb::index::vector::Metric::Cosine)
        .context("vector chunks")?;
    db.create_traverse_index("symbols").context("traverse symbols")?;
    db.create_edge_index("edges").context("edge edges")?;
    for col in MEMORY_COLLECTIONS {
        db.create_fts_index(col, "text").with_context(|| format!("fts {col}"))?;
        db.create_vector_index(col, "emb", DIM, hakodb::index::vector::Metric::Cosine)
            .with_context(|| format!("vector {col}"))?;
    }
    Ok(())
}

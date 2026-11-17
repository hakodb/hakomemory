//! Manager instance HakoDB per project + skema koleksi/index.

use anyhow::{anyhow, Context, Result};
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::{BatchMutation, Hako, IndexSchema};
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
        persist_root(&db, canonical_path)?;
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

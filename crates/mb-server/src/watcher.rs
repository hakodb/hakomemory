//! Watcher auto-indexing: pantau root project, debounce event, lalu
//! scan ulang file yang berubah (inkremental, per file).
//!
//! - Hanya file source (ekstensi dikenal + tidak di direktori skip).
//! - Debounce 1.5 s (build tool menyentuh banyak file).
//! - Thread terpisah (notify = std; scan = sync) — tidak menyentuh tokio.

use crate::embed::SharedEmbedder;
use crate::indexer;
use crate::store::ProjectStore;
use anyhow::Result;
use notify::{Event, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEBOUNCE: Duration = Duration::from_millis(1500);

/// Registry watcher agar satu project tidak dipantau dobel.
#[derive(Default)]
pub struct WatchRegistry {
    watched: Mutex<HashSet<String>>,
}

impl WatchRegistry {
    pub fn ensure(&self, pid: &str) -> bool {
        self.watched.lock().unwrap().insert(pid.to_string())
    }
}

fn path_ignored(path: &Path) -> bool {
    for comp in path.components() {
        let s = comp.as_os_str().to_string_lossy();
        if matches!(
            s.as_ref(),
            ".git" | ".hg" | ".svn" | "target" | "node_modules" | "vendor" | "dist"
            | "build" | "__pycache__" | ".venv" | "data"
        ) {
            return true;
        }
    }
    false
}

/// Create/Modify → file yang masih ada; Remove → file source yang hilang
/// (scan nanti melakukan cleanup murni untuk rel-nya).
fn event_paths(ev: &Event) -> Vec<PathBuf> {
    match ev.kind {
        notify::EventKind::Create(_) | notify::EventKind::Modify(_) => ev
            .paths
            .iter()
            .filter(|p| p.is_file())
            .filter(|p| !path_ignored(p))
            .filter(|p| indexer::is_source(p))
            .filter(|p| p.metadata().map(|m| m.len() <= 256 * 1024).unwrap_or(false))
            .cloned()
            .collect(),
        notify::EventKind::Remove(_) => ev
            .paths
            .iter()
            .filter(|p| !path_ignored(p))
            .filter(|p| indexer::is_source(p))
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// Mulai watcher untuk satu project. Dipanggil dari handler async tapi
/// membentuk thread std sendiri.
pub fn spawn(
    store: Arc<ProjectStore>,
    embedder: Arc<SharedEmbedder>,
    pid: String,
    root: PathBuf,
) -> Result<()> {
    std::thread::Builder::new()
        .name(format!("mb-watch-{pid}"))
        .spawn(move || run(store, embedder, pid, root))?;
    Ok(())
}

fn run(store: Arc<ProjectStore>, embedder: Arc<SharedEmbedder>, pid: String, root: PathBuf) {
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let Ok(mut watcher) = notify::recommended_watcher(tx) else {
        return;
    };
    if watcher.watch(&root, RecursiveMode::Recursive).is_err() {
        return;
    }
    // buka instance sekali di awal (lazy-open dipakai ulang oleh get)
    let Ok(db) = store.get(&pid) else { return };

    let mut pending: HashSet<PathBuf> = HashSet::new();
    loop {
        match rx.recv_timeout(DEBOUNCE) {
            Ok(Ok(ev)) => {
                for p in event_paths(&ev) {
                    pending.insert(p);
                }
            }
            Ok(Err(_)) => {} // notify error: abaikan
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if pending.is_empty() {
                    continue;
                }
                let files: Vec<PathBuf> = pending.drain().collect();
                let mut ok = 0usize;
                for f in &files {
                    match indexer::scan(&db, &embedder, &root, f) {
                        Ok(_) => ok += 1,
                        Err(e) => {
                            tracing::warn!("watcher scan {}: {e}", f.display())
                        }
                    }
                }
                if !files.is_empty() {
                    tracing::info!(pid = %pid, files = ok, "watcher re-scan selesai");
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

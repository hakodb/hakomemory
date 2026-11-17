//! State aplikasi: dibuka HAKO-DULU (pemilik pool rayon global), baru
//! embedder candle — urutan ini terkunci, lihat docs/PLAN.md §0.2.

use crate::auth::{AuthContext, AuthConfig};
use crate::embed::SharedEmbedder;
use crate::store::ProjectStore;
use crate::watcher;
use anyhow::{bail, Context, Result};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Home + nama subdirektori cache (khusus unix).
fn dirs_or_default(sub: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".cache").join(sub).join("hub"))
}

/// Rate limiter sederana (fixed window 60 detik) per identitas user.
pub struct RateLimiter {
    pub limit: u32, // 0 = mati
    hits: Mutex<HashMap<String, (u64, u32)>>,
}

impl RateLimiter {
    pub fn new(limit: u32) -> Self {
        Self { limit, hits: Mutex::new(HashMap::new()) }
    }

    pub fn allow(&self, sub: &str) -> bool {
        if self.limit == 0 {
            return true;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let window = now / 60;
        let mut map = self.hits.lock().unwrap();
        let entry = map.entry(sub.to_string()).or_insert((window, 0));
        if entry.0 != window {
            *entry = (window, 0);
        }
        if entry.1 >= self.limit {
            false
        } else {
            entry.1 += 1;
            true
        }
    }
}

/// Alias project per user + peta pid → root. Disimpan server-side supaya
/// klien tidak bisa memilih `pid` sembarangan (Fase 3).
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct AliasRegistry {
    /// kunci: "<sub>/<alias>" → entri project
    pub by_user: HashMap<String, AliasEntry>,
    /// pid → path canonical (dipakai otorisasi project)
    pub by_pid: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct AliasEntry {
    pub pid: String,
    pub path: String,
}

impl AliasRegistry {
    fn file(data_dir: &Path) -> PathBuf {
        data_dir.join("project_aliases.json")
    }

    fn load(data_dir: &Path) -> Self {
        std::fs::read_to_string(Self::file(data_dir))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, data_dir: &Path) -> Result<()> {
        let file = Self::file(data_dir);
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &file).context("simpan project_aliases.json")?;
        Ok(())
    }
}

pub struct AppState {
    pub store: Arc<ProjectStore>,
    pub embedder: Arc<SharedEmbedder>,
    pub watches: watcher::WatchRegistry,
    pub auth: Arc<AuthConfig>,
    pub aliases: Mutex<AliasRegistry>,
    pub revoked: Mutex<HashSet<String>>,
    pub rate: RateLimiter,
    data_dir: PathBuf,
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
        Ok(Self::with_embedder(
            data_dir,
            store,
            embedder,
            Arc::new(AuthConfig::from_env()),
            rate_limit_from_env(),
        ))
    }

    /// Konstruktor test: embedder mock, tanpa model/unduhan.
    pub fn open_mock(data_dir: &Path) -> Result<Arc<Self>> {
        Self::open_mock_with_auth(data_dir, AuthConfig::Disabled)
    }

    pub fn open_mock_with_auth(data_dir: &Path, auth: AuthConfig) -> Result<Arc<Self>> {
        Self::open_mock_with_limits(data_dir, auth, 0)
    }

    pub fn open_mock_with_limits(
        data_dir: &Path,
        auth: AuthConfig,
        rate_per_min: u32,
    ) -> Result<Arc<Self>> {
        let store = Arc::new(ProjectStore::new(data_dir.to_path_buf()));
        let embedder = Arc::new(SharedEmbedder::mock(crate::store::DIM));
        Ok(Self::with_embedder(
            data_dir,
            store,
            embedder,
            Arc::new(auth),
            RateLimiter::new(rate_per_min),
        ))
    }

    fn with_embedder(
        data_dir: &Path,
        store: Arc<ProjectStore>,
        embedder: Arc<SharedEmbedder>,
        auth: Arc<AuthConfig>,
        rate: RateLimiter,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            embedder,
            watches: watcher::WatchRegistry::default(),
            auth,
            rate,
            aliases: Mutex::new(AliasRegistry::load(data_dir)),
            revoked: Mutex::new(load_revoked(data_dir)),
            data_dir: data_dir.to_path_buf(),
        })
    }

    /// Daftarkan pid→root dan (opsional) alias milik user, lalu simpan.
    pub fn register_project(
        &self,
        sub: &str,
        alias: Option<&str>,
        pid: &str,
        path: &str,
    ) -> Result<()> {
        let mut reg = self.aliases.lock().unwrap();
        reg.by_pid.insert(pid.to_string(), path.to_string());
        if let Some(alias) = alias {
            reg.by_user.insert(
                format!("{sub}/{alias}"),
                AliasEntry {
                    pid: pid.to_string(),
                    path: path.to_string(),
                },
            );
        }
        reg.save(&self.data_dir)
    }

    pub fn resolve_alias(&self, sub: &str, alias: &str) -> Option<AliasEntry> {
        self.aliases
            .lock()
            .unwrap()
            .by_user
            .get(&format!("{sub}/{alias}"))
            .cloned()
    }

    /// Pid diizinkan kalau ada di klaim token ATAU root-nya (dari registry)
    /// berada di bawah root yang diizinkan user.
    pub fn authorize_project(&self, auth: &AuthContext, pid: &str) -> Result<(), StatusCode> {
        if auth.admin {
            return Ok(());
        }
        // root dari registry lebih dulu: token hanya berisi root pun tetap
        // bisa mengakses project yang path-nya berada di bawah root itu.
        let root = self.aliases.lock().unwrap().by_pid.get(pid).cloned();
        if let Some(root) = root {
            if auth.require_path(&root).is_ok() {
                return Ok(());
            }
        }
        auth.require_project(pid)
    }
}

/// Daftar kunci token yang dicabut, dipersisten sebagai JSON sederhana.
fn load_revoked(data_dir: &Path) -> HashSet<String> {
    std::fs::read_to_string(data_dir.join("revoked_tokens.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

impl AppState {
    pub fn revoke_token(&self, key: String) -> Result<()> {
        let mut set = self.revoked.lock().unwrap();
        set.insert(key);
        let tmp = self.data_dir.join("revoked_tokens.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&*set)?)?;
        std::fs::rename(&tmp, self.data_dir.join("revoked_tokens.json"))?;
        Ok(())
    }
}

fn rate_limit_from_env() -> RateLimiter {
    let limit = std::env::var("MEMORYBOX_RATE_PER_MIN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    RateLimiter::new(limit)
}

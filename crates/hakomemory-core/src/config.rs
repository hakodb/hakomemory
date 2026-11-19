//! File-based configuration for the server adapter.
//!
//! The library facade (`EngineConfig`) needs no config file and no env vars.
//! This module is for the daemon: an optional JSON file with explicit values;
//! environment variables remain opt-in overrides applied by the server.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_FILE: &str = "hakomemory.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub data_dir: Option<PathBuf>,
    /// Embedding model id (local candle backend).
    pub model: Option<String>,
    /// `local` or `http`.
    pub provider: String,
    pub dim: u32,
    pub metric: String,
    pub max_projects: usize,
    pub idle_ttl_secs: u64,
    /// Optional cache directory for downloaded models.
    pub model_cache: Option<PathBuf>,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            model: None,
            provider: "local".into(),
            dim: 384,
            metric: "Cosine".into(),
            max_projects: 8,
            idle_ttl_secs: 1800,
            model_cache: None,
        }
    }
}

impl FileConfig {
    /// Load JSON config. A missing file is not an error (defaults are used).
    pub fn load(path: &Path) -> Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading config {}", path.display()))
            }
        };
        let config: Self =
            serde_json::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
        if config.dim == 0 || config.dim > 4096 {
            anyhow::bail!("dim must be 1..=4096 in {}", path.display());
        }
        if config.max_projects == 0 {
            anyhow::bail!("max_projects must be > 0 in {}", path.display());
        }
        Ok(config)
    }

    pub fn load_default_in(dir: &Path) -> Result<Self> {
        Self::load(&dir.join(DEFAULT_CONFIG_FILE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_roundtrip_and_validation() {
        let dir = std::env::temp_dir().join(format!(
            "hakomemory-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DEFAULT_CONFIG_FILE);
        std::fs::write(
            &path,
            r#"{"model":"intfloat/multilingual-e5-small","dim":768,"max_projects":4}"#,
        )
        .unwrap();
        let config = FileConfig::load(&path).unwrap();
        assert_eq!(config.dim, 768);
        assert_eq!(config.max_projects, 4);
        assert_eq!(config.model.as_deref(), Some("intfloat/multilingual-e5-small"));
        assert_eq!(config.metric, "Cosine"); // default merged
        std::fs::write(&path, r#"{"dim":0}"#).unwrap();
        assert!(FileConfig::load(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

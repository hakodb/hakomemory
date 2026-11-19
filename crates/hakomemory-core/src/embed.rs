//! Embedder bersama (dipakai semua project) — Mutex karena model
//! BertModel::forward &mut self (dropout span dsb.). Bisa memakai backend
//! candle asli atau mock deterministik untuk integration test.

use anyhow::{anyhow, Result};
use hakomemory_embed::{Embedder, HttpEmbedder, MockEmbedder, TextEmbedder};
pub use hakomemory_embed::model_from_env;
use std::path::Path;
use std::sync::Mutex;

pub struct SharedEmbedder {
    inner: Mutex<Box<dyn TextEmbedder>>,
}

impl SharedEmbedder {
    pub fn load(cache_dir: &Path) -> Result<Self> {
        // Mode HTTP: embedder tidak terikat proses (bisa di box lain / adapter).
        if let Ok(url) = std::env::var("MEMORYBOX_EMBED_URL") {
            if !url.trim().is_empty() {
                let dim: u32 = std::env::var("MEMORYBOX_EMBED_DIM")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| anyhow!("MEMORYBOX_EMBED_DIM wajib saat MEMORYBOX_EMBED_URL diset"))?;
                let token = std::env::var("MEMORYBOX_EMBED_TOKEN").ok().filter(|t| !t.is_empty());
                let inner = HttpEmbedder::new(&url, token, dim);
                return Ok(Self { inner: Mutex::new(Box::new(inner)) });
            }
        }
        let inner = Embedder::load(&hakomemory_embed::model_from_env(), Some(cache_dir))?;
        Ok(Self { inner: Mutex::new(Box::new(inner)) })
    }

    /// HTTP backend chosen explicitly from config (no env lookup).
    pub fn http(url: &str, token: Option<String>, dim: u32) -> Result<Self> {
        let inner = HttpEmbedder::new(url, token, dim);
        Ok(Self { inner: Mutex::new(Box::new(inner)) })
    }

    /// Load a model chosen by config (no environment lookup).
    pub fn load_model(model: &str, cache_dir: &Path) -> Result<Self> {
        let inner = Embedder::load(model, Some(cache_dir))?;
        Ok(Self { inner: Mutex::new(Box::new(inner)) })
    }

    /// Backend test: vektor hash deterministik, tanpa model/unduhan.
    pub fn mock(dim: u32) -> Self {
        Self { inner: Mutex::new(Box::new(MockEmbedder::new(dim as usize))) }
    }

    pub fn dim(&self) -> u32 {
        self.inner.lock().unwrap().dim() as u32
    }

    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        self.inner.lock().unwrap().embed_one(text)
    }

    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.inner.lock().unwrap().embed(texts)
    }
}

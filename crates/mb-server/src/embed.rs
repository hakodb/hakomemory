//! Embedder bersama (dipakai semua project) — Mutex karena model
//! BertModel::forward &mut self (dropout span dsb.). Bisa memakai backend
//! candle asli atau mock deterministik untuk integration test.

use anyhow::Result;
use mb_embed::{Embedder, MockEmbedder, TextEmbedder};
use std::path::Path;
use std::sync::Mutex;

pub struct SharedEmbedder {
    inner: Mutex<Box<dyn TextEmbedder>>,
}

impl SharedEmbedder {
    pub fn load(cache_dir: &Path) -> Result<Self> {
        let inner = Embedder::load(&mb_embed::model_from_env(), Some(cache_dir))?;
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

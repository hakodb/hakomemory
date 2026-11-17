//! mb-embed: embedder teks lokal (pure Rust, backend candle).
//!
//! Model default: paraphrase-multilingual-MiniLM-L12-v2 (384 dim,
//! mean pooling + L2 normalization) — bilingual ID/EN. Bisa diganti lewat
//! MEMORYBOX_MODEL=multi|en|<repo-id>. Model diunduh sekali via hf-hub
//! (cache lokal), lalu inferensi offline.

use anyhow::{anyhow, Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use hf_hub::api::sync::{Api, ApiBuilder};
use tokenizers::Tokenizer;

pub const MINILM_L6_V2: &str = "sentence-transformers/all-MiniLM-L6-v2";
/// Model multibahasa (Indonesia didukung baik) — dim 384 juga, arsitektur
/// BERT standar (mean pooling), tidak gated di HF.
pub const MINILM_MULTI: &str = "sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2";

/// Resolusi model dari env: `MEMORYBOX_MODEL=multi|en|<repo-id-hf>`.
/// Default `multi` (bilingual); `en` = all-MiniLM-L6-v2 untuk repo full-English.
pub fn model_from_env() -> String {
    match std::env::var("MEMORYBOX_MODEL").as_deref() {
        Ok("en") => MINILM_L6_V2.to_string(),
        Ok("multi") | Err(_) => MINILM_MULTI.to_string(),
        Ok(other) => other.to_string(),
    }
}

/// Abstraksi embedder — dipakai backend candle maupun mock deterministik
/// untuk integration test (tanpa unduh model).
pub trait TextEmbedder: Send {
    fn dim(&self) -> usize;
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn embed_one(&mut self, text: &str) -> Result<Vec<f32>>;
}

impl TextEmbedder for Embedder {
    fn dim(&self) -> usize { self.dim }
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Embedder::embed(self, texts)
    }
    fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        Embedder::embed_one(self, text)
    }
}

/// Embedder deterministik berbasis hash kata (L2-normalized).
/// Hanya untuk test — tidak kompatibel dengan vektor model asli.
pub struct MockEmbedder {
    pub dim: usize,
}

impl MockEmbedder {
    pub fn new(dim: usize) -> Self { Self { dim } }
}

impl TextEmbedder for MockEmbedder {
    fn dim(&self) -> usize { self.dim }
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed_one(t)).collect()
    }
    fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0f32; self.dim];
        for word in text.split_whitespace() {
            let mut h = 2166136261u32; // FNV-1a
            for b in word.bytes() {
                h ^= b as u32;
                h = h.wrapping_mul(16777619);
            }
            v[h as usize % self.dim] += 1.0;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v { *x /= norm; }
        } else {
            v[0] = 1.0;
        }
        Ok(v)
    }
}

pub struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    pub dim: usize,
}

impl Embedder {
    /// Muat model dari cache HF (unduh sekali). `cache_dir` opsional.
    pub fn load(model_id: &str, cache_dir: Option<&std::path::Path>) -> Result<Self> {
        let api: Api = match cache_dir {
            Some(dir) => ApiBuilder::new()
                .with_cache_dir(dir.to_path_buf())
                .build()
                .map_err(|e| anyhow!("hf api: {e}"))?,
            None => Api::new().map_err(|e| anyhow!("hf api: {e}"))?,
        };
        let repo = api.model(model_id.to_string());
        let config_path = repo.get("config.json").context("config.json")?;
        let tokenizer_path = repo.get("tokenizer.json").context("tokenizer.json")?;
        let weights_path = repo.get("model.safetensors").context("model.safetensors")?;

        let device = Device::Cpu;
        let config: Config = serde_json::from_slice(&std::fs::read(config_path)?)?;
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow!("tokenizer: {e}"))?;

        // BertModel::load auto-deteksi prefix bobot ("bert." / model_type / root).
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, &device)?
        };
        let model = BertModel::load(vb, &config)?;
        Ok(Self { model, tokenizer, device, dim: config.hidden_size })
    }

    /// Embed sejumlah teks dalam SATU forward pass (padding ke panjang
    /// maksimum batch), lalu mean pooling per-baris + L2 normalize.
    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        const MAX_LEN: usize = 512;
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow!("encode batch: {e}"))?;
        let batch = texts.len();
        let seq = encodings
            .iter()
            .map(|e| e.get_ids().len().min(MAX_LEN))
            .max()
            .unwrap_or(1)
            .max(1);
        let pad_id = self
            .tokenizer
            .token_to_id("[PAD]")
            .or_else(|| self.tokenizer.token_to_id("<pad>"))
            .unwrap_or(0);

        let mut ids = Vec::with_capacity(batch * seq);
        let mut attn = Vec::with_capacity(batch * seq);
        let mut types = Vec::with_capacity(batch * seq);
        for enc in &encodings {
            let n = enc.get_ids().len().min(MAX_LEN);
            ids.extend_from_slice(&enc.get_ids()[..n]);
            ids.resize(ids.len() + seq - n, pad_id);
            attn.extend_from_slice(&enc.get_attention_mask()[..n]);
            attn.resize(attn.len() + seq - n, 0);
            types.extend_from_slice(&enc.get_type_ids()[..n]);
            types.resize(types.len() + seq - n, 0);
        }

        let ids_t = Tensor::new(ids.as_slice(), &self.device)?.reshape((batch, seq))?;
        let attn_t = Tensor::new(attn.as_slice(), &self.device)?.reshape((batch, seq))?;
        let types_t = Tensor::new(types.as_slice(), &self.device)?.reshape((batch, seq))?;

        let hidden = self.model.forward(&ids_t, &types_t, Some(&attn_t))?;
        // mean pooling dengan mask: (batch, seq, hidden) -> (batch, hidden)
        let mask_f = attn_t.to_dtype(DType::F32)?;
        let mask3 = mask_f.unsqueeze(2)?;
        let masked = hidden.broadcast_mul(&mask3)?;
        let summed = masked.sum_keepdim(1)?;
        let count = mask_f.sum_keepdim(1)?.unsqueeze(2)?;
        let mean = summed.broadcast_div(&count)?.squeeze(1)?;
        // L2 normalize per baris
        let norm = mean.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normed = mean.broadcast_div(&norm)?;
        let rows = normed.to_vec2::<f32>()?;
        debug_assert!(rows.iter().all(|v| v.len() == self.dim));
        Ok(rows)
    }

    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        let tokens = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow!("encode: {e}"))?;
        let ids = tokens.get_ids().to_vec();
        let attn = tokens.get_attention_mask().to_vec();
        let types = tokens.get_type_ids().to_vec();
        let token_ids = Tensor::new(&ids[..], &self.device)?.unsqueeze(0)?; // (1, seq)
        let attn_t = Tensor::new(&attn[..], &self.device)?.unsqueeze(0)?; // (1, seq)
        let types_t = Tensor::new(&types[..], &self.device)?.unsqueeze(0)?; // (1, seq)

        // (1, seq, hidden)
        let hidden = self.model.forward(&token_ids, &types_t, Some(&attn_t))?;

        // mean pooling dengan mask: sum(h * mask) / sum(mask)
        let mask_f = attn_t.to_dtype(DType::F32)?;
        let mask3 = mask_f.unsqueeze(2)?; // (1, seq, 1)
        let masked = hidden.broadcast_mul(&mask3)?;
        let summed = masked.sum_keepdim(1)?; // (1, 1, hidden)
        let count = mask_f.sum_keepdim(1)?.unsqueeze(2)?; // (1, 1, 1)
        let mean = summed.broadcast_div(&count)?.squeeze(1)?.squeeze(0)?; // (hidden,)

        // L2 normalize manual
        let sq = mean.sqr()?;
        let denom = sq.sum_all()?.sqrt()?.reshape((1,))?; // (1,)
        let norm = mean.reshape((1, self.dim))?.broadcast_div(&denom)?.squeeze(0)?; // (hidden,)
        let v = norm.to_vec1::<f32>()?;
        debug_assert_eq!(v.len(), self.dim);
        Ok(v)
    }
}

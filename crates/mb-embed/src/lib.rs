//! mb-embed: embedder teks lokal (pure Rust, backend candle).
//!
//! Model default: sentence-transformers/all-MiniLM-L6-v2 (384 dim,
//! mean pooling + L2 normalization). Model diunduh sekali via hf-hub
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

    /// Embed sejumlah teks: mean pooling + L2 normalize. Satu teks per
    /// forward pass (POC); batching jadi pekerjaan fase 2.
    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.embed_one(text)?);
        }
        Ok(out)
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

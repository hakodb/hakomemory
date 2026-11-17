//! Backend embedder via HTTP (self-hosted atau adapter vendor).
//! Protokol internal memory-box: POST {base}/embed {"texts":[...]} ->
//! {"dim":N,"vectors":[[f32,...],...]}; GET {base}/dim -> {"dim":N}.

use crate::TextEmbedder;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::net::TcpStream;

pub struct HttpEmbedder {
    base: String,
    token: Option<String>,
    dim: usize,
}

impl HttpEmbedder {
    pub fn new(base: &str, token: Option<String>, dim: u32) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token,
            dim: dim as usize,
        }
    }

    /// Probe dimensi dari endpoint /dim (opsional; dipakai saat startup).
    pub fn probe_dim(&self) -> Result<u32> {
        let value = self.request("GET", "/dim", &json!({}))?;
        let d = value
            .get("dim")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("respons /dim tanpa field dim"))?;
        Ok(d as u32)
    }

    fn request(&self, method: &str, path: &str, body: &Value) -> Result<Value> {
        let rest = self
            .base
            .strip_prefix("http://")
            .ok_or_else(|| anyhow!("MEMORYBOX_EMBED_URL harus http://host[:port][/prefix]"))?;
        let (authority, prefix) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let payload = body.to_string();
        let auth = self
            .token
            .as_ref()
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let req = format!(
            "{method} {prefix}{path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{auth}Connection: close\r\n\r\n{payload}",
            payload.len()
        );
        let mut stream = TcpStream::connect(authority)
            .map_err(|e| anyhow!("connect embedder {authority}: {e}"))?;
        use std::io::Write;
        stream.write_all(req.as_bytes())?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        let text = String::from_utf8_lossy(&raw);
        let status = text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| anyhow!("respons HTTP tanpa status"))?;
        if !(200..300).contains(&status) {
            return Err(anyhow!("embedder HTTP {status}: {}", clip(&text)));
        }
        let body_text = text.splitn(2, "\r\n\r\n").nth(1).unwrap_or("");
        let value: Value = serde_json::from_str(body_text)
            .map_err(|e| anyhow!("respons embedder bukan JSON: {e}: {}", clip(body_text)))?;
        if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
            return Err(anyhow!("embedder: {error}"));
        }
        Ok(value)
    }
}

fn clip(s: &str) -> String {
    s.chars().take(200).collect()
}

impl TextEmbedder for HttpEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let value = self.request("POST", "/embed", &json!({ "texts": texts }))?;
        if let Some(d) = value.get("dim").and_then(|v| v.as_u64()) {
            if d as usize != self.dim {
                return Err(anyhow!(
                    "dimensi embedder {} != konfigurasi {}",
                    d,
                    self.dim
                ));
            }
        }
        let arr = value
            .get("vectors")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("respons /embed tanpa field vectors"))?;
        if arr.len() != texts.len() {
            return Err(anyhow!(
                "embedder mengembalikan {} vektor untuk {} teks",
                arr.len(),
                texts.len()
            ));
        }
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            let nums = item
                .as_array()
                .ok_or_else(|| anyhow!("vektor bukan array"))?;
            if nums.len() != self.dim {
                return Err(anyhow!(
                    "dimensi vektor {} != {}",
                    nums.len(),
                    self.dim
                ));
            }
            let mut v = Vec::with_capacity(nums.len());
            for n in nums {
                v.push(
                    n.as_f64()
                        .ok_or_else(|| anyhow!("elemen vektor bukan angka"))?
                        as f32,
                );
            }
            out.push(v);
        }
        Ok(out)
    }

    fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        let mut rows = self.embed(&[text.to_string()])?;
        Ok(rows.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::clip;

    #[test]
    fn clip_membatasi_panjang() {
        assert_eq!(clip(&"x".repeat(500)).len(), 200);
    }
}

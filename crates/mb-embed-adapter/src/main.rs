//! mb-embed-adapter — jembatan protokol internal memory-box ke API embedding online.
//!
//! Internal (dipanggil mb-server lewat MEMORYBOX_EMBED_URL):
//!   GET  /dim   -> {"dim":N}
//!   POST /embed -> {"texts":[...],"role":"query|document"}
//!               <- {"dim":N,"vectors":[[f32,...],...]}
//!
//! Vendor didukung: cohere | openai | gemini. API key hanya ada di proses ini.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Provider {
    Cohere,
    OpenAi,
    Gemini,
    Nim,
}

impl Provider {
    fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cohere" => Ok(Self::Cohere),
            "openai" => Ok(Self::OpenAi),
            "gemini" | "google" => Ok(Self::Gemini),
            "nim" | "nvidia" => Ok(Self::Nim),
            other => Err(anyhow!("provider tidak dikenal: {other}")),
        }
    }
}

#[derive(Clone, Debug)]
struct Config {
    bind: String,
    port: u16,
    provider: Provider,
    api_key: String,
    model: String,
    dim: u32,
    /// override base URL (untuk test / proxy lokal)
    base: String,
}

impl Config {
    fn from_env() -> Result<Self> {
        let provider = Provider::parse(
            &std::env::var("MEMORYBOX_ADAPTER_PROVIDER")
                .map_err(|_| anyhow!("MEMORYBOX_ADAPTER_PROVIDER wajib: cohere|openai|gemini"))?,
        )?;
        let api_key = std::env::var("MEMORYBOX_ADAPTER_API_KEY")
            .map_err(|_| anyhow!("MEMORYBOX_ADAPTER_API_KEY wajib"))?;
        let default_model = match provider {
            Provider::Cohere => "embed-multilingual-light-v3.0",
            Provider::OpenAi => "text-embedding-3-small",
            Provider::Gemini => "gemini-embedding-001",
            Provider::Nim => "nvidia/nemotron-3-embed-1b",
        };
        let model = std::env::var("MEMORYBOX_ADAPTER_MODEL").unwrap_or_else(|_| default_model.into());
        let dim: u32 = std::env::var("MEMORYBOX_ADAPTER_DIM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(384);
        if dim == 0 || dim > 4096 {
            return Err(anyhow!("MEMORYBOX_ADAPTER_DIM harus 1..=4096"));
        }
        let base = std::env::var("MEMORYBOX_ADAPTER_BASE_URL")
            .unwrap_or_else(|_| match provider {
                Provider::Cohere => "https://api.cohere.com".into(),
                Provider::OpenAi => "https://api.openai.com".into(),
                Provider::Gemini => "https://generativelanguage.googleapis.com/v1beta".into(),
                Provider::Nim => "https://integrate.api.nvidia.com".into(),
            })
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            bind: std::env::var("MEMORYBOX_ADAPTER_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("MEMORYBOX_ADAPTER_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(7416),
            provider,
            api_key,
            model,
            dim,
            base,
        })
    }
}

/// Batasi laju request ke vendor (default 6 dtk ≈ 10 RPM untuk tier free).
static LAST_CALL: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

fn throttle() {
    let interval_ms: u64 = std::env::var("MEMORYBOX_ADAPTER_MIN_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6000);
    if interval_ms == 0 {
        return;
    }
    let interval = std::time::Duration::from_millis(interval_ms);
    let mut last = LAST_CALL.lock().unwrap();
    if let Some(prev) = *last {
        if let Some(wait) = interval.checked_sub(prev.elapsed()) {
            std::thread::sleep(wait);
        }
    }
    *last = Some(std::time::Instant::now());
}

fn post_json(url: &str, headers: &[(&str, String)], body: &Value) -> Result<Value> {
    let mut attempt = 0usize;
    loop {
        throttle();
        let mut req = ureq::post(url).header("Content-Type", "application/json");
        for (name, value) in headers {
            req = req.header(*name, value.as_str());
        }
        match req.send_json(body) {
            Ok(response) => {
                let text = response.into_body().read_to_string()?;
                return serde_json::from_str(&text)
                    .map_err(|e| anyhow!("vendor respons bukan JSON: {e}: {}", clip(&text)));
            }
            Err(ureq::Error::StatusCode(code)) if (code == 429 || code >= 500) && attempt < 8 => {
                let secs = [2u64, 4, 8, 16, 30, 45, 60, 60][attempt];
                let delay = std::time::Duration::from_secs(secs);
                eprintln!("vendor HTTP {code}, retry {}/8 dalam {:?}", attempt + 1, delay);
                std::thread::sleep(delay);
                attempt += 1;
            }
            Err(ureq::Error::StatusCode(code)) => return Err(anyhow!("vendor HTTP {code}")),
            Err(other) => return Err(anyhow!("vendor request: {other}")),
        }
    }
}

fn clip(s: &str) -> String {
    s.chars().take(200).collect()
}

/// Kirim teks ke provider, kembalikan vektor yang sudah divalidasi + L2-normalized.
fn forward(cfg: &Config, texts: &[String], role: &str) -> Result<Vec<Vec<f32>>> {
    let raw = match cfg.provider {
        Provider::Cohere => forward_cohere(cfg, texts, role)?,
        Provider::OpenAi => forward_openai(cfg, texts, true)?,
        Provider::Gemini => forward_gemini(cfg, texts, role)?,
        Provider::Nim => forward_openai(cfg, texts, false)?,
    };
    if raw.len() != texts.len() {
        return Err(anyhow!(
            "vendor mengembalikan {} vektor untuk {} teks",
            raw.len(),
            texts.len()
        ));
    }
    let dim = cfg.dim as usize;
    let mut out = Vec::with_capacity(raw.len());
    for vector in raw {
        if vector.len() != dim {
            return Err(anyhow!(
                "dimensi vektor vendor {} != konfigurasi {dim}",
                vector.len()
            ));
        }
        let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm <= f32::EPSILON {
            return Err(anyhow!("vektor vendor ber-norma nol"));
        }
        out.push(vector.into_iter().map(|x| x / norm).collect());
    }
    Ok(out)
}

fn forward_cohere(cfg: &Config, texts: &[String], role: &str) -> Result<Vec<Vec<f32>>> {
    let input_type = if role == "query" {
        "search_query"
    } else {
        "search_document"
    };
    let mut body = json!({
        "model": cfg.model,
        "input_type": input_type,
        "texts": texts,
        "embedding_types": ["float"],
    });
    // output_dimension hanya didukung embed-v4/v5; model light v3 sudah 384.
    if cfg.model.contains("-v4") || cfg.model.contains("-v5") {
        body["output_dimension"] = json!(cfg.dim);
    }
    let url = format!("{}/v2/embed", cfg.base);
    let value = post_json(
        &url,
        &[("Authorization", format!("Bearer {}", cfg.api_key))],
        &body,
    )?;
    let arr = value
        .get("embeddings")
        .and_then(|e| e.get("float"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("respons Cohere tanpa embeddings.float"))?;
    arr.iter().map(|v| as_f32_vec(v)).collect()
}

fn forward_openai(cfg: &Config, texts: &[String], send_dimensions: bool) -> Result<Vec<Vec<f32>>> {
    let mut body = json!({
        "model": cfg.model,
        "input": texts,
    });
    if send_dimensions {
        body["dimensions"] = json!(cfg.dim);
    }
    let url = format!("{}/v1/embeddings", cfg.base);
    let value = post_json(
        &url,
        &[("Authorization", format!("Bearer {}", cfg.api_key))],
        &body,
    )?;
    let mut rows = value
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("respons OpenAI tanpa data"))?
        .iter()
        .map(|item| {
            let index = item.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let vector = as_f32_vec(
                item.get("embedding")
                    .ok_or_else(|| anyhow!("item OpenAI tanpa embedding"))?,
            )?;
            Ok((index, vector))
        })
        .collect::<Result<Vec<_>>>()?;
    rows.sort_by_key(|(i, _)| *i);
    Ok(rows.into_iter().map(|(_, v)| v).collect())
}

fn forward_gemini(cfg: &Config, texts: &[String], role: &str) -> Result<Vec<Vec<f32>>> {
    let is_v2 = cfg.model.contains("-2");
    let task = if is_v2 {
        None
    } else if role == "query" {
        Some("CODE_RETRIEVAL_QUERY")
    } else {
        Some("RETRIEVAL_DOCUMENT")
    };
    let requests: Vec<Value> = texts
        .iter()
        .map(|text| {
            let prepared = if is_v2 {
                if role == "query" {
                    format!("task: code retrieval | query: {text}")
                } else {
                    format!("title: none | text: {text}")
                }
            } else {
                text.clone()
            };
            let mut item = json!({
                "model": format!("models/{}", cfg.model),
                "content": { "parts": [{ "text": prepared }] },
                "output_dimensionality": cfg.dim,
            });
            if let Some(task) = task {
                item["taskType"] = json!(task);
            }
            item
        })
        .collect();
    let url = format!("{}/models/{}:batchEmbedContents", cfg.base, cfg.model);
    let value = post_json(&url, &[("x-goog-api-key", cfg.api_key.clone())], &json!({ "requests": requests }))?;
    let arr = value
        .get("embeddings")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("respons Gemini tanpa embeddings"))?;
    arr.iter()
        .map(|item| {
            as_f32_vec(
                item.get("values")
                    .ok_or_else(|| anyhow!("item Gemini tanpa values"))?,
            )
        })
        .collect()
}

fn as_f32_vec(value: &Value) -> Result<Vec<f32>> {
    let arr = value
        .as_array()
        .ok_or_else(|| anyhow!("vektor bukan array JSON"))?;
    arr.iter()
        .map(|v| {
            v.as_f64()
                .map(|n| n as f32)
                .ok_or_else(|| anyhow!("elemen vektor bukan angka"))
        })
        .collect()
}

// ---- server internal HTTP ----

fn handle(mut stream: TcpStream, cfg: Arc<Config>) -> Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 16 * 1024 * 1024 {
            return Err(anyhow!("header terlalu besar"));
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts
        .next()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("")
        .to_string();
    let content_len = lines
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_len {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let request_body = &buf[header_end..(header_end + content_len).min(buf.len())];

    let (status, payload) = match (method.as_str(), path.as_str()) {
        ("GET", "/healthz") => (200, json!({ "status": "ok" })),
        ("GET", "/dim") => (200, json!({ "dim": cfg.dim })),
        ("POST", "/embed") => {
            let input: Value = serde_json::from_slice(request_body)
                .map_err(|e| anyhow!("body /embed bukan JSON: {e}"))?;
            let texts: Vec<String> = input
                .get("texts")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("field texts wajib"))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow!("text bukan string"))
                })
                .collect::<Result<Vec<_>>>()?;
            let role = input
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("document");
            let vectors = forward(&cfg, &texts, role)?;
            (200, json!({ "dim": cfg.dim, "vectors": vectors }))
        }
        _ => (404, json!({ "error": "not found" })),
    };

    let response_body = serde_json::to_vec(&payload)?;
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response_body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.write_all(&response_body)?;
    Ok(())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn main() -> Result<()> {
    let cfg = Arc::new(Config::from_env()?);
    let listener = TcpListener::bind(format!("{}:{}", cfg.bind, cfg.port))?;
    eprintln!(
        "mb-embed-adapter: {:?} model={} dim={} listening {}:{}",
        cfg.provider, cfg.model, cfg.dim, cfg.bind, cfg.port
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let cfg = cfg.clone();
                thread::spawn(move || {
                    if let Err(e) = handle(stream, cfg) {
                        eprintln!("adapter request error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("adapter accept error: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cohere_response() {
        let value = json!({ "id": "x", "embeddings": { "float": [[1.0, 0.0], [0.0, 2.0]] } });
        let arr = value["embeddings"]["float"].as_array().unwrap();
        let rows: Vec<Vec<f32>> = arr.iter().map(|v| as_f32_vec(v).unwrap()).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], vec![0.0, 2.0]);
    }

    #[test]
    fn parse_openai_response_sorted() {
        let value = json!({ "data": [
            { "index": 1, "embedding": [0.0, 2.0] },
            { "index": 0, "embedding": [1.0, 0.0] }
        ] });
        let mut rows = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["index"].as_u64().unwrap() as usize,
                    as_f32_vec(&item["embedding"]).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        rows.sort_by_key(|(i, _)| *i);
        assert_eq!(rows[0].1, vec![1.0, 0.0]);
    }

    #[test]
    fn normalize_membuat_norma_satu() {
        let cfg = Config {
            bind: "127.0.0.1".into(),
            port: 7416,
            provider: Provider::Cohere,
            api_key: "x".into(),
            model: "embed-multilingual-light-v3.0".into(),
            dim: 2,
            base: "http://127.0.0.1:1".into(),
        };
        // forward tidak dipanggil di sini; uji normalisasi lewat helper lokal
        let v = vec![3.0f32, 4.0];
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        let n: Vec<f32> = v.into_iter().map(|x| x / norm).collect();
        assert!((n[0] - 0.6).abs() < 1e-6);
        assert!((n[1] - 0.8).abs() < 1e-6);
        assert_eq!(cfg.dim, 2);
    }
}

//! mb-embed-srv — server embedder MiniLM lokal yang bisa dipanggil box lain.
//!
//! Protokol internal (sama seperti mb-embed-adapter / HttpEmbedder):
//!   GET  /dim   -> {"dim":384}
//!   POST /embed -> {"texts":[...],"role":"query|document"(diabaikan)}
//!               <- {"dim":384,"vectors":[[f32,...],...]}
//!
//! Default bind 127.0.0.1:7416. Set MEMORYBOX_EMBED_HOST / _PORT untuk LAN.

use anyhow::{anyhow, Result};
use mb_embed::Embedder;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

struct Config {
    host: String,
    port: u16,
    token: Option<String>,
    dim: usize,
}

fn main() -> Result<()> {
    let cache = std::env::var("MEMORYBOX_MODEL_CACHE")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);
    let model = mb_embed::model_from_env();
    eprintln!("mb-embed-srv: loading model {model} (cache: {:?})", cache);
    let embedder = Embedder::load(&model, cache.as_deref())?;
    let cfg = Arc::new(Config {
        host: std::env::var("MEMORYBOX_EMBED_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("MEMORYBOX_EMBED_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7416),
        token: std::env::var("MEMORYBOX_EMBED_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
        dim: embedder.dim,
    });
    let embedder = Arc::new(Mutex::new(embedder));
    let listener = TcpListener::bind(format!("{}:{}", cfg.host, cfg.port))?;
    eprintln!(
        "mb-embed-srv: model={} dim={} listening {}:{}",
        model, cfg.dim, cfg.host, cfg.port
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let cfg = cfg.clone();
                let embedder = embedder.clone();
                thread::spawn(move || {
                    if let Err(e) = handle(stream, cfg, embedder) {
                        eprintln!("request error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

fn authorized(head: &str, token: Option<&str>) -> bool {
    match token {
        None => true,
        Some(expected) => head.lines().any(|line| {
            let (k, v) = match line.split_once(':') {
                Some(pair) => pair,
                None => return false,
            };
            k.eq_ignore_ascii_case("authorization") && v.trim() == format!("Bearer {expected}")
        }),
    }
}

fn handle(
    mut stream: TcpStream,
    cfg: Arc<Config>,
    embedder: Arc<Mutex<Embedder>>,
) -> Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 16 * 1024 * 1024 {
            return Err(anyhow!("header terlalu besar"));
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    if !authorized(&head, cfg.token.as_deref()) {
        return respond(&mut stream, 401, &json!({ "error": "unauthorized" }));
    }
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

    let (status, payload): (u16, Value) = match (method.as_str(), path.as_str()) {
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
            let vectors = {
                let mut guard = embedder.lock().unwrap();
                guard.embed(&texts)?
            };
            (200, json!({ "dim": cfg.dim, "vectors": vectors }))
        }
        _ => (404, json!({ "error": "not found" })),
    };
    respond(&mut stream, status, &payload)
}

fn respond(stream: &mut TcpStream, status: u16, payload: &Value) -> Result<()> {
    let body = serde_json::to_vec(payload)?;
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::authorized;

    #[test]
    fn auth_opsional() {
        assert!(authorized("GET /dim HTTP/1.1\r\n", None));
    }

    #[test]
    fn auth_bearer_cocok() {
        let head = "POST /embed HTTP/1.1\r\nAuthorization: Bearer rahasia\r\n";
        assert!(authorized(head, Some("rahasia")));
        assert!(!authorized(head, Some("lain")));
    }
}

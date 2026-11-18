//! Adapter MCP (stdio) untuk memory-box.
//!
//! Protokol: MCP stdio = JSON-RPC 2.0 per baris (newline-delimited).
//! Proxy tipis ke server HTTP memory-box (default 127.0.0.1:7412).
//!
//! Konfigurasi (env):
//!  - MEMORYBOX_URL     : base URL server (default http://127.0.0.1:7412)
//!  - MEMORYBOX_PROJECT : path canonical project (wajib)
//!
//! Tools:
//!  - memory.retrieve : potongan kode relevan (hybrid RRF + KNN)
//!  - memory.search   : memory jangka panjang (episodic/semantic/procedural)
//!  - memory.save     : tulis memory
//!  - memory.forget   : hapus memory by id
//!  - memory.graph    : subgraph sekitar simbol (nama atau collection/id)
//!  - memory.symbols  : lookup simbol by nama
//!
//! Tanpa dependensi HTTP eksternal: klien HTTP/1.1 minimal via TcpStream
//! (localhost, tanpa TLS).

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::OnceLock;

const PROTOCOL: &str = "2024-11-05";

struct Cfg {
    base: String,
    project: Option<String>,
}

static CFG: OnceLock<Cfg> = OnceLock::new();

fn cfg() -> &'static Cfg {
    CFG.get_or_init(|| Cfg {
        base: std::env::var("MEMORYBOX_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7412".into()),
        project: std::env::var("MEMORYBOX_PROJECT").ok().filter(|s| !s.is_empty()),
    })
}

/// POST JSON ke server memory-box; kembalikan body sebagai Value.
fn http_post_json(path: &str, body: &Value) -> Result<Value, String> {
    use std::net::TcpStream;
    let base = &cfg().base;
    // parse http://host[:port][/prefix]
    let rest = base
        .strip_prefix("http://")
        .ok_or_else(|| format!("MEMORYBOX_URL harus http://... : {base}"))?;
    let (host, base_path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, String::new()),
    };
    let uri = format!("{base_path}{path}"); // path selalu berawalan '/'
    let payload = body.to_string();
    let auth = std::env::var("MEMORYBOX_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST {uri} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{auth}Connection: close\r\n\r\n{payload}",
        payload.len()
    );
    let mut stream = TcpStream::connect(host).map_err(|e| format!("connect {host}: {e}"))?;
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut resp = Vec::new();
    std::io::Read::read_to_end(&mut stream, &mut resp).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&resp);
    let body = parse_http_response(&text)?;
    serde_json::from_str(&body).map_err(|e| format!("respons bukan JSON: {e}: {}", clip(&body)))
}

/// Potong string untuk pesan error (jangan membanjiri stderr MCP).
fn clip(s: &str) -> String {
    s.chars().take(200).collect()
}

/// Cek status HTTP lalu ambil body. Error 4xx/5xx tidak boleh dianggap
/// sebagai hasil sukses hanya karena bodynya kebetulan JSON valid.
fn parse_http_response(text: &str) -> Result<String, String> {
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok());
    let raw = text.splitn(2, "\r\n\r\n").nth(1).unwrap_or("");
    let body = if text.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        dechunk(raw)
    } else {
        raw.to_string()
    };
    match status {
        Some(200..=299) => Ok(body),
        Some(code) => Err(format!("HTTP {code}: {}", clip(&body))),
        None => Err(format!("respons HTTP tanpa status: {}", clip(&body))),
    }
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let Some(i) = rest.find("\r\n") else { break };
        let size = usize::from_str_radix(rest[..i].trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = i + 2;
        let end = (start + size).min(rest.len());
        out.push_str(&rest[start..end]);
        rest = &rest[(end + 2).min(rest.len())..];
    }
    out
}

fn project_id() -> Result<String, String> {
    match &cfg().project {
        Some(p) => {
            let alias = std::env::var("MEMORYBOX_ALIAS").ok().filter(|a| !a.is_empty());
            let body = match alias {
                Some(a) => json!({ "alias": a, "path": p }),
                None => json!({ "path": p }),
            };
            let r = http_post_json("/v1/projects/open", &body)?;
            r.get("project_id")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| "respons projects/open tanpa project_id".into())
        }
        None => Err("MEMORYBOX_PROJECT belum diset (path canonical project)".into()),
    }
}

// ---------------- tool handlers ----------------

fn tool_retrieve(args: &Value) -> Result<Value, String> {
    let pid = project_id()?;
    let r = http_post_json(
        "/v1/retrieve",
        &json!({
            "project_id": pid,
            "query": args["query"].as_str().unwrap_or_default(),
            "k": args["k"].as_u64().unwrap_or(5),
        }),
    )?;
    let mut lines = Vec::new();
    for h in r["hits"].as_array().cloned().unwrap_or_default() {
        lines.push(format!(
            "# {}:{}\n{}",
            h["path"].as_str().unwrap_or("?"),
            h["start_line"].as_i64().unwrap_or(0),
            h["content"].as_str().unwrap_or("")
        ));
    }
    Ok(json!({ "content": [{ "type": "text", "text": if lines.is_empty() { "tidak ada hit".into() } else { lines.join("\n\n") } }] }))
}

fn tool_search(args: &Value) -> Result<Value, String> {
    let pid = project_id()?;
    let mut body = json!({
        "project_id": pid,
        "query": args["query"].as_str().unwrap_or_default(),
        "k": args["k"].as_u64().unwrap_or(5),
    });
    if let Some(k) = args["kind"].as_str() {
        body["kind"] = json!(k);
    }
    let r = http_post_json("/v1/memory/search", &body)?;
    let hits = r["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        return Ok(json!({ "content": [{ "type": "text", "text": "tidak ada hit" }] }));
    }
    let text = hits
        .iter()
        .map(|h| {
            format!(
                "[{}] id={} score={:.3}\n{}",
                h["kind"].as_str().unwrap_or("?"),
                h["id"].as_str().unwrap_or("?"),
                h["score"].as_f64().unwrap_or(0.0),
                h["text"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n---\n");
    Ok(json!({ "content": [{ "type": "text", "text": text }] }))
}

fn tool_save(args: &Value) -> Result<Value, String> {
    let pid = project_id()?;
    let mut body = json!({
        "project_id": pid,
        "text": args["text"].as_str().unwrap_or_default(),
    });
    // API mewajibkan kind — samakan dengan plugin (default semantic).
    body["kind"] = json!(args["kind"].as_str().unwrap_or("semantic"));
    if let Some(m) = args.get("metadata") {
        body["metadata"] = m.clone();
    }
    let r = http_post_json("/v1/memory/write", &body)?;
    Ok(json!({ "content": [{ "type": "text", "text": format!("tersimpan: {}", r) }] }))
}

fn tool_forget(args: &Value) -> Result<Value, String> {
    let pid = project_id()?;
    // API memerlukan kind untuk memilih koleksi mem_* — jangan menebak.
    let kind = args["kind"]
        .as_str()
        .ok_or("kind wajib: episodic | semantic | procedural")?;
    let r = http_post_json(
        "/v1/memory/forget",
        &json!({ "project_id": pid, "kind": kind, "id": args["id"].as_str().unwrap_or_default() }),
    )?;
    Ok(json!({ "content": [{ "type": "text", "text": format!("{}", r) }] }))
}

fn tool_symbols(args: &Value) -> Result<Value, String> {
    let pid = project_id()?;
    let r = http_post_json(
        "/v1/symbols/lookup",
        &json!({ "project_id": pid, "name": args["name"].as_str().unwrap_or_default() }),
    )?;
    let hits = r["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        return Ok(json!({ "content": [{ "type": "text", "text": "simbol tidak ditemukan" }] }));
    }
    let text = hits
        .iter()
        .map(|h| {
            format!(
                "{} [{}] {}:{}",
                h["id"].as_str().unwrap_or("?"),
                h["kind"].as_str().unwrap_or("?"),
                h["path"].as_str().unwrap_or("?"),
                h["start_line"].as_i64().unwrap_or(0)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(json!({ "content": [{ "type": "text", "text": text }] }))
}

fn tool_graph(args: &Value) -> Result<Value, String> {
    let pid = project_id()?;
    let seed = args["seed"].as_str().unwrap_or_default().to_string();
    // seed nama simbol → resolve dulu
    let resolved = if seed.contains('/') {
        seed
    } else {
        let r = http_post_json(
            "/v1/symbols/lookup",
            &json!({ "project_id": pid, "name": seed, "k": 1 }),
        )?;
        let id = r["hits"][0]["id"].as_str().ok_or("simbol tidak ditemukan")?;
        format!("symbols/{id}")
    };
    let r = http_post_json(
        "/v1/graph/context",
        &json!({ "project_id": pid, "seed": resolved, "hops": args["hops"].as_u64().unwrap_or(2) }),
    )?;
    // dereferensi edge-doc (_from/_to __ref__) → "caller -> callee (call)"
    let nodes = r["nodes"].as_array().cloned().unwrap_or_default();
    let mut doc_by_id: std::collections::HashMap<String, Value> = Default::default();
    for n in &nodes {
        let key = format!("{}/{}", n["collection"].as_str().unwrap_or(""), n["id"].as_str().unwrap_or(""));
        doc_by_id.insert(key, n["doc"].clone());
    }
    let deref = |v: &Value| -> String {
        if v.is_object() {
            v["__ref__"].as_str().unwrap_or_default().to_string()
        } else {
            v.as_str().unwrap_or_default().to_string()
        }
    };
    let name_of = |reftxt: &str| -> String {
        if let Some(doc) = doc_by_id.get(reftxt) {
            let col = reftxt.split('/').next().unwrap_or("");
            if col == "symbols" {
                return format!(
                    "{} [{}] {}:{}",
                    doc["name"].as_str().unwrap_or("?"),
                    doc["kind"].as_str().unwrap_or("?"),
                    doc["path"].as_str().unwrap_or("?"),
                    doc["start_line"].as_i64().unwrap_or(0)
                );
            }
            if col == "chunks" {
                return format!("{}:{}", doc["path"].as_str().unwrap_or("?"), doc["start_line"].as_i64().unwrap_or(0));
            }
        }
        reftxt.to_string()
    };
    let mut lines = Vec::new();
    for e in r["edges"].as_array().cloned().unwrap_or_default() {
        let from = e["from"].as_str().unwrap_or_default().to_string();
        let to = e["to"].as_str().unwrap_or_default().to_string();
        if let Some(doc) = doc_by_id.get(&from) {
            if doc.get("_from").is_some() {
                lines.push(format!(
                    "{} -> {} ({})",
                    name_of(&deref(&doc["_from"])),
                    name_of(&deref(&doc["_to"])),
                    doc["kind"].as_str().unwrap_or("")
                ));
                continue;
            }
        }
        lines.push(format!("{} -> {}", name_of(&from), name_of(&to)));
    }
    Ok(json!({ "content": [{ "type": "text", "text": format!("subgraph dari {resolved}:\n{}", lines.join("\n")) }] }))
}

// ---------------- skema tools ----------------

fn tools() -> Value {
    let obj = |name: &str, desc: &str, props: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": desc,
            "inputSchema": {
                "type": "object",
                "properties": props,
                "required": required,
            }
        })
    };
    json!([
        obj("memory.retrieve", "Cari potongan kode relevan di codebase (hybrid full-text + vektor).", json!({
            "query": { "type": "string" }, "k": { "type": "integer", "description": "default 5" }
        }), &["query"]),
        obj("memory.search", "Cari memory jangka panjang (episodic/semantic/procedural).", json!({
            "query": { "type": "string" }, "kind": { "type": "string", "enum": ["episodic","semantic","procedural"] }, "k": { "type": "integer" }
        }), &["query"]),
        obj("memory.save", "Simpan memory jangka panjang.", json!({
            "text": { "type": "string" }, "kind": { "type": "string", "enum": ["episodic","semantic","procedural"], "description": "default semantic" }, "metadata": { "type": "object" }
        }), &["text"]),
        obj("memory.forget", "Hapus memory by id dari koleksi tertentu.", json!({
            "id": { "type": "string" },
            "kind": { "type": "string", "enum": ["episodic","semantic","procedural"] }
        }), &["id", "kind"]),
        obj("memory.symbols", "Lookup simbol by nama (id, kind, path, line).", json!({ "name": { "type": "string" } }), &["name"]),
        obj("memory.graph", "Subgraph call sekitar simbol: siapa memanggil apa. Seed = nama simbol atau symbols/<id>.", json!({
            "seed": { "type": "string" }, "hops": { "type": "integer", "description": "default 2" }
        }), &["seed"]),
    ])
}

fn call_tool(name: &str, args: &Value) -> Value {
    let res: Result<Value, String> = match name {
        "memory.retrieve" => tool_retrieve(args),
        "memory.search" => tool_search(args),
        "memory.save" => tool_save(args),
        "memory.forget" => tool_forget(args),
        "memory.symbols" => tool_symbols(args),
        "memory.graph" => tool_graph(args),
        _ => Err(format!("tool tidak dikenal: {name}")),
    };
    match res {
        Ok(v) => v,
        Err(e) => json!({ "content": [{ "type": "text", "text": format!("error: {e}") }], "isError": true }),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = msg["id"].clone();
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        // notifikasi (tanpa id) tidak dibalas
        if id.is_null() {
            continue;
        }
        // balikan sukses atau error JSON-RPC
        let known = matches!(
            method.as_str(),
            "initialize" | "ping" | "tools/list" | "tools/call"
        );
        if !known {
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method tidak dikenal: {method}") },
            });
            let _ = writeln!(out, "{resp}");
            let _ = out.flush();
            continue;
        }
        let result = match method.as_str() {
            "initialize" => json!({
                "protocolVersion": PROTOCOL,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "memory-box", "version": "0.1.0" },
            }),
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools() }),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or_default().to_string();
                call_tool(&name, &msg["params"]["arguments"])
            }
            _ => unreachable!(),
        };
        let resp = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = writeln!(out, "{resp}");
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respons_sukses_diterima() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}";
        assert_eq!(parse_http_response(raw).unwrap(), "{\"ok\":true}");
    }

    #[test]
    fn respons_error_tidak_dianggap_sukses() {
        let raw = "HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 30\r\n\r\n{\"error\":\"missing field kind\"}";
        let err = parse_http_response(raw).unwrap_err();
        assert!(err.contains("HTTP 422"), "{err}");
        assert!(err.contains("missing field kind"), "{err}");
    }

    #[test]
    fn respons_chunked_didekode() {
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(parse_http_response(raw).unwrap(), "hello world");
    }

    #[test]
    fn respons_tanpa_status_ditolak() {
        assert!(parse_http_response("tidak ada status").is_err());
    }
}

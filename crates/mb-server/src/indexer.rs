//! Indexer fase 2 (tahap 2): chunker baris + tree-sitter (simbol & call-graph).
//!
//! Koleksi:
//!  - `chunks`  : potongan kode (vector + FTS)
//!  - `symbols` : definisi simbol; field `ref_chunks` → chunks (Reference)
//!  - `edges`   : caller → callee, `_from`/`_to` Reference + kind/w (edge index)
//!
//! Edge id stabil = hash(caller|callee|kind) → re-scan idempotent (overwrite).
//! TODO fase lanjut: pembersihan simbol/edge stale, resolusi import path.

use crate::embed::SharedEmbedder;
use crate::parser;
use crate::store::DIM;
use anyhow::Result;
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::{BatchMutation, Hako};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_CHUNK_LINES: usize = 40;

const SKIP_DIRS: [&str; 10] = [
    ".git", ".hg", ".svn", "target", "node_modules", "vendor", "dist",
    "build", "__pycache__", ".venv",
];

const SKIP_FILES: [&str; 3] = [".lock", ".sum", ".snap"];

pub fn is_source(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(
            ext,
            "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java" | "kt"
            | "c" | "h" | "cpp" | "hpp" | "cs" | "rb" | "php" | "swift"
            | "md" | "toml" | "yaml" | "yml" | "json" | "sql" | "sh"
        ),
        None => false,
    }
}

fn id_for(path: &str, start: usize) -> String {
    let h = Sha256::digest(format!("{path}:{start}").as_bytes());
    h.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn hex(d: &[u8], n: usize) -> String {
    d.iter().take(n).map(|b| format!("{b:02x}")).collect()
}

fn ext_of(rel: &str) -> Option<&str> {
    Path::new(rel).extension().and_then(|e| e.to_str())
}

fn chunk_lines(content: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_start = 0usize;
    for (i, line) in content.lines().enumerate() {
        if cur.is_empty() {
            cur_start = i + 1;
        }
        cur.push_str(line);
        cur.push('\n');
        if cur.len() > MAX_CHUNK_LINES * 80 || cur.lines().count() >= MAX_CHUNK_LINES {
            out.push((cur_start, cur.trim_end().to_string()));
            cur = String::new();
        }
    }
    if !cur.trim_end().is_empty() {
        out.push((cur_start, cur.trim_end().to_string()));
    }
    out
}

fn push_file_rec(result: &mut Vec<PathBuf>, path: &Path) {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    if path.is_dir() {
        if SKIP_DIRS.contains(&name.as_str()) {
            return;
        }
        if let Ok(rd) = std::fs::read_dir(path) {
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                push_file_rec(result, &e.path());
            }
        }
        return;
    }
    if !is_source(path) || SKIP_FILES.iter().any(|s| name.ends_with(s)) {
        return;
    }
    if let Ok(md) = path.metadata() {
        if md.len() <= MAX_FILE_BYTES {
            result.push(path.to_path_buf());
        }
    }
}

/// Hasil scan untuk API.
#[derive(serde::Serialize)]
pub struct ScanReport {
    pub files: usize,
    pub chunks: usize,
    pub symbols: usize,
    pub edges: usize,
}

pub fn scan(
    db: &Hako,
    embedder: &SharedEmbedder,
    root: &Path,
) -> Result<ScanReport> {
    let mut files = Vec::new();
    if root.is_dir() {
        push_file_rec(&mut files, root);
    } else if is_source(root) {
        files.push(root.to_path_buf());
    }

    // ---- pass 1: chunking ----
    // (rel, content, [(start_line, chunk_id)])
    let mut file_data: Vec<(String, String, Vec<(usize, String)>)> = Vec::new();
    let mut chunk_mutations: Vec<(String, HakoDoc, String)> = Vec::new(); // (id, doc, path|text)

    for file in &files {
        let Ok(content) = std::fs::read_to_string(file) else { continue };
        let rel = file
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| file.to_string_lossy().to_string());
        // scan file tunggal: strip_prefix(root) menghasilkan "" — pakai nama file
        let rel = if rel.is_empty() {
            file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(rel)
        } else {
            rel
        };
        let mut chunk_list = Vec::new();
        for (start, text) in chunk_lines(&content) {
            if text.trim().is_empty() {
                continue;
            }
            let id = id_for(&rel, start);
            let mut d = HakoDoc::default();
            d.insert("path", Value::String(rel.clone()));
            d.insert("content", Value::String(text.clone()));
            d.insert("start_line", Value::Int(start as i64));
            chunk_mutations.push((id.clone(), d, format!("{rel} | {text}")));
            chunk_list.push((start, id));
        }
        file_data.push((rel, content, chunk_list));
    }

    // ---- embed per batch 16 ----
    let mut chunk_ops: Vec<BatchMutation> = Vec::with_capacity(chunk_mutations.len());
    for batch in chunk_mutations.chunks(16) {
        let inputs: Vec<String> = batch.iter().map(|(_, _, pt)| pt.clone()).collect();
        let embs = embedder.embed(&inputs)?;
        for (emb, (id, doc, _)) in embs.into_iter().zip(batch.iter()) {
            let mut doc = doc.clone();
            doc.insert("emb", Value::Binary(hakodb::index::vector::encode_f32s(&emb)));
            chunk_ops.push(BatchMutation::Put {
                collection: "chunks".into(),
                doc_id: id.clone(),
                doc,
            });
        }
    }
    assert_eq!(embedder.dim(), DIM);

    // ---- pass 2: tree-sitter symbols ----
    let mut parsed: Vec<Option<parser::ParsedFile>> = Vec::with_capacity(file_data.len());
    // (file_idx, sym_idx) -> symbol doc id; sekaligus untuk resolusi callee
    let mut sym_ids: Vec<Vec<String>> = Vec::with_capacity(file_data.len());
    // name -> [(file_idx, path, sym_id)]
    let mut by_name: HashMap<String, Vec<(usize, String, String)>> = HashMap::new();
    let mut symbol_ops: Vec<BatchMutation> = Vec::new();

    for (fi, (rel, content, chunk_list)) in file_data.iter().enumerate() {
        let Some(ext) = ext_of(rel).and_then(parser::for_ext) else {
            parsed.push(None);
            sym_ids.push(Vec::new());
            continue;
        };
        let p = ext.parse(content);
        let mut ids = Vec::with_capacity(p.symbols.len());
        for sym in &p.symbols {
            let id = hex(&Sha256::digest(format!("{rel}|{}|{}", sym.name, sym.kind).as_bytes()), 8);
            let mut d = HakoDoc::default();
            d.insert("name", Value::String(sym.name.clone()));
            d.insert("kind", Value::String(sym.kind.into()));
            d.insert("path", Value::String(rel.clone()));
            d.insert("start_line", Value::Int(sym.start_row as i64));
            if let Some((_, chunk_id)) = chunk_list.iter().rev().find(|(s, _)| *s <= sym.start_row) {
                d.insert(
                    "ref_chunks",
                    Value::Reference { collection: "chunks".into(), doc_id: chunk_id.clone() },
                );
            }
            symbol_ops.push(BatchMutation::Put {
                collection: "symbols".into(),
                doc_id: id.clone(),
                doc: d,
            });
            by_name
                .entry(sym.name.clone())
                .or_default()
                .push((fi, rel.clone(), id.clone()));
            ids.push(id);
        }
        parsed.push(Some(p));
        sym_ids.push(ids);
    }

    // ---- pass 3: call-graph edges ----
    let mut edge_ops: Vec<BatchMutation> = Vec::new();
    let mut edge_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (fi, p) in parsed.iter().enumerate() {
        let Some(p) = p else { continue };
        for call in &p.calls {
            let Some(caller_id) = sym_ids[fi].get(call.enclosing) else { continue };
            let Some(cands) = by_name.get(&call.callee) else { continue };
            // resolusi: file yang sama dulu, lalu file pertama yang lain
            let callee = cands
                .iter()
                .find(|(cfi, _, _)| *cfi == fi)
                .or_else(|| cands.first());
            let Some((_, _, callee_id)) = callee else { continue };
            if callee_id == caller_id {
                continue; // rekursi langsung: skip agar graph tidak melingkar sepele
            }
            if !edge_seen.insert(format!("{caller_id}|{callee_id}")) {
                continue;
            }
            let edge_id = hex(&Sha256::digest(format!("{caller_id}|{callee_id}|call").as_bytes()), 8);
            let mut d = HakoDoc::default();
            d.insert("_from", Value::Reference {
                collection: "symbols".into(),
                doc_id: caller_id.clone(),
            });
            d.insert("_to", Value::Reference {
                collection: "symbols".into(),
                doc_id: callee_id.clone(),
            });
            d.insert("kind", Value::String("call".into()));
            d.insert("w", Value::Float(2.0));
            edge_ops.push(BatchMutation::Put {
                collection: "edges".into(),
                doc_id: edge_id,
                doc: d,
            });
        }
    }

    // ---- tulis semua dalam satu batch ----
    let (n_symbols, n_edges) = (symbol_ops.len(), edge_ops.len());
    let mut ops = chunk_ops;
    ops.extend(symbol_ops);
    ops.extend(edge_ops);
    if !ops.is_empty() {
        db.write_batch(ops)?;
    }

    Ok(ScanReport {
        files: files.len(),
        chunks: chunk_mutations.len(),
        symbols: n_symbols,
        edges: n_edges,
    })
}

/// Lookup simbol by nama persis (id + lokasi) — untuk seed graph.
pub fn lookup_symbol(db: &Hako, name: &str, k: usize) -> Result<Vec<(String, HakoDoc)>> {
    let q = hakodb::query::query::Query::new("symbols")
        .where_filter("name", hakodb::query::filter::Operator::Eq, Value::String(name.into()))
        .limit(k);
    db.query(q).map_err(|e| anyhow::anyhow!("{e}"))
}

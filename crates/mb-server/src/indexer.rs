//! Indexer fase 2 (tahap 3): chunker baris + tree-sitter (simbol & call-graph)
//! + cleanup stale per file.
//!
//! Koleksi:
//!  - `chunks`  : potongan kode (vector + FTS)
//!  - `symbols` : definisi simbol; field `ref_chunks` → chunks (Reference)
//!  - `edges`   : caller → callee, `_from`/`_to` Reference + kind/w/path
//!
//! Konsistensi rel: rel selalu relatif terhadap **root project** (rel_base),
//! bukan terhadap target scan — sehingga id simbol/chunk stabil di seluruh
//! mode scan (root penuh, subdir, file tunggal, watcher).
//!
//! Cleanup: sebelum menulis batch, query docs lama dengan path == rel yang
//! di-scan; id yang tidak ada di set baru dihapus (chunks/symbols/edges).
//! Target yang sudah tidak ada (file dihapus) → cleanup murni.
//!
//! TODO fase lanjut: re-resolusi edge lintas-file yang menunjuk ke simbol
//! milik file lain yang berubah (dangling inbound edge), resolusi import path.

use crate::embed::SharedEmbedder;
use crate::parser;
use crate::store::DIM;
use anyhow::Result;
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::{BatchMutation, Hako};
use hakodb::query::filter::Operator;
use hakodb::query::query::Query;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_CHUNK_LINES: usize = 40;

const SKIP_DIRS: [&str; 10] = [
    ".git", ".hg", ".svn", "target", "node_modules", "vendor", "dist",
    "build", "__pycache__", ".venv",
];

const SKIP_FILES: [&str; 3] = [".lock", ".sum", ".snap"];

/// Koleksi yang membersihkan berdasar field `path`.
const PATHED_COLLECTIONS: [&str; 3] = ["chunks", "symbols", "edges"];

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

/// rel file terhadap root project (rel_base). File == base → nama file
/// (scan file tunggal); di luar base → path absolut.
fn rel_of(file: &Path, rel_base: &Path) -> String {
    let rel = file
        .strip_prefix(rel_base)
        .map(|r| r.to_string_lossy().to_string())
        .unwrap_or_else(|_| file.to_string_lossy().to_string());
    if rel.is_empty() {
        file.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(rel)
    } else {
        rel
    }
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
    /// jumlah doc lama yang dihapus (stale)
    pub removed: usize,
}

/// id yang harus dipertahankan per rel per koleksi — dasar cleanup.
type KeepMap = HashMap<String, HashMap<&'static str, HashSet<String>>>;

fn keep_insert(keep: &mut KeepMap, rel: &str, col: &'static str, id: &str) {
    keep.entry(rel.to_string()).or_default().entry(col).or_default().insert(id.to_string());
}

/// Hapus doc lama dengan path == rel yang tidak ada di keep (atau keep kosong
/// untuk file terhapus). Query membaca state LAMA — dipanggil sebelum
/// write_batch.
fn stale_deletes(db: &Hako, keep: &KeepMap, removed_rels: &[String]) -> Result<Vec<BatchMutation>> {
    let mut dels: Vec<BatchMutation> = Vec::new();
    let clean_rel = |db: &Hako, rel: &str, ids: Option<&HashSet<String>>, dels: &mut Vec<BatchMutation>| -> Result<()> {
        for col in PATHED_COLLECTIONS {
            let q = Query::new(col)
                .where_filter("path", Operator::Eq, Value::String(rel.to_string()))
                .limit(20_000);
            for (id, _) in db.query(q).map_err(|e| anyhow::anyhow!("{e}"))? {
                let stale = match ids {
                    Some(set) => !set.contains(&id),
                    None => true,
                };
                if stale {
                    dels.push(BatchMutation::Delete {
                        collection: col.to_string(),
                        doc_id: id,
                    });
                }
            }
        }
        Ok(())
    };
    for (rel, cols) in keep {
        clean_rel(db, rel, cols.get("chunks"), &mut dels)?;
        clean_rel(db, rel, cols.get("symbols"), &mut dels)?;
        clean_rel(db, rel, cols.get("edges"), &mut dels)?;
    }
    for rel in removed_rels {
        clean_rel(db, rel, None, &mut dels)?;
    }
    Ok(dels)
}

/// Scan target (dir/file) dengan rel terhadap `rel_base` (root project).
/// Target file yang tidak ada → cleanup murni untuk rel-nya.
pub fn scan(
    db: &Hako,
    embedder: &SharedEmbedder,
    rel_base: &Path,
    target: &Path,
) -> Result<ScanReport> {
    let mut files = Vec::new();
    let mut removed_rels: Vec<String> = Vec::new();
    if target.is_dir() {
        push_file_rec(&mut files, target);
    } else if target.is_file() {
        if is_source(target) {
            files.push(target.to_path_buf());
        }
    } else {
        // file dihapus dari disk → bersihkan dokumennya
        removed_rels.push(rel_of(target, rel_base));
    }

    // ---- pass 1: chunking ----
    let mut file_data: Vec<(String, String, Vec<(usize, String)>)> = Vec::new();
    let mut chunk_mutations: Vec<(String, HakoDoc, String)> = Vec::new(); // (id, doc, path|text)
    let mut keep: KeepMap = HashMap::new();

    for file in &files {
        let Ok(content) = std::fs::read_to_string(file) else { continue };
        let rel = rel_of(file, rel_base);
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
            chunk_list.push((start, id.clone()));
            keep_insert(&mut keep, &rel, "chunks", &id);
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
            keep_insert(&mut keep, rel, "symbols", &id);
            ids.push(id);
        }
        parsed.push(Some(p));
        sym_ids.push(ids);
    }

    // ---- pass 3: call-graph edges (path = rel caller, untuk cleanup) ----
    let mut edge_ops: Vec<BatchMutation> = Vec::new();
    let mut edge_seen: HashSet<String> = HashSet::new();
    for (fi, p) in parsed.iter().enumerate() {
        let Some(p) = p else { continue };
        let caller_rel = &file_data[fi].0;
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
            d.insert("path", Value::String(caller_rel.clone()));
            edge_ops.push(BatchMutation::Put {
                collection: "edges".into(),
                doc_id: edge_id.clone(),
                doc: d,
            });
            keep_insert(&mut keep, caller_rel, "edges", &edge_id);
        }
    }

    // ---- cleanup stale (query state lama) + tulis batch gabungan ----
    let del_ops = stale_deletes(db, &keep, &removed_rels)?;
    let (n_symbols, n_edges, n_removed) = (symbol_ops.len(), edge_ops.len(), del_ops.len());
    let mut ops = chunk_ops;
    ops.extend(symbol_ops);
    ops.extend(edge_ops);
    ops.extend(del_ops);
    if !ops.is_empty() {
        db.write_batch(ops)?;
    }

    Ok(ScanReport {
        files: files.len(),
        chunks: chunk_mutations.len(),
        symbols: n_symbols,
        edges: n_edges,
        removed: n_removed,
    })
}

/// Hapus semua doc (chunks/symbols/edges) milik satu rel — untuk file terhapus.
pub fn remove_rel(db: &Hako, rel: &str) -> Result<usize> {
    let dels = stale_deletes(db, &HashMap::new(), &[rel.to_string()])?;
    let n = dels.len();
    if !dels.is_empty() {
        db.write_batch(dels)?;
    }
    Ok(n)
}

/// rel file terhadap root project (publik untuk API).
pub fn rel_of_pub(file: &Path, rel_base: &Path) -> String {
    rel_of(file, rel_base)
}

/// Lookup simbol by nama persis (id + lokasi) — untuk seed graph.
pub fn lookup_symbol(db: &Hako, name: &str, k: usize) -> Result<Vec<(String, HakoDoc)>> {
    let q = Query::new("symbols")
        .where_filter("name", Operator::Eq, Value::String(name.into()))
        .limit(k);
    db.query(q).map_err(|e| anyhow::anyhow!("{e}"))
}

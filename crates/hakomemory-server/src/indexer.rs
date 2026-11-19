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
//! TODO: resolusi import path (saat ini callee dicocokkan nama global).

use crate::embed::SharedEmbedder;
use crate::parser;
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
const PATHED_COLLECTIONS: [&str; 2] = ["chunks", "symbols"];

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
type FileChunks = Vec<(String, String, Vec<(usize, String)>)>;

fn rel_of(file: &Path, rel_base: &Path) -> String {
    let rel_path = relative_of(file, rel_base).unwrap_or_else(|| file.to_path_buf());
    let rel = rel_path.to_string_lossy().replace('\\', "/");
    if rel.is_empty() {
        file.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(rel)
    } else {
        rel
    }
}

/// Relative path against the project root. Falls back to canonical forms so
/// Windows 8.3 short paths and differently-spelled roots still compare equal.
fn relative_of(file: &Path, rel_base: &Path) -> Option<std::path::PathBuf> {
    if let Ok(rel) = file.strip_prefix(rel_base) {
        return Some(rel.to_path_buf());
    }
    let canonical_file = std::fs::canonicalize(file).ok()?;
    let canonical_base = std::fs::canonicalize(rel_base).ok()?;
    canonical_file.strip_prefix(canonical_base).ok().map(|p| p.to_path_buf())
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
    /// jumlah edge yang direkonsiliasi (rewrite/hapus) pasca-tulis
    pub settled: usize,
}

/// Rel yang masih tersimpan di DB di bawah `prefix` scan tetapi tidak
/// termasuk hasil scan saat ini (file sudah hilang, misal akibat `git mv`
/// saat server mati). Dipakai scan direktori — bukan scan file tunggal.
fn missing_rels(db: &Hako, prefix: &str, present: &KeepMap) -> Result<Vec<String>> {
    let mut out = HashSet::new();
    for col in ["chunks", "symbols", "edges"] {
        let q = Query::new(col).limit(100_000);
        for (_, doc) in db.query(q).map_err(|e| anyhow::anyhow!("{e}"))? {
            if let Some(Value::String(path)) = doc.get("path") {
                if !path.is_empty()
                    && path.starts_with(prefix)
                    && !present.contains_key(path)
                {
                    out.insert(path.clone());
                }
            }
        }
    }
    Ok(out.into_iter().collect())
}

/// id yang harus dipertahankan per rel per koleksi — dasar cleanup.
type KeepMap = HashMap<String, HashMap<&'static str, HashSet<String>>>;

/// Referensi → doc_id (Value::Reference).
fn ref_id(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::Reference { doc_id, .. }) => Some(doc_id.clone()),
        _ => None,
    }
}

/// Resolusi nama callee → (path callee, sym id).
/// Prioritas: file yang sama dengan caller (`fi`), lalu `prefer_path`,
/// lalu kandidat pertama. Batch tidak punya nama itu → fallback DB
/// (simbol milik file lain yang tidak ikut di-scan).
fn resolve_callee(
    db: &Hako,
    by_name: &HashMap<String, Vec<(usize, String, String)>>,
    fi: usize,
    prefer_path: Option<&str>,
    name: &str,
) -> Option<(String, String)> {
    if let Some(cands) = by_name.get(name) {
        if let Some((_, rel, id)) = cands.iter().find(|(cfi, _, _)| *cfi == fi) {
            return Some((rel.clone(), id.clone()));
        }
        if let Some(prefer) = prefer_path {
            if let Some((_, rel, id)) = cands.iter().find(|(_, r, _)| r == prefer) {
                return Some((rel.clone(), id.clone()));
            }
        }
        if let Some((_, rel, id)) = cands.first() {
            return Some((rel.clone(), id.clone()));
        }
        return None;
    }
    // fallback DB
    let q = Query::new("symbols")
        .where_filter("name", Operator::Eq, Value::String(name.to_string()))
        .limit(10);
    let rows = db.query(q).map_err(|_| anyhow::anyhow!("query symbols")).ok()?;
    let pick = |filter_path: Option<&str>| -> Option<(String, String)> {
        rows.iter().find(|(_, d)| {
                filter_path.is_none()
                    || d.get("path")
                        .and_then(|v| match v { Value::String(s) => Some(s.as_str()), _ => None })
                        == filter_path
            })
            .and_then(|(id, d)| {
                let path = d.get("path")
                    .and_then(|v| match v { Value::String(s) => Some(s.clone()), _ => None })?;
                Some((path, id.clone()))
            })
    };
    pick(prefer_path).or_else(|| pick(None))
}

fn keep_insert(keep: &mut KeepMap, rel: &str, col: &'static str, id: &str) {
    keep.entry(rel.to_string()).or_default().entry(col).or_default().insert(id.to_string());
}

/// Hapus doc lama dengan path == rel yang tidak ada di keep (atau keep kosong
/// untuk file terhapus). Query membaca state LAMA — dipanggil sebelum
/// write_batch.
fn stale_deletes(db: &Hako, keep: &KeepMap, removed_rels: &[String]) -> Result<Vec<BatchMutation>> {
    let mut dels: Vec<BatchMutation> = Vec::new();
    let clean_rel =
        |db: &Hako, rel: &str, cols: Option<&HashMap<&'static str, HashSet<String>>>,
         dels: &mut Vec<BatchMutation>|
         -> Result<()> {
            // chunks & symbols: id yang hilang dari set baru dihapus
            for col in PATHED_COLLECTIONS {
                let ids = cols.and_then(|c| c.get(col));
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
            // edges: hapus format lama (tanpa callee) dan yang tidak
            // diregenerasi batch ini; sisanya direkonsiliasi settle_edges
            let keep_edges = cols.and_then(|c| c.get("edges"));
            let q = Query::new("edges")
                .where_filter("path", Operator::Eq, Value::String(rel.to_string()))
                .limit(20_000);
            for (id, doc) in db.query(q).map_err(|e| anyhow::anyhow!("{e}"))? {
                let regen = keep_edges.is_some_and(|s| s.contains(&id));
                let old_format = doc.get("callee").is_none();
                if !regen || old_format {
                    dels.push(BatchMutation::Delete {
                        collection: "edges".into(),
                        doc_id: id,
                    });
                }
            }
            Ok(())
        };
    for (rel, cols) in keep {
        clean_rel(db, rel, Some(cols), &mut dels)?;
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
    let mut scan_prefix: Option<String> = None;
    if target.is_dir() {
        // prefix rel terhadap root project — untuk mendeteksi file lama
        // yang hilang dari disk (misal git mv saat server tidak jalan)
        scan_prefix = match relative_of(target, rel_base) {
            Some(rel) => {
                let s = rel.to_string_lossy().replace('\\', "/").trim_matches('/').to_string();
                Some(if s.is_empty() { String::new() } else { format!("{s}/") })
            }
            None => None,
        };
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
    let mut file_data: FileChunks = Vec::new();
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
    // Ukuran batch embedding bisa disesuaikan untuk provider remote
    // (mis. 96 untuk Cohere/Gemini) agar jumlah request API lebih sedikit.
    let embed_batch = std::env::var("MEMORYBOX_EMBED_BATCH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(16)
        .clamp(1, 250);
    for batch in chunk_mutations.chunks(embed_batch) {
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

    // ---- pass 3: call-graph edges ----
    // Edge id stabil = hash(caller|callee_NAME|call) — target boleh pindah
    // file tanpa mengubah id edge. Doc membawa callee + to_path untuk
    // re-resolusi inbound lintas-file saat file callee berubah.
    let mut edge_ops: Vec<BatchMutation> = Vec::new();
    let mut edge_seen: HashSet<String> = HashSet::new();
    for (fi, p) in parsed.iter().enumerate() {
        let Some(p) = p else { continue };
        let caller_rel = &file_data[fi].0;
        for call in &p.calls {
            let Some(caller_id) = sym_ids[fi].get(call.enclosing) else { continue };
            // resolusi: file yang sama dulu, lalu file pertama yang lain;
            // nama tidak ada di batch → fallback DB (simbol file lain)
            let callee = resolve_callee(
                db, &by_name, fi, Some(caller_rel), &call.callee,
            );
            let Some((callee_path, callee_id)) = callee else { continue };
            if callee_id.as_str() == caller_id {
                continue; // rekursi langsung: skip agar graph tidak melingkar sepele
            }
            if !edge_seen.insert(format!("{caller_id}|{}", call.callee)) {
                continue;
            }
            let edge_id = hex(
                &Sha256::digest(format!("{caller_id}|{}|call", call.callee).as_bytes()),
                8,
            );
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
            d.insert("callee", Value::String(call.callee.clone()));
            d.insert("to_path", Value::String(callee_path.clone()));
            edge_ops.push(BatchMutation::Put {
                collection: "edges".into(),
                doc_id: edge_id.clone(),
                doc: d,
            });
            keep_insert(&mut keep, caller_rel, "edges", &edge_id);
        }
    }

    // ---- cleanup stale (chunks/symbols + edge format lama) + tulis batch ----
    // Edge valid tidak dihapus di sini — settle_edges pasca-tulis yang
    // merekonsiliasi (bebas race urutan file dalam batch).
    let del_ops = stale_deletes(db, &keep, &removed_rels)?;
    let (n_symbols, n_edges, n_removed) = (symbol_ops.len(), edge_ops.len(), del_ops.len());
    // file yang hilang dari disk tapi tidak masuk removed_rels (misal pemindahan
    // dibuat saat server mati) — kumpulkan SEBELUM tulis.batch ini
    let gone: Vec<String> = match scan_prefix.as_deref() {
        Some(prefix) => missing_rels(db, prefix, &keep)?,
        None => Vec::new(),
    };
    let mut ops = chunk_ops;
    ops.extend(symbol_ops);
    ops.extend(edge_ops);
    ops.extend(del_ops);
    if !ops.is_empty() {
        db.write_batch(ops)?;
    }
    let mut n_removed = n_removed;
    for rel in &gone {
        n_removed += remove_rel(db, rel)?;
    }

    // ---- pass 4 (pasca-tulis): rekonsiliasi edge milik rel batch ----
    // Dijalankan SETELAH write_batch agar resolusi melihat state DB final
    // batch ini — bebas race urutan file (callee pindah file dulu/belum).
    let settled = {
        let mut rels: Vec<String> = keep.keys().cloned().collect();
        rels.extend(removed_rels.iter().cloned());
        settle_edges(db, &rels)?
    };

    Ok(ScanReport {
        files: files.len(),
        chunks: chunk_mutations.len(),
        symbols: n_symbols,
        edges: n_edges,
        removed: n_removed,
        settled,
    })
}

/// Rekonsiliasi edge milik `rels` terhadap state DB FINAL (dipanggil
/// setelah write_batch):
///  - caller simbol hilang           → edge dihapus
///  - callee nama tidak ada lagi     → edge dihapus
///  - callee pindah file / id berubah → edge di-rewrite (_to, to_path)
///  - target tetap                   → dibiarkan
///
/// Edge format lama (tanpa callee) diabaikan — migrasi lewat rescan file
/// caller-nya. Bebas race urutan file karena resolusi melihat hasil tulisan
/// seluruh batch.
fn settle_edges(db: &Hako, rels: &[String]) -> Result<usize> {
    let mut batch: Vec<BatchMutation> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for rel in rels {
        for field in ["path", "to_path"] {
            let q = Query::new("edges")
                .where_filter(field, Operator::Eq, Value::String(rel.to_string()))
                .limit(5_000);
            for (id, doc) in db.query(q).map_err(|e| anyhow::anyhow!("{e}"))? {
                if !seen.insert(id.clone()) {
                    continue;
                }
                let Some(from_id) = ref_id(doc.get("_from")) else { continue };
                let Some(caller_path) = doc
                    .get("path")
                    .and_then(|v| match v { Value::String(s) => Some(s.clone()), _ => None })
                else { continue };
                let Some(callee_name) = doc
                    .get("callee")
                    .and_then(|v| match v { Value::String(s) => Some(s.clone()), _ => None })
                else { continue };
                // caller harus masih ada
                if db.get("symbols", &from_id)?.is_none() {
                    batch.push(BatchMutation::Delete {
                        collection: "edges".into(),
                        doc_id: id,
                    });
                    continue;
                }
                match resolve_callee(db, &HashMap::new(), usize::MAX, Some(&caller_path), &callee_name)
                {
                    None => batch.push(BatchMutation::Delete {
                        collection: "edges".into(),
                        doc_id: id,
                    }),
                    Some((tpath, tid)) => {
                        let changed = ref_id(doc.get("_to")).as_deref() != Some(tid.as_str());
                        if changed {
                            let mut d = HakoDoc::default();
                            d.insert("_from", Value::Reference {
                                collection: "symbols".into(),
                                doc_id: from_id,
                            });
                            d.insert("_to", Value::Reference {
                                collection: "symbols".into(),
                                doc_id: tid,
                            });
                            d.insert("kind", Value::String("call".into()));
                            d.insert("w", Value::Float(2.0));
                            d.insert("path", Value::String(caller_path.clone()));
                            d.insert("callee", Value::String(callee_name.clone()));
                            d.insert("to_path", Value::String(tpath));
                            batch.push(BatchMutation::Put {
                                collection: "edges".into(),
                                doc_id: id,
                                doc: d,
                            });
                        }
                    }
                }
            }
        }
    }
    let n = batch.len();
    if !batch.is_empty() {
        db.write_batch(batch)?;
    }
    Ok(n)
}

/// Hapus semua doc (chunks/symbols/edges) milik satu rel — untuk file terhapus.
pub fn remove_rel(db: &Hako, rel: &str) -> Result<usize> {
    let dels = stale_deletes(db, &HashMap::new(), &[rel.to_string()])?;
    let n_del = dels.len();
    if !dels.is_empty() {
        db.write_batch(dels)?;
    }
    // edge: rekonsiliasi pasca-hapus (caller hilang → hapus; callee pindah → ikut)
    let settled = settle_edges(db, &[rel.to_string()])?;
    Ok(n_del + settled)
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

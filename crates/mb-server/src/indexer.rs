//! Indexer fase 2 (tahap awal): walker + chunker berbasis baris.
//! Tree-sitter (simbol & call-graph) menyusul di tahap berikutnya.

use crate::store::DIM;
use anyhow::Result;
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::{BatchMutation, Hako};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_CHUNK_LINES: usize = 40;

const SKIP_DIRS: [&str; 10] = [
    ".git", ".hg", ".svn", "target", "node_modules", "vendor", "dist",
    "build", "__pycache__", ".venv",
];

const SKIP_FILES: [&str; 3] = [".lock", ".sum", ".snap"];

fn is_source(path: &Path) -> bool {
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

fn chunk_lines(path: &str, content: &str) -> Vec<(usize, String)> {
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
    let _ = path;
    out
}

fn push_file_rec(result: &mut Vec<PathBuf>, root: &Path, path: &Path) {
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
                push_file_rec(result, root, &e.path());
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
    pub skipped_non_source: bool,
}

pub fn scan(
    db: &Hako,
    embedder: &crate::embed::SharedEmbedder,
    root: &Path,
) -> Result<ScanReport> {
    let mut files = Vec::new();
    if root.is_dir() {
        push_file_rec(&mut files, root, root);
    } else if is_source(root) {
        files.push(root.to_path_buf());
    }

    let mut mutations = Vec::new();
    let mut chunk_count = 0usize;

    for file in &files {
        let Ok(content) = std::fs::read_to_string(file) else { continue };
        let rel = file
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| file.to_string_lossy().to_string());
        for (start, text) in chunk_lines(&rel, &content) {
            if text.trim().is_empty() {
                continue;
            }
            let mut d = HakoDoc::default();
            d.insert("path", Value::String(rel.clone()));
            d.insert("content", Value::String(text.clone()));
            d.insert("start_line", Value::Int(start as i64));
            // embedding diisi setelah batch embed
            mutations.push((id_for(&rel, start), d, text));
            chunk_count += 1;
        }
    }

    // embed per batch 16
    let mut emb_results = Vec::with_capacity(mutations.len());
    for batch in mutations.chunks(16) {
        let inputs: Vec<String> = batch
            .iter()
            .map(|(_, d, text)| format!("{} | {}", path_field(d), text))
            .collect();
        let embs = embedder.embed(&inputs)?;
        emb_results.extend(embs);
    }
    let mut batch_ops = Vec::with_capacity(mutations.len());
    for (emb, (id, mut doc, _)) in emb_results.into_iter().zip(mutations) {
        doc.insert("emb", Value::Binary(hakodb::index::vector::encode_f32s(&emb)));
        batch_ops.push(BatchMutation::Put {
            collection: "chunks".into(),
            doc_id: id,
            doc,
        });
    }
    if !batch_ops.is_empty() {
        db.write_batch(batch_ops)?;
    }
    assert_eq!(embedder.dim(), DIM);

    Ok(ScanReport {
        files: files.len(),
        chunks: chunk_count,
        skipped_non_source: false,
    })
}

fn path_field(d: &HakoDoc) -> String {
    match d.get("path") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

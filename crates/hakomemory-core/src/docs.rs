//! Generic document ingestion and CRUD for local-first RAG.
//!
//! Documents and their chunks live in the same HakoDB project as the agent
//! memory collections. Chunks keep `source_uri`, `heading_path`, `start_line`
//! and `end_line` so retrieval results can carry citations without a second
//! lookup. A `files` manifest makes reindexing idempotent.

use crate::embed::SharedEmbedder;
use anyhow::Result;
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::Hako;
use hakodb::query::filter::Operator;
use hakodb::query::query::Query;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DOCUMENTS: &str = "documents";
pub const CHUNKS: &str = "chunks";
pub const FILES: &str = "files";
pub const SCHEMA_VERSION: i64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ChunkStrategy {
    /// Fixed line windows with overlap.
    Fixed,
    /// Markdown/heading aware; falls back to fixed windows inside long sections.
    #[default]
    Header,
}


#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentInput {
    pub source_uri: String,
    pub content: String,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub strategy: ChunkStrategy,
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
    #[serde(default)]
    pub overlap: usize,
    #[serde(default)]
    pub mtime: Option<i64>,
}

pub fn default_max_lines() -> usize {
    40
}

#[derive(Clone, Debug, Serialize)]
pub struct Citation {
    pub document_id: String,
    pub source_uri: String,
    pub heading: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChunkView {
    pub ord: usize,
    pub text: String,
    pub citation: Citation,
}

#[derive(Clone, Debug, Serialize)]
pub struct DocumentRecord {
    pub id: String,
    pub source_uri: String,
    pub title: String,
    pub collection: String,
    pub hash: String,
    pub chunks: usize,
    pub reindexed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct DocumentView {
    pub id: String,
    pub source_uri: String,
    pub title: String,
    pub collection: String,
    pub metadata: serde_json::Value,
    pub hash: String,
    pub chunks: Vec<ChunkView>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScanSummary {
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub failed: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MigrationReport {
    pub schema_version_before: Option<i64>,
    pub schema_version_after: i64,
    pub backfilled: usize,
}

/// A chunk produced by a `Chunker`, before embedding/storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub ord: usize,
    pub text: String,
    pub start_line: usize,
    pub end_line: usize,
    pub heading_path: String,
}

pub trait Chunker {
    fn chunk(&self, content: &str) -> Vec<Chunk>;
}

pub struct FixedChunker {
    pub max_lines: usize,
    pub overlap: usize,
}

impl Chunker for FixedChunker {
    fn chunk(&self, content: &str) -> Vec<Chunk> {
        let lines: Vec<&str> = content.lines().collect();
        windows(&lines, 1, "", self.max_lines, self.overlap)
    }
}

pub struct HeaderChunker {
    pub max_lines: usize,
    pub overlap: usize,
}

impl Chunker for HeaderChunker {
    fn chunk(&self, content: &str) -> Vec<Chunk> {
        let lines: Vec<&str> = content.lines().collect();
        let mut stack: Vec<(usize, String)> = Vec::new();
        let mut sections: Vec<(usize, Vec<&str>, String)> = Vec::new();
        let mut current: Option<(usize, Vec<&str>, String)> = None;

        for (idx, line) in lines.iter().enumerate() {
            let header = parse_header(line);
            if let Some((level, title)) = header {
                if let Some(section) = current.take() {
                    sections.push(section);
                }
                while stack.last().is_some_and(|(l, _)| *l >= level) {
                    stack.pop();
                }
                stack.push((level, title.to_string()));
                let path = stack.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" > ");
                current = Some((idx + 1, vec![*line], path));
            } else if let Some(section) = current.as_mut() {
                section.1.push(*line);
            } else {
                current = Some((idx + 1, vec![*line], String::new()));
            }
        }
        if let Some(section) = current {
            sections.push(section);
        }

        let mut out = Vec::new();
        for (start, section_lines, heading) in sections {
            let part = windows(&section_lines, start, &heading, self.max_lines, self.overlap);
            out.extend(part);
        }
        out
    }
}

fn parse_header(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let level = trimmed.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&level)
        && trimmed.as_bytes().get(level) == Some(&b' ')
        && trimmed.len() > level + 1
    {
        Some((level, trimmed[level + 1..].trim()))
    } else {
        None
    }
}

fn windows(
    lines: &[&str],
    offset: usize,
    heading: &str,
    max_lines: usize,
    overlap: usize,
) -> Vec<Chunk> {
    let max_lines = max_lines.max(1);
    let overlap = overlap.clamp(0, max_lines.saturating_sub(1));
    let step = max_lines - overlap;
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < lines.len() {
        let end = (start + max_lines).min(lines.len());
        let text = lines[start..end].join("\n").trim().to_string();
        if !text.is_empty() {
            out.push(Chunk {
                ord: out.len(),
                text,
                start_line: offset + start,
                end_line: offset + end - 1,
                heading_path: heading.to_string(),
            });
        }
        if end == lines.len() {
            break;
        }
        start += step;
    }
    out
}

/// Split simple `---` frontmatter into a JSON object. No YAML dependency:
/// `key: value` scalars (and JSON arrays) are enough for titles/tags.
pub fn split_frontmatter(content: &str) -> (serde_json::Value, &str) {
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return (serde_json::Value::Object(Default::default()), content);
    }
    let body = content.split_once("---") .map(|(_, rest)| rest).unwrap_or(content);
    let Some((front, rest)) = body.split_once("---") else {
        return (serde_json::Value::Object(Default::default()), content);
    };
    let mut map = serde_json::Map::new();
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim().trim_matches('"').to_string();
            let parsed = serde_json::from_str(&value)
                .unwrap_or(serde_json::Value::String(value.clone()));
            map.insert(key.to_string(), parsed);
        }
    }
    let rest = rest.strip_prefix('\n').or_else(|| rest.strip_prefix("\r\n")).unwrap_or(rest);
    (serde_json::Value::Object(map), rest)
}

fn digest_hex(text: &str) -> String {
    let bytes = Sha256::digest(text.as_bytes());
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn document_id(source_uri: &str) -> String {
    format!("doc_{}", &digest_hex(source_uri)[..12])
}

fn chunk_id(doc_id: &str, ord: usize) -> String {
    format!("{doc_id}-{ord:04}")
}

fn file_id(source_uri: &str) -> String {
    format!("file_{}", &digest_hex(source_uri)[..12])
}

fn ensure_schema_version(db: &Hako) -> Result<i64> {
    if let Some(doc) = db.get("meta", "schema_version")? {
        if let Some(Value::Int(version)) = doc.get("version") {
            return Ok(*version);
        }
    }
    let mut doc = HakoDoc::default();
    doc.insert("version", Value::Int(SCHEMA_VERSION));
    db.put("meta", "schema_version", &doc)?;
    Ok(SCHEMA_VERSION)
}

fn str_field(doc: &HakoDoc, field: &str) -> String {
    match doc.get(field) {
        Some(Value::String(s)) => s.clone(),
        Some(value) => format!("{value:?}"),
        None => String::new(),
    }
}

fn metadata_of(doc: &HakoDoc) -> serde_json::Value {
    match doc.get("metadata") {
        Some(Value::String(json)) => serde_json::from_str(json).unwrap_or_default(),
        _ => serde_json::Value::Object(Default::default()),
    }
}

/// Check the manifest. Returns true when content changed or no manifest exists.
pub fn needs_reindex(db: &Hako, source_uri: &str, hash: &str, mtime: Option<i64>) -> Result<bool> {
    let Some(doc) = db.get(FILES, &file_id(source_uri))? else {
        return Ok(true);
    };
    let same_hash = matches!(doc.get("hash"), Some(Value::String(h)) if h == hash);
    let same_mtime = match (doc.get("mtime"), mtime) {
        (Some(Value::Int(stored)), Some(now)) => *stored == now,
        _ => true, // absent mtime is not a reason to reindex
    };
    Ok(!same_hash || !same_mtime)
}

/// Insert or update one document, replacing its chunks idempotently.
pub fn upsert_document(
    db: &Hako,
    embedder: &SharedEmbedder,
    input: DocumentInput,
) -> Result<DocumentRecord> {
    ensure_schema_version(db)?;
    let (frontmatter, body) = split_frontmatter(&input.content);
    let hash = digest_hex(&input.content);
    let id = document_id(&input.source_uri);
    let reindexed = needs_reindex(db, &input.source_uri, &hash, input.mtime)?;

    let mut metadata = frontmatter;
    if let Some(extra) = input.metadata {
        if let (Some(map), Some(extra)) = (
            metadata.as_object_mut(),
            extra.as_object(),
        ) {
            for (key, value) in extra {
                map.insert(key.clone(), value.clone());
            }
        }
    }
    let title = input
        .title
        .or_else(|| {
            metadata
                .get("title")
                .and_then(|value| match value {
                    serde_json::Value::String(title) => Some(title.clone()),
                    _ => None,
                })
        })
        .unwrap_or_else(|| {
            std::path::Path::new(&input.source_uri)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| input.source_uri.clone())
        });
    let collection = input.collection.unwrap_or_else(|| "default".into());

    let chunker: Box<dyn Chunker> = match input.strategy {
        ChunkStrategy::Fixed => Box::new(FixedChunker {
            max_lines: input.max_lines,
            overlap: input.overlap,
        }),
        ChunkStrategy::Header => Box::new(HeaderChunker {
            max_lines: input.max_lines,
            overlap: input.overlap,
        }),
    };
    let chunks = chunker.chunk(body);
    let texts: Vec<String> = chunks.iter().map(|chunk| chunk.text.clone()).collect();
    let vectors = embedder.embed(&texts)?;

    // Delete stale chunks by deterministic id (idempotent reindex).
    let query = Query::new(CHUNKS)
        .where_filter(
            "doc_id",
            Operator::Eq,
            Value::String(id.clone()),
        )
        .limit(100_000);
    let existing: Vec<String> = db
        .query(query)?
        .into_iter()
        .map(|(chunk_id, _)| chunk_id)
        .collect();
    let new_ids: Vec<String> = (0..chunks.len()).map(|ord| chunk_id(&id, ord)).collect();
    for stale in existing.iter().filter(|id| !new_ids.contains(id)) {
        db.delete(CHUNKS, stale)?;
    }

    for (ord, (chunk, vector)) in chunks.iter().zip(vectors.iter()).enumerate() {
        let mut doc = HakoDoc::default();
        doc.insert("doc_id", Value::String(id.clone()));
        doc.insert("source_uri", Value::String(input.source_uri.clone()));
        doc.insert("path", Value::String(input.source_uri.clone()));
        doc.insert("ord", Value::Int(ord as i64));
        doc.insert("content", Value::String(chunk.text.clone()));
        doc.insert("emb", Value::Binary(hakodb::index::vector::encode_f32s(vector)));
        doc.insert("heading_path", Value::String(chunk.heading_path.clone()));
        doc.insert("start_line", Value::Int(chunk.start_line as i64));
        doc.insert("end_line", Value::Int(chunk.end_line as i64));
        doc.insert("collection", Value::String(collection.clone()));
        db.put(CHUNKS, &chunk_id(&id, ord), &doc)?;
    }

    let mut document = HakoDoc::default();
    document.insert("source_uri", Value::String(input.source_uri.clone()));
    document.insert("title", Value::String(title.clone()));
    document.insert("collection", Value::String(collection.clone()));
    document.insert("metadata", Value::String(serde_json::to_string(&metadata)?));
    document.insert("hash", Value::String(hash.clone()));
    document.insert("chunk_count", Value::Int(chunks.len() as i64));
    db.put(DOCUMENTS, &id, &document)?;

    let mut manifest = HakoDoc::default();
    manifest.insert("source_uri", Value::String(input.source_uri.clone()));
    manifest.insert("path", Value::String(input.source_uri.clone()));
    manifest.insert("doc_id", Value::String(id.clone()));
    manifest.insert("hash", Value::String(hash.clone()));
    manifest.insert("mtime", Value::Int(input.mtime.unwrap_or(0)));
    manifest.insert("status", Value::String("indexed".into()));
    manifest.insert("collection", Value::String(collection.clone()));
    db.put(FILES, &file_id(&input.source_uri), &manifest)?;

    Ok(DocumentRecord {
        id,
        source_uri: input.source_uri,
        title,
        collection,
        hash,
        chunks: chunks.len(),
        reindexed,
    })
}

pub fn get_document(db: &Hako, source_uri: &str) -> Result<Option<DocumentView>> {
    let id = document_id(source_uri);
    let Some(doc) = db.get(DOCUMENTS, &id)? else {
        return Ok(None);
    };
    let query = Query::new(CHUNKS)
        .where_filter("doc_id", Operator::Eq, Value::String(id.clone()))
        .limit(100_000);
    let mut chunks = Vec::new();
    for (_, chunk) in db.query(query)? {
        let start_line = match chunk.get("start_line") {
            Some(Value::Int(n)) => *n as usize,
            _ => 0,
        };
        let end_line = match chunk.get("end_line") {
            Some(Value::Int(n)) => *n as usize,
            _ => start_line,
        };
        let ord = match chunk.get("ord") {
            Some(Value::Int(n)) => *n as usize,
            _ => 0,
        };
        chunks.push(ChunkView {
            ord,
            text: str_field(&chunk, "content"),
            citation: Citation {
                document_id: id.clone(),
                source_uri: str_field(&chunk, "source_uri"),
                heading: str_field(&chunk, "heading_path"),
                start_line,
                end_line,
            },
        });
    }
    chunks.sort_by_key(|chunk| chunk.ord);
    Ok(Some(DocumentView {
        id,
        source_uri: str_field(&doc, "source_uri"),
        title: str_field(&doc, "title"),
        collection: str_field(&doc, "collection"),
        metadata: metadata_of(&doc),
        hash: str_field(&doc, "hash"),
        chunks,
    }))
}

pub fn list_documents(db: &Hako, collection: Option<&str>, limit: usize) -> Result<Vec<DocumentView>> {
    let mut query = Query::new(DOCUMENTS).limit(limit.clamp(1, 100_000));
    if let Some(collection) = collection {
        query = query.where_filter("collection", Operator::Eq, Value::String(collection.to_string()));
    }
    let mut out = Vec::new();
    for (_, doc) in db.query(query)? {
        out.push(DocumentView {
            id: str_field(&doc, "source_uri"),
            source_uri: str_field(&doc, "source_uri"),
            title: str_field(&doc, "title"),
            collection: str_field(&doc, "collection"),
            metadata: metadata_of(&doc),
            hash: str_field(&doc, "hash"),
            chunks: Vec::new(),
        });
    }
    Ok(out)
}

/// Delete a document and its chunks; the manifest entry is kept as `removed`
/// so a later scan does not re-learn the deleted file on every run.
pub fn delete_document(db: &Hako, source_uri: &str) -> Result<bool> {
    let id = document_id(source_uri);
    if db.get(DOCUMENTS, &id)?.is_none() {
        return Ok(false);
    }
    let query = Query::new(CHUNKS)
        .where_filter("doc_id", Operator::Eq, Value::String(id.clone()))
        .limit(100_000);
    for (chunk_id, _) in db.query(query)? {
        db.delete(CHUNKS, &chunk_id)?;
    }
    db.delete(DOCUMENTS, &id)?;
    if let Some(mut manifest) = db.get(FILES, &file_id(source_uri))? {
        manifest.insert("status", Value::String("removed".into()));
        db.put(FILES, &file_id(source_uri), &manifest)?;
    }
    Ok(true)
}

fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if !matches!(name.as_str(), ".git" | "node_modules" | "target" | ".cache") {
                collect_files(&path, out)?;
            }
        } else if path.is_file()
            && matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("md" | "markdown" | "txt")
            )
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Incremental directory ingestion driven by the `files` manifest.
pub fn scan_directory(
    db: &Hako,
    embedder: &SharedEmbedder,
    root: &std::path::Path,
    input: &DocumentInput,
) -> Result<ScanSummary> {
    let mut summary = ScanSummary::default();
    let mut files = Vec::new();
    collect_files(root, &mut files)?;
    let mut seen = std::collections::HashSet::new();
    for path in files {
        let source_uri = path.to_string_lossy().to_string();
        seen.insert(source_uri.clone());
        let content = match std::fs::read_to_string(&path) {
            Ok(content) if content.len() <= 4 * 1024 * 1024 => content,
            _ => {
                summary.failed += 1;
                continue;
            }
        };
        let hash = digest_hex(&content);
        let mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64);
        if !needs_reindex(db, &source_uri, &hash, mtime)? {
            summary.unchanged += 1;
            continue;
        }
        let mut doc_input = input.clone();
        doc_input.source_uri = source_uri;
        doc_input.content = content;
        doc_input.mtime = mtime;
        match upsert_document(db, embedder, doc_input) {
            Ok(_) => summary.indexed += 1,
            Err(_) => summary.failed += 1,
        }
    }
    // Files that disappeared from disk: remove document + chunks.
    let manifest_query = Query::new(FILES).limit(100_000);
    let tracked: Vec<(String, HakoDoc)> = db.query(manifest_query)?;
    for (id, doc) in tracked {
        let source_uri = match doc.get("source_uri") {
            Some(Value::String(uri)) => uri.clone(),
            _ => continue,
        };
        if input.collection.as_deref().is_some_and(|wanted| {
            !matches!(doc.get("collection"), Some(Value::String(c)) if c == wanted)
        }) {
            continue;
        }
        if seen.contains(&source_uri) || std::path::Path::new(&source_uri).exists() {
            continue;
        }
        if delete_document(db, &source_uri)? {
            summary.removed += 1;
        }
        let _ = id; // manifest id already known via source_uri
    }
    Ok(summary)
}

/// Backfill `meta/schema_version` and missing fields. No re-embedding.
pub fn migrate(db: &Hako, dry_run: bool) -> Result<MigrationReport> {
    let before = db
        .get("meta", "schema_version")?
        .and_then(|doc| match doc.get("version") {
            Some(Value::Int(v)) => Some(*v),
            _ => None,
        });
    let mut report = MigrationReport {
        schema_version_before: before,
        schema_version_after: SCHEMA_VERSION,
        backfilled: 0,
    };
    if dry_run {
        return Ok(report);
    }
    ensure_schema_version(db)?;
    let query = Query::new(DOCUMENTS).limit(100_000);
    for (id, doc) in db.query(query)? {
        let mut doc = doc;
        if doc.get("source_uri").is_none() {
            doc.insert("source_uri", Value::String(id.clone()));
            report.backfilled += 1;
        }
        if doc.get("metadata").is_none() {
            doc.insert("metadata", Value::String("{}".into()));
            report.backfilled += 1;
        }
        if report.backfilled > 0 {
            db.put(DOCUMENTS, &id, &doc)?;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_chunker_tracks_heading_and_lines() {
        let content = "# Title\nintro\n\n## Section\nbody one\nbody two\n";
        let chunks = HeaderChunker { max_lines: 20, overlap: 0 }.chunk(content);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].heading_path, "Title");
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[1].heading_path, "Title > Section");
        assert_eq!(chunks[1].end_line, 6);
    }

    #[test]
    fn fixed_chunker_overlaps() {
        let content = "1\n2\n3\n4\n5\n";
        let chunks = FixedChunker { max_lines: 3, overlap: 1 }.chunk(content);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "1\n2\n3");
        assert_eq!(chunks[1].text, "3\n4\n5");
    }

    #[test]
    fn frontmatter_parses_title_and_tags() {
        let (meta, body) = split_frontmatter("---\ntitle: RAG\ntags: [\"ai\", \"rag\"]\n---\n# Body\n");
        assert_eq!(meta["title"], "RAG");
        assert!(meta["tags"].as_array().is_some());
        assert!(body.starts_with("# Body"));
    }
}

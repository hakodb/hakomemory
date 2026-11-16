//! Handler HTTP /v1.

use crate::indexer;
use crate::state::AppState;
use crate::store::project_id;
use anyhow::{anyhow, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use serde::Deserialize;
use std::sync::Arc;

pub async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

// ---- helper ----

fn err500(e: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() })))
}

fn err400(e: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e.to_string() })))
}

#[derive(Deserialize)]
pub struct ProjectsOpen {
    /// path canonical project (klien mengirim dari ctx.location.project)
    pub path: String,
}

pub async fn projects_open(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ProjectsOpen>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let store = state.store.clone();
    let path = input.path.clone();
    let res = tokio::task::spawn_blocking(move || {
        let (pid, db) = store.open_project(&path)?;
        assert!(db.await_quiescent(std::time::Duration::from_secs(600)), "recovery index harus selesai");
        Ok::<_, anyhow::Error>(pid)
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(serde_json::json!({ "project_id": res })))
}

#[derive(Deserialize)]
pub struct IndexScan {
    pub project_id: String,
    /// root direktori/file yang di-index
    pub path: String,
}

pub async fn index_scan(
    State(state): State<Arc<AppState>>,
    Json(input): Json<IndexScan>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let store = state.store.clone();
    let embedder = state.embedder.clone();
    let (pid, path) = (input.project_id.clone(), input.path.clone());
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        indexer::scan(&db, &embedder, std::path::Path::new(&path))
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(serde_json::to_value(res).unwrap()))
}

#[derive(Deserialize)]
pub struct IndexStatus {
    pub project_id: String,
}

pub async fn index_status(
    State(state): State<Arc<AppState>>,
    Json(input): Json<IndexStatus>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let store = state.store.clone();
    let pid = input.project_id.clone();
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        Ok::<_, anyhow::Error>(serde_json::json!({
            "vector_complete": db.vector_index_complete("chunks", "emb"),
            "quiescent": db.is_quiescent(),
        }))
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(res))
}

#[derive(Deserialize)]
pub struct Retrieve {
    pub project_id: String,
    pub query: String,
    #[serde(default = "default_k")]
    pub k: usize,
}

fn default_k() -> usize {
    5
}

pub async fn retrieve(
    State(state): State<Arc<AppState>>,
    Json(input): Json<Retrieve>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let store = state.store.clone();
    let embedder = state.embedder.clone();
    let (pid, query, k) = (input.project_id.clone(), input.query.clone(), input.k);
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        let qv = embedder.embed_one(&query)?;
        let rows = db.hybrid_search("chunks", "content", &query, "emb", &qv, k, 60)?;
        let hits: Vec<serde_json::Value> = rows
            .iter()
            .map(|(id, score, doc)| {
                serde_json::json!({
                    "id": id,
                    "score": score,
                    "path": str_field(doc, "path"),
                    "content": str_field(doc, "content"),
                    "start_line": int_field(doc, "start_line"),
                })
            })
            .collect();
        Ok::<_, anyhow::Error>(serde_json::json!({ "hits": hits }))
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(res))
}

#[derive(Deserialize)]
pub struct GraphContext {
    pub project_id: String,
    /// seed "collection/id" (mis. "symbols/sym_auth")
    pub seed: String,
    #[serde(default = "default_hops")]
    pub hops: usize,
}

fn default_hops() -> usize {
    2
}

pub async fn graph_context(
    State(state): State<Arc<AppState>>,
    Json(input): Json<GraphContext>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let (col, id) = input
        .seed
        .split_once('/')
        .ok_or_else(|| err400(anyhow!("seed harus berformat collection/id")))?;
    let store = state.store.clone();
    let hops = input.hops.clamp(1, 8);
    let col = col.to_string();
    let id = id.to_string();
    let pid = input.project_id.clone();
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        let spec = hakodb::index::traverse::TraverseSpec {
            direction: hakodb::index::traverse::TraverseDir::Outgoing,
            via_edge: Some("edges".into()),
            max_depth: hops,
            limit: 200,
            edge_field: None,
            weight_field: None,
            node_match: None,
            edge_type: None,
            edge_match: None,
            include_path: false,
            include_via: false,
        };
        let json = db.subgraph_json(&col, &id, &spec)?;
        let v: serde_json::Value = serde_json::from_str(&json)?;
        Ok::<_, anyhow::Error>(v)
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(res))
}

#[derive(Deserialize)]
pub struct MemoryWrite {
    pub project_id: String,
    /// episodic | semantic | procedural
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

pub async fn memory_write(
    State(state): State<Arc<AppState>>,
    Json(input): Json<MemoryWrite>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let col = memory_col(&input.kind).map_err(err400)?;
    let store = state.store.clone();
    let embedder = state.embedder.clone();
    let pid = input.project_id.clone();
    let text = input.text.clone();
    let metadata = input.metadata.clone();
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        let emb = embedder.embed_one(&text)?;
        let id = {
            use sha2::{Digest, Sha256};
            let h = Sha256::digest(format!("{text}|{}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0))
                .as_bytes());
            h.iter().take(8).map(|b| format!("{b:02x}")).collect::<String>()
        };
        let mut d = HakoDoc::default();
        d.insert("text", Value::String(text.clone()));
        d.insert("emb", Value::Binary(hakodb::index::vector::encode_f32s(&emb)));
        d.insert(
            "ts",
            Value::Int(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_micros() as i64)
                    .unwrap_or(0),
            ),
        );
        if let Some(md) = metadata {
            d.insert(
                "metadata",
                Value::String(serde_json::to_string(&md).unwrap_or_default()),
            );
        }
        db.put(&col, &id, &d)?;
        Ok::<_, anyhow::Error>(serde_json::json!({ "id": id, "collection": col }))
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(res))
}

#[derive(Deserialize)]
pub struct MemorySearch {
    pub project_id: String,
    pub kind: String,
    pub query: String,
    #[serde(default = "default_k")]
    pub k: usize,
}

pub async fn memory_search(
    State(state): State<Arc<AppState>>,
    Json(input): Json<MemorySearch>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let col = memory_col(&input.kind).map_err(err400)?;
    let store = state.store.clone();
    let embedder = state.embedder.clone();
    let (pid, query, k) = (input.project_id.clone(), input.query.clone(), input.k);
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        let qv = embedder.embed_one(&query)?;
        let rows = db.hybrid_search(&col, "text", &query, "emb", &qv, k, 60)?;
        let hits: Vec<serde_json::Value> = rows
            .iter()
            .map(|(id, score, doc)| {
                serde_json::json!({
                    "id": id,
                    "score": score,
                    "text": str_field(doc, "text"),
                    "ts": int_field(doc, "ts"),
                })
            })
            .collect();
        Ok::<_, anyhow::Error>(serde_json::json!({ "hits": hits }))
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(res))
}

#[derive(Deserialize)]
pub struct MemoryForget {
    pub project_id: String,
    pub kind: String,
    pub id: String,
}

pub async fn memory_forget(
    State(state): State<Arc<AppState>>,
    Json(input): Json<MemoryForget>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let col = memory_col(&input.kind).map_err(err400)?;
    let store = state.store.clone();
    let (pid, id) = (input.project_id.clone(), input.id.clone());
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        db.delete(&col, &id)?; // soft delete (restoreable) — hakodb v0.20.0
        Ok::<_, anyhow::Error>(serde_json::json!({ "forgotten": id }))
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(res))
}

// ---- util ----

fn memory_col(kind: &str) -> Result<&'static str> {
    Ok(match kind {
        "episodic" => "mem_episodic",
        "semantic" => "mem_semantic",
        "procedural" => "mem_procedural",
        other => return Err(anyhow!("kind tidak dikenal: {other}")),
    })
}

fn str_field(doc: &HakoDoc, key: &str) -> String {
    match doc.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn int_field(doc: &HakoDoc, key: &str) -> i64 {
    match doc.get(key) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    }
}

#[allow(dead_code)]
fn _assert_pub_invariants() {
    let _ = project_id("x");
}

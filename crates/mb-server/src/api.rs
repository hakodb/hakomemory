//! Handler HTTP /v1.

use crate::auth::AuthContext;
use crate::indexer;
use crate::state::AppState;
use crate::store::project_id;
use anyhow::{anyhow, Result};
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::query::filter::{Filter, Operator};
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

fn require_admin(auth: &AuthContext) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if auth.admin {
        Ok(())
    } else {
        Err(auth_err(StatusCode::FORBIDDEN))
    }
}

/// Konversi kegagalan otorisasi ke respons API.
fn auth_err(code: StatusCode) -> (StatusCode, Json<serde_json::Value>) {
    let msg = if code == StatusCode::FORBIDDEN { "forbidden" } else { "unauthorized" };
    (code, Json(serde_json::json!({ "error": msg })))
}

#[derive(Deserialize)]
pub struct ProjectsOpen {
    /// path canonical project (kompatibel dengan klien lama)
    pub path: Option<String>,
    /// alias yang dipilih klien; path diputuskan server dari registry
    #[serde(default)]
    pub alias: Option<String>,
}

pub async fn projects_open(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<ProjectsOpen>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let sub = auth.sub.clone();
    let (pid_resolved, path): (String, String) = if let Some(alias) = input.alias.as_deref() {
        match state.resolve_alias(&sub, alias) {
            Some(entry) => (entry.pid, entry.path),
            None => {
                let raw = input.path.clone().ok_or_else(|| {
                    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "alias belum terdaftar; sertakan path untuk mendaftar" })))
                })?;
                auth.require_path(&raw).map_err(auth_err)?;
                let canonical = std::fs::canonicalize(&raw)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or(raw);
                let pid = project_id(&canonical);
                state
                    .register_project(&sub, Some(alias), &pid, &canonical)
                    .map_err(err500)?;
                (pid, canonical)
            }
        }
    } else if let Some(raw) = input.path.clone() {
        auth.require_path(&raw).map_err(auth_err)?;
        let canonical = std::fs::canonicalize(&raw)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or(raw);
        let pid = project_id(&canonical);
        state
            .register_project(&sub, None, &pid, &canonical)
            .map_err(err500)?;
        (pid, canonical)
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "path atau alias wajib" })),
        ));
    };
    // pastikan pid yang dipilih server memang diizinkan untuk user ini
    state.authorize_project(&auth, &pid_resolved).map_err(auth_err)?;
    let app = state.clone();
    let pid = pid_resolved;
    let res = tokio::task::spawn_blocking(move || {
        let (_, db) = app.store.open_project(&path)?;
        assert!(db.await_quiescent(std::time::Duration::from_secs(600)), "recovery index harus selesai");
        // auto-watch (MEMORYBOX_WATCH=0 untuk mematikan)
        if std::env::var("MEMORYBOX_WATCH").as_deref() != Ok("0") && app.watches.ensure(&pid) {
            if let Err(e) = crate::watcher::spawn(
                app.store.clone(),
                app.embedder.clone(),
                pid.clone(),
                std::path::PathBuf::from(&path),
            ) {
                tracing::warn!("watcher gagal dimulai: {e}");
            }
        }
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
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<IndexScan>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    let store = state.store.clone();
    let embedder = state.embedder.clone();
    let (pid, path) = (input.project_id.clone(), input.path.clone());
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        // rel selalu terhadap root project (meta); fallback: path scan
        let base = crate::store::read_root(&db)?.unwrap_or_else(|| path.clone());
        indexer::scan(&db, &embedder, std::path::Path::new(&base), std::path::Path::new(&path))
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(serde_json::to_value(res).unwrap()))
}

#[derive(Deserialize)]
pub struct IndexRemove {
    pub project_id: String,
    /// path file (rel terhadap root project, atau absolut)
    pub path: String,
}

/// Hapus semua chunk/simbol/edge milik satu file (file terhapus).
pub async fn index_remove(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<IndexRemove>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    let store = state.store.clone();
    let (pid, path) = (input.project_id.clone(), input.path.clone());
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        let base = crate::store::read_root(&db)?.unwrap_or_default();
        // terima rel atau absolut
        let rel = if std::path::Path::new(&path).is_absolute() {
            indexer::rel_of_pub(std::path::Path::new(&path), std::path::Path::new(&base))
        } else {
            path
        };
        let removed = indexer::remove_rel(&db, &rel)?;
        Ok::<_, anyhow::Error>(serde_json::json!({ "removed": removed, "rel": rel }))
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(res))
}

#[derive(Deserialize)]
pub struct IndexStatus {
    pub project_id: String,
}

pub async fn index_status(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Query(input): Query<IndexStatus>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
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
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<Retrieve>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
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
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<GraphContext>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
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
            direction: hakodb::index::traverse::TraverseDir::Both,
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
        let mut v: serde_json::Value = serde_json::from_str(&json)?;
        // buang field emb (384 float) dari node chunk — plugin hanya butuh
        // path/content/start_line; tanpa ini respons ~70% binatang kosong
        if let Some(nodes) = v.get_mut("nodes").and_then(|n| n.as_array_mut()) {
            for n in nodes {
                if let Some(doc) = n.get_mut("doc").and_then(|d| d.as_object_mut()) {
                    doc.remove("emb");
                }
            }
        }
        Ok::<_, anyhow::Error>(v)
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(res))
}

#[derive(Deserialize)]
pub struct SymbolsLookup {
    pub project_id: String,
    pub name: String,
    #[serde(default = "default_k")]
    pub k: usize,
}

/// Resolve nama simbol → id + lokasi (untuk seed graph dari plugin).
pub async fn symbols_lookup(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<SymbolsLookup>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    let store = state.store.clone();
    let (pid, name, k) = (input.project_id.clone(), input.name.clone(), input.k);
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        let rows = crate::indexer::lookup_symbol(&db, &name, k.min(50))?;
        let hits: Vec<serde_json::Value> = rows
            .iter()
            .map(|(id, doc)| {
                serde_json::json!({
                    "id": id,
                    "name": str_field(doc, "name"),
                    "kind": str_field(doc, "kind"),
                    "path": str_field(doc, "path"),
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
pub struct MemoryWrite {
    pub project_id: String,
    /// episodic | semantic | procedural
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

pub async fn memory_write(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<MemoryWrite>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    let col = memory_col(&input.kind).map_err(err400)?;
    let store = state.store.clone();
    let embedder = state.embedder.clone();
    let pid = input.project_id.clone();
    let text = input.text.clone();
    let metadata = input.metadata.clone();
    let sub = auth.sub.clone();
    let agent_id = input.agent_id.clone();
    let session_id = input.session_id.clone();
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
        d.insert("user_id", Value::String(sub.clone()));
        if let Some(a) = agent_id {
            d.insert("agent_id", Value::String(a));
        }
        if let Some(sess) = session_id {
            d.insert("session_id", Value::String(sess));
        }
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
    /// opsional: episodic/semantic/procedural; tanpa ini = gabung semua
    pub kind: Option<String>,
    pub query: String,
    #[serde(default = "default_k")]
    pub k: usize,
}

pub async fn memory_search(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<MemorySearch>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    // 0..3 koleksi memory sesuai kind (tanpa kind = semua)
    let cols: Vec<(String, String)> = match input.kind.as_deref() {
        Some(k) => vec![(memory_col(k).map_err(err400)?.to_string(), k.to_string())],
        None => crate::store::MEMORY_COLLECTIONS
            .iter()
            .map(|c| (c.to_string(), c[4..].to_string())) // mem_episodic -> episodic
            .collect(),
    };
    let store = state.store.clone();
    let embedder = state.embedder.clone();
    let (pid, query, k) = (input.project_id.clone(), input.query.clone(), input.k);
    // v0.21.0: filter owner ditaruh di engine (sebelum peringkat), bukan
    // over-fetch + buang di aplikasi. Admin tanpa filter melihat semua,
    // termasuk dokumen legacy tanpa user_id.
    let filters: Vec<Filter> = if auth.admin {
        Vec::new()
    } else {
        vec![Filter {
            field: "user_id".into(),
            op: Operator::Eq,
            value: Value::String(auth.sub.clone()),
        }]
    };
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        let qv = embedder.embed_one(&query)?;
        // k per koleksi lalu merge-score, ambil k terbaik
        let mut hits: Vec<serde_json::Value> = Vec::new();
        for (col, kind) in &cols {
            let rows = db.hybrid_search_filtered(
                col.as_str(), "text", &query, "emb", &qv, k, 60, &filters,
            )?;
            for (id, score, doc) in rows {
                hits.push(serde_json::json!({
                    "id": id,
                    "kind": kind.as_str(),
                    "score": score,
                    "text": str_field(&doc, "text"),
                    "ts": int_field(&doc, "ts"),
                }));
            }
        }
        hits.sort_by(|a, b| b["score"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["score"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(k);
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
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<MemoryForget>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    let col = memory_col(&input.kind).map_err(err400)?;
    let store = state.store.clone();
    let (pid, id) = (input.project_id.clone(), input.id.clone());
    let sub = auth.sub.clone();
    let is_admin = auth.admin;
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        if let Some(doc) = db.get(&col, &id)? {
            if !is_admin && !matches!(doc.get("user_id"), Some(Value::String(u)) if u == &sub) {
                return Ok::<_, anyhow::Error>(serde_json::json!({ "forbidden": true }));
            }
        }
        db.delete(&col, &id)?; // soft delete (restoreable) — hakodb v0.20.0
        Ok::<_, anyhow::Error>(serde_json::json!({ "forgotten": id }))
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    if res.get("forbidden").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(auth_err(StatusCode::FORBIDDEN));
    }
    Ok(Json(res))
}

#[derive(Deserialize)]
pub struct BackfillOwner {
    pub project_id: String,
    pub user_id: String,
    /// episodic | semantic | procedural; tanpa ini = semua koleksi
    pub kind: Option<String>,
}

/// Stamp `user_id` pada dokumen memory lama yang belum punya owner.
/// Hanya admin. Dokumen tanpa user_id disembunyikan dari user biasa直到 backfill.
pub async fn admin_backfill_owner(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<BackfillOwner>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_admin(&auth)?;
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    let cols: Vec<String> = match input.kind.as_deref() {
        Some(k) => vec![memory_col(k).map_err(err400)?.to_string()],
        None => crate::store::MEMORY_COLLECTIONS.iter().map(|c| c.to_string()).collect(),
    };
    let store = state.store.clone();
    let (pid, owner) = (input.project_id.clone(), input.user_id.clone());
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        let mut stamped = 0usize;
        for col in cols {
            let q = hakodb::query::query::Query::new(&col).limit(100_000);
            let rows = db.query(q)?;
            for (id, mut doc) in rows {
                if doc.get("user_id").is_none() {
                    doc.insert("user_id", Value::String(owner.clone()));
                    db.put(&col, &id, &doc)?;
                    stamped += 1;
                }
            }
        }
        Ok::<_, anyhow::Error>(serde_json::json!({ "stamped": stamped }))
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(res))
}

#[derive(Deserialize)]
pub struct RevokeToken {
    /// token lengkap (dari header klien) — jti/fingerprint dihitung server
    pub token: Option<String>,
    /// atau langsung kunci revokasi (jti)
    pub jti: Option<String>,
}

/// Cabut token: rotasi = mint token baru via mb-token lalu cabut yang lama.
pub async fn admin_revoke(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<RevokeToken>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_admin(&auth)?;
    let key = if let Some(jti) = input.jti {
        jti
    } else if let Some(token) = input.token {
        crate::auth::token_key(&state.auth, &token).map_err(auth_err)?
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "token atau jti wajib" })),
        ));
    };
    state.revoke_token(key.clone()).map_err(err500)?;
    Ok(Json(serde_json::json!({ "revoked": key })))
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

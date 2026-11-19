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

/// Lexical check plus canonical check: `..` and symlinks must not escape the
/// token roots. Both the caller's spelling and the canonical form are checked
/// because Windows canonicalize produces 8.3 short paths.
fn validate_ingest_path(
    auth: &AuthContext,
    path: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    // require_path checks both lexical and canonical forms (symlink-safe,
    // 8.3-path tolerant on Windows).
    auth.require_path(path).map_err(auth_err)
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
    validate_ingest_path(&auth, &input.path)?;
    let store = state.store.clone();
    let embedder = state.embedder.clone();
    // Feed the original path to the indexer: canonicalize on Windows yields
    // short (8.3) paths that break relative-path computation against `meta`.
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
    let (pid, query, k) = (input.project_id.clone(), input.query.clone(), input.k);
    let engine = state.engine.clone();
    let res = tokio::task::spawn_blocking(move || {
        let project = engine.project_by_id(&pid)?;
        let hits = project.retrieve(&query, k, 60)?;
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
        // v0.21.0: proyeksi di engine ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â `emb` tidak pernah ikut di-serialize.
        const FIELDS: &[&str] = &[
            "name", "kind", "path", "start_line", "ref_chunks", "content",
            "_from", "_to", "callee", "to_path", "w",
        ];
        let fields: Vec<String> = FIELDS.iter().map(|s| s.to_string()).collect();
        let json = db.subgraph_json_projected(&col, &id, &spec, Some(&fields))?;
        Ok::<_, anyhow::Error>(serde_json::from_str::<serde_json::Value>(&json)?)
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

/// Resolve nama simbol ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ id + lokasi (untuk seed graph dari plugin).
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
    match input.agent_id.as_deref() {
        Some(agent) => auth.require_agent(agent).map_err(auth_err)?,
        None if auth.allowed_agents().is_some() => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "agent_id wajib untuk token berscope agent" })),
            ))
        }
        None => {}
    }
    match input.session_id.as_deref() {
        Some(session) => auth.require_session(session).map_err(auth_err)?,
        None if auth.allowed_sessions().is_some() => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "session_id wajib untuk token berscope session" })),
            ))
        }
        None => {}
    }
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
        db.put(col, &id, &d)?;
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
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
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
    // v0.21.0: filter owner + scope agent/session ditaruh di engine
    // (sebelum peringkat). Admin tanpa filter melihat semua.
    let mut filters: Vec<Filter> = if auth.admin {
        Vec::new()
    } else {
        vec![Filter {
            field: "user_id".into(),
            op: Operator::Eq,
            value: Value::String(auth.sub.clone()),
        }]
    };
    if let Some(agent) = input.agent_id.as_deref() {
        auth.require_agent(agent).map_err(auth_err)?;
        filters.push(Filter {
            field: "agent_id".into(),
            op: Operator::Eq,
            value: Value::String(agent.to_string()),
        });
    } else if let Some(allowed) = auth.allowed_agents() {
        filters.push(Filter {
            field: "agent_id".into(),
            op: Operator::In,
            value: Value::Array(allowed.into_iter().map(Value::String).collect()),
        });
    }
    if let Some(session) = input.session_id.as_deref() {
        auth.require_session(session).map_err(auth_err)?;
        filters.push(Filter {
            field: "session_id".into(),
            op: Operator::Eq,
            value: Value::String(session.to_string()),
        });
    } else if let Some(allowed) = auth.allowed_sessions() {
        filters.push(Filter {
            field: "session_id".into(),
            op: Operator::In,
            value: Value::Array(allowed.into_iter().map(Value::String).collect()),
        });
    }
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
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

pub async fn memory_forget(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<MemoryForget>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    if let Some(agent) = input.agent_id.as_deref() {
        auth.require_agent(agent).map_err(auth_err)?;
    }
    if let Some(session) = input.session_id.as_deref() {
        auth.require_session(session).map_err(auth_err)?;
    }
    let col = memory_col(&input.kind).map_err(err400)?;
    let store = state.store.clone();
    let (pid, id) = (input.project_id.clone(), input.id.clone());
    let sub = auth.sub.clone();
    let is_admin = auth.admin;
    let req_agent = input.agent_id.clone();
    let req_session = input.session_id.clone();
    let allowed_agents = auth.allowed_agents();
    let allowed_sessions = auth.allowed_sessions();
    let res = tokio::task::spawn_blocking(move || {
        let db = store.get(&pid)?;
        if let Some(doc) = db.get(col, &id)? {
            let field_eq = |doc: &HakoDoc, field: &str, expected: &str| {
                matches!(doc.get(field), Some(Value::String(v)) if v == expected)
            };
            let field_in = |doc: &HakoDoc, field: &str, list: &[String]| {
                matches!(doc.get(field), Some(Value::String(v)) if list.contains(v))
            };
            let allowed = is_admin
                || (field_eq(&doc, "user_id", &sub)
                    && req_agent.as_deref().is_none_or(|a| field_eq(&doc, "agent_id", a))
                    && allowed_agents
                        .as_ref()
                        .is_none_or(|list| field_in(&doc, "agent_id", list))
                    && req_session.as_deref().is_none_or(|s| field_eq(&doc, "session_id", s))
                    && allowed_sessions
                        .as_ref()
                        .is_none_or(|list| field_in(&doc, "session_id", list)));
            if !allowed {
                return Ok::<_, anyhow::Error>(serde_json::json!({ "forbidden": true }));
            }
        }
        db.delete(col, &id)?; // soft delete (restoreable) ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â hakodb v0.20.0
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
/// Hanya admin. Dokumen tanpa user_id disembunyikan dari user biasaÃƒÂ§Ã¢â‚¬ÂºÃ‚Â´ÃƒÂ¥Ã‹â€ Ã‚Â° backfill.
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
    /// token lengkap (dari header klien) ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â jti/fingerprint dihitung server
    pub token: Option<String>,
    /// atau langsung kunci revokasi (jti)
    pub jti: Option<String>,
}

/// Cabut token: rotasi = mint token baru via hakomemory-token lalu cabut yang lama.
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

// ---- generic documents (#6) ----

use hakomemory_core::docs;

fn validate_source_uri(
    auth: &AuthContext,
    uri: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let path = std::path::Path::new(uri);
    if path.is_absolute() && path.exists() {
        validate_ingest_path(auth, uri)?;
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct DocumentUpsert {
    pub project_id: String,
    #[serde(flatten)]
    pub document: docs::DocumentInput,
}

pub async fn documents_upsert(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<DocumentUpsert>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    validate_source_uri(&auth, &input.document.source_uri)?;
    let (pid, doc) = (input.project_id.clone(), input.document);
    let engine = state.engine.clone();
    let res = tokio::task::spawn_blocking(move || {
        let project = engine.project_by_id(&pid)?;
        docs::upsert_document(project.db(), &project.embedder_handle(), doc)})
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(serde_json::to_value(res).map_err(|e| err500(anyhow!("serialize: {e}")))?))
}

#[derive(Deserialize)]
pub struct DocumentBatch {
    pub project_id: String,
    pub documents: Vec<docs::DocumentInput>,
}

pub async fn documents_batch(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<DocumentBatch>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    for document in &input.documents {
        validate_source_uri(&auth, &document.source_uri)?;
    }
    let (pid, documents) = (input.project_id.clone(), input.documents);
    let engine = state.engine.clone();
    let res = tokio::task::spawn_blocking(move || {
        let project = engine.project_by_id(&pid)?;
        let mut records = Vec::new();
        for document in documents {
            records.push(docs::upsert_document(
                project.db(),
                &project.embedder_handle(),
                document,
            )?);
        }
        Ok::<_, anyhow::Error>(records)
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(serde_json::to_value(res).map_err(|e| err500(anyhow!("serialize: {e}")))?))
}

#[derive(Deserialize)]
pub struct DocumentGetParams {
    pub project_id: String,
    pub source_uri: String,
}

pub async fn documents_get(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<DocumentGetParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &params.project_id).map_err(auth_err)?;
    validate_source_uri(&auth, &params.source_uri)?;
    let (pid, uri) = (params.project_id.clone(), params.source_uri.clone());
    let engine = state.engine.clone();
    let res = tokio::task::spawn_blocking(move || {
        let project = engine.project_by_id(&pid)?;
        docs::get_document(project.db(), &uri)})
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    match res {
        Some(view) => Ok(Json(serde_json::to_value(view).map_err(|e| err500(anyhow!("serialize: {e}")))?)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "document not found" })),
        )),
    }
}

#[derive(Deserialize)]
pub struct DocumentListParams {
    pub project_id: String,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default = "default_doc_limit")]
    pub limit: usize,
}

fn default_doc_limit() -> usize {
    50
}

pub async fn documents_list(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<DocumentListParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &params.project_id).map_err(auth_err)?;
    let (pid, collection, limit) = (
        params.project_id.clone(),
        params.collection.clone(),
        params.limit,
    );
    let engine = state.engine.clone();
    let res = tokio::task::spawn_blocking(move || {
        let project = engine.project_by_id(&pid)?;
        docs::list_documents(project.db(), collection.as_deref(), limit)})
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(serde_json::json!({ "documents": res })))
}

#[derive(Deserialize)]
pub struct DocumentDelete {
    pub project_id: String,
    pub source_uri: String,
}

pub async fn documents_delete(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<DocumentDelete>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    validate_source_uri(&auth, &input.source_uri)?;
    let (pid, uri) = (input.project_id.clone(), input.source_uri.clone());
    let response_uri = uri.clone();
    let engine = state.engine.clone();
    let deleted = tokio::task::spawn_blocking(move || {
        let project = engine.project_by_id(&pid)?;
        docs::delete_document(project.db(), &uri)})
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    if deleted {
        Ok(Json(serde_json::json!({ "deleted": response_uri })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "document not found" })),
        ))
    }
}

#[derive(Deserialize)]
pub struct DocumentScan {
    pub project_id: String,
    pub path: String,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub strategy: docs::ChunkStrategy,
    #[serde(default = "docs::default_max_lines")]
    pub max_lines: usize,
    #[serde(default)]
    pub overlap: usize,
}

pub async fn documents_scan(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<DocumentScan>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    validate_ingest_path(&auth, &input.path)?;
    let options = docs::DocumentInput {
        source_uri: String::new(),
        content: String::new(),
        collection: input.collection,
        title: None,
        metadata: None,
        strategy: input.strategy,
        max_lines: input.max_lines,
        overlap: input.overlap,
        mtime: None,
    };
    let (pid, path) = (input.project_id.clone(), input.path.clone());
    let engine = state.engine.clone();
    let res = tokio::task::spawn_blocking(move || {
        let project = engine.project_by_id(&pid)?;
        docs::scan_directory(project.db(), &project.embedder_handle(), std::path::Path::new(&path), &options)})
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(serde_json::to_value(res).map_err(|e| err500(anyhow!("serialize: {e}")))?))
}

#[derive(Deserialize)]
pub struct MigrateInput {
    pub project_id: String,
    #[serde(default)]
    pub dry_run: bool,
}

pub async fn admin_migrate(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<MigrateInput>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    let (pid, dry_run) = (input.project_id.clone(), input.dry_run);
    let engine = state.engine.clone();
    let res = tokio::task::spawn_blocking(move || {
        let project = engine.project_by_id(&pid)?;
        docs::migrate(project.db(), dry_run)})
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(serde_json::to_value(res).map_err(|e| err500(anyhow!("serialize: {e}")))?))
}
// ---- generic filtered retrieval (#7) ----

use hakomemory_core::retrieval;

#[derive(Deserialize)]
pub struct DocumentSearch {
    pub project_id: String,
    #[serde(flatten)]
    pub query: retrieval::RetrievalQuery,
}

pub async fn documents_search(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(input): Json<DocumentSearch>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    let (pid, query) = (input.project_id.clone(), input.query);
    let engine = state.engine.clone();
    let res = tokio::task::spawn_blocking(move || {
        let project = engine.project_by_id(&pid)?;
        retrieval::search(project.db(), &project.embedder_handle(), &query)
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(err500)?;
    Ok(Json(serde_json::to_value(res).map_err(|e| err500(anyhow!("serialize: {e}")))?))
}
// ---- generic graph analytics (#8) ----

use hakomemory_core::graph;

#[derive(Deserialize)]
pub struct GraphQuery {
    pub project_id: String,
    #[serde(flatten)]
    pub options: graph::GraphRequest,
    /// Target node for `/v1/graph/path` (`collection/id` or bare id).
    #[serde(default)]
    pub target: Option<String>,
}

pub async fn graph_analytics(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    uri: axum::http::Uri,
    Json(input): Json<GraphQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.authorize_project(&auth, &input.project_id).map_err(auth_err)?;
    let operation = uri.path().rsplit('/').next().unwrap_or("").to_string();
    let (pid, options, target) = (
        input.project_id.clone(),
        input.options,
        input.target.clone(),
    );
    let engine = state.engine.clone();
    let res = tokio::task::spawn_blocking(move || {
        let project = engine.project_by_id(&pid)?;
        let db = project.db();
        match operation.as_str() {
            "rank" => graph::rank(db, &options),
            "community" => graph::community(db, &options),
            "components" => graph::components(db, &options),
            "degrees" => graph::degrees(db, &options),
            "cycle" => graph::cycle(db, &options),
            "topo" => graph::topo(db, &options),
            "subgraph" => graph::subgraph(db, &options),
            "export" => graph::export(db, &options),
            "path" => {
                let target = target.ok_or_else(|| {
                    hakomemory_core::error::Error::InvalidInput("target is required".into())
                })?;
                graph::path(db, &options, &target)
            }
            other => Err(hakomemory_core::error::Error::InvalidInput(format!(
                "unknown graph operation '{other}'"
            ))),
        }
    })
    .await
    .map_err(|e| err500(anyhow!("join: {e}")))?
    .map_err(|e| err500(anyhow!("graph: {e}")))?;
    Ok(Json(serde_json::to_value(res).map_err(|e| err500(anyhow!("serialize: {e}")))?))
}
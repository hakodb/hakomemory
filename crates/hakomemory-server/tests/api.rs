//! Integration test untuk seluruh endpoint /v1/* (dan /healthz).
//! Memakai embedder mock deterministik: tidak butuh model 470 MB / network.

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use hakomemory_server::router;
use hakomemory_server::state::AppState;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

fn temp_root() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "mb-api-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes: Bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(&bytes) }));
    (status, value)
}

fn test_app() -> (Router, PathBuf) {
    let dir = temp_root();
    let state = AppState::open_mock(&dir).unwrap();
    (router(state), dir)
}

async fn open_project(app: &Router, dir: &std::path::Path) -> String {
    let (status, body) = call(
        app,
        "POST",
        "/v1/projects/open",
        json!({ "path": dir.to_string_lossy() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pid = body["project_id"].as_str().unwrap().to_string();
    assert_eq!(pid.len(), 12, "project id harus 12 hex: {pid}");
    pid
}

#[tokio::test]
async fn healthz_and_project_status() {
    let (app, dir) = test_app();
    let (status, body) = call(&app, "GET", "/healthz", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    let pid = open_project(&app, &dir).await;
    let (status, body) = call(
        &app,
        "GET",
        &format!("/v1/index/status?project_id={pid}"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("vector_complete").is_some());
    assert!(body.get("quiescent").is_some());
}

#[tokio::test]
async fn memory_write_search_forget() {
    let (app, dir) = test_app();
    let pid = open_project(&app, &dir).await;

    let (status, written) = call(
        &app,
        "POST",
        "/v1/memory/write",
        json!({
            "project_id": pid,
            "kind": "semantic",
            "text": "selalu verifikasi kebenaran data sebelum mempercayai benchmark race 209",
            "metadata": { "source": "integration-test" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    let id = written["id"].as_str().unwrap().to_string();

    let mut found = false;
    for _ in 0..20 {
        let (_, hits) = call(
            &app,
            "POST",
            "/v1/memory/search",
            json!({ "project_id": pid, "kind": "semantic", "query": "verifikasi kebenaran data", "k": 5 }),
        )
        .await;
        if hits["hits"].as_array().is_some_and(|h| !h.is_empty()) {
            found = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(found, "memory yang ditulis harus bisa dicari");

    let (status, _) = call(
        &app,
        "POST",
        "/v1/memory/forget",
        json!({ "project_id": pid, "kind": "semantic", "id": id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let mut gone = false;
    for _ in 0..20 {
        let (_, hits) = call(
            &app,
            "POST",
            "/v1/memory/search",
            json!({ "project_id": pid, "kind": "semantic", "query": "verifikasi kebenaran data", "k": 5 }),
        )
        .await;
        let empty = hits["hits"].as_array().is_some_and(|h| h.is_empty());
        if empty { gone = true; break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(gone, "memory harus hilang setelah forget");
}

#[tokio::test]
async fn index_symbols_graph_retrieve_and_remove() {
    let (app, dir) = test_app();
    let pid = open_project(&app, &dir).await;
    let file = dir.join("lib.rs");
    std::fs::write(
        &file,
        "pub fn helper() -> u32 { 42 }\npub fn caller() -> u32 { helper() }\n",
    )
    .unwrap();

    let (status, report) = call(
        &app,
        "POST",
        "/v1/index/scan",
        json!({ "project_id": pid, "path": file.to_string_lossy() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(report["symbols"].as_u64().unwrap() >= 2);

    let (status, syms) = call(
        &app,
        "POST",
        "/v1/symbols/lookup",
        json!({ "project_id": pid, "name": "caller" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sid = syms["hits"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(syms["hits"][0]["path"], "lib.rs");

    let (status, graph) = call(
        &app,
        "POST",
        "/v1/graph/context",
        json!({ "project_id": pid, "seed": format!("symbols/{sid}"), "hops": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{graph}");
    let nodes = graph["nodes"].as_array().unwrap();
    assert!(!nodes.is_empty());
    assert!(graph["edges"].as_array().is_some_and(|e| !e.is_empty()));
    // v0.21.0 proyeksi: emb tidak pernah ikut, tapi field edge-doc tetap ada
    assert!(
        nodes.iter().all(|n| n["doc"].get("emb").is_none()),
        "emb bocor ke respons graph"
    );
    assert!(
        nodes.iter().any(|n| n["collection"] == "edges"
            && n["doc"].get("callee").is_some()
            && n["doc"].get("to_path").is_some()),
        "field edge-doc hilang dari proyeksi: {graph}"
    );

    let (status, retrieved) = call(
        &app,
        "POST",
        "/v1/retrieve",
        json!({ "project_id": pid, "query": "caller helper", "k": 3 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retrieved}");
    assert!(retrieved["hits"].as_array().is_some_and(|h| !h.is_empty()));

    let (status, removed) = call(
        &app,
        "POST",
        "/v1/index/remove",
        json!({ "project_id": pid, "path": "lib.rs" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    let (_, syms) = call(
        &app,
        "POST",
        "/v1/symbols/lookup",
        json!({ "project_id": pid, "name": "caller" }),
    )
    .await;
    assert!(syms["hits"].as_array().is_some_and(|h| h.is_empty()));
}

#[tokio::test]
async fn full_rescan_removes_files_missing_from_disk() {
    let (app, dir) = test_app();
    let pid = open_project(&app, &dir).await;
    let old = dir.join("moved_file.rs");
    std::fs::write(&old, "pub fn moved_marker() -> u32 { 7 }\n").unwrap();
    call(
        &app,
        "POST",
        "/v1/index/scan",
        json!({ "project_id": pid, "path": dir.to_string_lossy() }),
    )
    .await;

    // pindahkan file (mirip git mv saat server tidak berjalan), lalu rescan root
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::rename(&old, sub.join("moved_file.rs")).unwrap();
    let (_, report) = call(
        &app,
        "POST",
        "/v1/index/scan",
        json!({ "project_id": pid, "path": dir.to_string_lossy() }),
    )
    .await;
    assert!(report["removed"].as_u64().unwrap() > 0, "{report}");

    let (_, syms) = call(
        &app,
        "POST",
        "/v1/symbols/lookup",
        json!({ "project_id": pid, "name": "moved_marker", "k": 10 }),
    )
    .await;
    let paths: Vec<&str> = syms["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap())
        .collect();
    assert!(!paths.contains(&"moved_file.rs"), "path lama harus hilang: {paths:?}");
    assert!(paths.contains(&"sub/moved_file.rs"), "path baru harus ada: {paths:?}");
}

#[tokio::test]
async fn bad_requests_are_rejected() {
    let (app, dir) = test_app();
    let pid = open_project(&app, &dir).await;

    // kind memory tidak dikenal â†’ 400 (bukan 500)
    let (status, _) = call(
        &app,
        "POST",
        "/v1/memory/write",
        json!({ "project_id": pid, "kind": "bogus", "text": "x" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // path/alias hilang â†’ 400 (keduanya opsional di schema, tapi salah satu wajib)
    let (status, _) = call(
        &app,
        "POST",
        "/v1/projects/open",
        json!({ "nope": true }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // index/status memakai query string, bukan body JSON
    let (status, _) = call(&app, "GET", "/v1/index/status", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn memory_write_can_wait_for_index() {
    let (app, dir) = test_app();
    let pid = open_project(&app, &dir).await;
    let (status, written) = call(
        &app,
        "POST",
        "/v1/memory/write",
        json!({
            "project_id": pid,
            "kind": "semantic",
            "text": "read after write marker",
            "wait_for_index": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    let (status, hits) = call(
        &app,
        "POST",
        "/v1/memory/search",
        json!({ "project_id": pid, "query": "read after write marker", "k": 5 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    assert!(!hits["hits"].as_array().unwrap().is_empty());
}
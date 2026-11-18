//! Test model-agnostik: spec dipersisten, mismatch ditolak, 768d bekerja.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use hakomemory_server::router;
use hakomemory_server::state::AppState;
use hakomemory_server::store::{read_embed_spec, EmbeddingSpec, ProjectStore};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

fn temp_root() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "mb-spec-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    (status, value)
}

#[test]
fn spec_tersimpan_dan_mismatch_ditolak() {
    let dir = temp_root();
    let path = "/tmp/project-agnostik";
    let pid = {
        let store = ProjectStore::new(dir.clone(), EmbeddingSpec::local(384));
        let (pid, db) = store.open_project(path).unwrap();
        let spec = read_embed_spec(&db).unwrap().expect("spec belum tersimpan");
        assert_eq!(spec.dim, 384);
        assert_eq!(spec.metric, "Cosine");
        assert_eq!(spec.provider, "local");
        drop(db);
        drop(store); // lepas lock hako.lock
        pid
    };
    // Konfigurasi 768d membuka data lama 384d → harus error jelas
    let store2 = ProjectStore::new(dir, EmbeddingSpec::local(768));
    let err = match store2.get(&pid) {
        Ok(_) => panic!("mismatch dimensi harus ditolak"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("384d") && err.contains("768d"), "error harus menyebut dimensi: {err}");
    assert!(err.contains("migrasi"), "error harus menyarankan migrasi: {err}");
}

#[tokio::test]
async fn project_768d_bekerja_end_to_end() {
    let dir = temp_root();
    let root = temp_root();
    let app = router(AppState::open_mock_with_dim(&dir, 768).unwrap());
    let (_, opened) = call(
        &app,
        "POST",
        "/v1/projects/open",
        json!({ "path": root.to_string_lossy() }),
    )
    .await;
    let pid = opened["project_id"].as_str().unwrap();

    std::fs::write(root.join("lib.rs"), "pub fn probe() -> u32 { 7 }\n").unwrap();
    let (status, report) = call(
        &app,
        "POST",
        "/v1/index/scan",
        json!({ "project_id": pid, "path": root.to_string_lossy() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let (status, hits) = call(
        &app,
        "POST",
        "/v1/retrieve",
        json!({ "project_id": pid, "query": "probe", "k": 3 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    assert!(!hits["hits"].as_array().unwrap().is_empty(), "vektor 768d harus bisa di-query");

    let (status, written) = call(
        &app,
        "POST",
        "/v1/memory/write",
        json!({ "project_id": pid, "kind": "semantic", "text": "memori 768 dimensi" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    let (status, found) = call(
        &app,
        "POST",
        "/v1/memory/search",
        json!({ "project_id": pid, "query": "memori 768 dimensi", "k": 3 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!found["hits"].as_array().unwrap().is_empty());
}

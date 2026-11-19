//! Integration tests for generic document ingestion (#6).

use axum::body::Body;
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
    let path = std::env::temp_dir().join(format!(
        "mb-docs-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
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
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    (status, value)
}

async fn open_project(app: &Router, root: &std::path::Path) -> String {
    let (_, opened) = call(
        app,
        "POST",
        "/v1/projects/open",
        json!({ "path": root.to_string_lossy() }),
    )
    .await;
    opened["project_id"].as_str().unwrap().to_string()
}

const MARKDOWN: &str = "---\ntitle: RAG Internal\ntags: [\"ai\", \"rag\"]\n---\n# Overview\nHybrid search combines FTS and vectors.\n\n## Details\nFilters are pushed down before ranking.\n";

#[tokio::test]
async fn document_upsert_is_idempotent_and_carries_citations() {
    let dir = temp_root();
    let root = temp_root();
    let app = router(AppState::open_mock_with_dim(&dir, 384).unwrap());
    let pid = open_project(&app, &root).await;

    let (status, first) = call(
        &app,
        "POST",
        "/v1/documents/upsert",
        json!({
            "project_id": pid,
            "source_uri": "wiki/rag.md",
            "content": MARKDOWN,
            "collection": "wiki",
            "strategy": "header"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["title"], "RAG Internal");
    assert_eq!(first["chunks"], 2, "frontmatter + two header sections: {first}");
    assert_eq!(first["reindexed"], true);

    let (status, again) = call(
        &app,
        "POST",
        "/v1/documents/upsert",
        json!({
            "project_id": pid,
            "source_uri": "wiki/rag.md",
            "content": MARKDOWN,
            "collection": "wiki",
            "strategy": "header"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["chunks"], 2, "reindex must not duplicate chunks: {again}");
    assert_eq!(again["reindexed"], false);

    let uri = format!(
        "/v1/documents/get?project_id={}&source_uri=wiki%2Frag.md",
        pid
    );
    let (status, got) = call(&app, "GET", &uri, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["metadata"]["tags"][0], "ai");
    let chunks = got["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0]["citation"]["source_uri"], "wiki/rag.md");
    assert_eq!(chunks[0]["citation"]["heading"], "Overview");
    assert!(chunks[1]["citation"]["end_line"].as_u64().unwrap() > 0);

    let list_uri = format!("/v1/documents/list?project_id={}&collection=wiki", pid);
    let (status, listed) = call(&app, "GET", &list_uri, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["documents"].as_array().unwrap().len(), 1);

    let (status, deleted) = call(
        &app,
        "POST",
        "/v1/documents/delete",
        json!({ "project_id": pid, "source_uri": "wiki/rag.md" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let (status, _) = call(&app, "GET", &uri, json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn directory_scan_uses_manifest_and_migration_is_idempotent() {
    let dir = temp_root();
    let root = temp_root();
    std::fs::write(root.join("a.md"), "# A\nfirst\n").unwrap();
    std::fs::write(root.join("b.md"), "# B\nsecond\n").unwrap();
    let app = router(AppState::open_mock_with_dim(&dir, 384).unwrap());
    let pid = open_project(&app, &root).await;

    let body = json!({
        "project_id": pid,
        "path": root.to_string_lossy(),
        "collection": "notes",
        "strategy": "header"
    });
    let (status, first) = call(&app, "POST", "/v1/documents/scan", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["indexed"], 2, "{first}");

    let (status, second) = call(&app, "POST", "/v1/documents/scan", body).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["unchanged"], 2, "{second}");
    assert_eq!(second["indexed"], 0);

    let (status, report) = call(
        &app,
        "POST",
        "/v1/admin/migrate",
        json!({ "project_id": pid, "dry_run": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["schema_version_after"], 2);
    assert_eq!(report["backfilled"], 0);
}

#[tokio::test]
async fn document_search_filters_before_budget_and_returns_citations() {
    let dir = temp_root();
    let root = temp_root();
    let app = router(AppState::open_mock_with_dim(&dir, 384).unwrap());
    let pid = open_project(&app, &root).await;

    for (uri, collection, text) in [
        ("wiki/rag.md", "wiki", "# RAG\nHybrid search fuses FTS and vectors with filters.\n"),
        ("notes/other.md", "other", "# Other\nHybrid search notes from another collection.\n"),
    ] {
        let (status, upserted) = call(
            &app,
            "POST",
            "/v1/documents/upsert",
            json!({
                "project_id": pid,
                "source_uri": uri,
                "content": text,
                "collection": collection,
                "strategy": "header"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{upserted}");
    }

    let (status, result) = call(
        &app,
        "POST",
        "/v1/documents/search",
        json!({
            "project_id": pid,
            "query": "hybrid search",
            "mode": "hybrid",
            "filters": [{"field": "collection", "op": "eq", "value": "wiki"}],
            "k": 5,
            "budget_tokens": 200,
            "citations": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let hits = result["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "expected filtered hybrid hits: {result}");
    assert!(
        hits.iter().all(|hit| hit["citation"]["source_uri"] == "wiki/rag.md"),
        "filter must exclude the other collection: {result}"
    );
    assert!(hits.iter().all(|hit| !hit["citation"]["document_id"].as_str().unwrap().is_empty()));
    assert!(hits.iter().all(|hit| hit["citation"]["end_line"].as_u64().unwrap() > 0));

    let (status, tiny) = call(
        &app,
        "POST",
        "/v1/documents/search",
        json!({
            "project_id": pid,
            "query": "hybrid search",
            "mode": "vector",
            "k": 5,
            "budget_tokens": 4
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tiny}");
    assert!(tiny["used_tokens"].as_u64().unwrap() <= 4, "budget exceeded: {tiny}");
    assert_eq!(tiny["mode"], "vector");
}
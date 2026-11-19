//! Integration tests for generic graph analytics (#8).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use hakomemory_server::router;
use hakomemory_server::state::AppState;
use serde_json::{json, Value};
use std::path::PathBuf;
use tower::ServiceExt;

fn temp_root() -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "mb-graph-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value, String) {
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
    let text = String::from_utf8_lossy(&bytes).to_string();
    let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    (status, value, text)
}

#[tokio::test]
async fn graph_analytics_are_exposed_and_references_are_normalized() {
    let dir = temp_root();
    std::fs::write(
        dir.join("lib.rs"),
        "pub fn helper() -> u32 { 42 }\npub fn caller() -> u32 { helper() }\n",
    )
    .unwrap();
    let app = router(AppState::open_mock_with_dim(&dir, 384).unwrap());
    let (_, opened, _) = call(
        &app,
        "POST",
        "/v1/projects/open",
        json!({ "path": dir.to_string_lossy() }),
    )
    .await;
    let pid = opened["project_id"].as_str().unwrap().to_string();

    let (status, report, _) = call(
        &app,
        "POST",
        "/v1/index/scan",
        json!({ "project_id": pid, "path": dir.to_string_lossy() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(report["edges"].as_u64().unwrap() > 0, "fixture must have a call edge: {report}");

    let (_, syms, _) = call(
        &app,
        "POST",
        "/v1/symbols/lookup",
        json!({ "project_id": pid, "name": "caller" }),
    )
    .await;
    let seed = syms["hits"][0]["id"].as_str().unwrap().to_string();
    let options = json!({
        "project_id": pid,
        "collection": "symbols",
        "seed": format!("symbols/{seed}"),
        "via": "edges",
        "max_depth": 2,
        "limit": 50,
        "projection": ["name", "kind", "path"]
    });

    for (endpoint, field) in [
        ("rank", "ranks"),
        ("community", "communities"),
        ("components", "components"),
        ("degrees", "degrees"),
    ] {
        let mut body = options.clone();
        if endpoint == "rank" {
            body["damping"] = json!(0.85);
            body["top"] = json!(10);
        }
        let (status, result, text) = call(
            &app,
            "POST",
            &format!("/v1/graph/{endpoint}"),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{endpoint}: {text}");
        assert!(result.get(field).is_some(), "{endpoint} missing {field}: {text}");
        assert!(!text.contains("__ref__"), "{endpoint} leaked raw references: {text}");
    }

    for endpoint in ["subgraph", "export", "cycle", "topo"] {
        let (status, result, text) = call(
            &app,
            "POST",
            &format!("/v1/graph/{endpoint}"),
            options.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{endpoint}: {text}");
        assert!(!text.contains("__ref__"), "{endpoint} leaked raw references: {text}");
        if endpoint == "subgraph" || endpoint == "export" {
            assert!(result.get("entities").is_some(), "{endpoint}: {text}");
            assert!(result.get("relations").is_some(), "{endpoint}: {text}");
        }
    }

    let (status, helpers, _) = call(
        &app,
        "POST",
        "/v1/symbols/lookup",
        json!({ "project_id": pid, "name": "helper" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let target = helpers["hits"][0]["id"].as_str().unwrap();
    let mut path_body = options.clone();
    path_body["target"] = json!(format!("symbols/{target}"));
    let (status, result, text) = call(&app, "POST", "/v1/graph/path", path_body).await;
    assert_eq!(status, StatusCode::OK, "path: {text}");
    assert!(result.get("entities").is_some(), "path: {text}");
    assert!(!text.contains("__ref__"), "path leaked raw references: {text}");
}

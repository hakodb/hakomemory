//! Error-envelope and rate-limit response tests (#9).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hakomemory_server::auth::AuthConfig;
use hakomemory_server::router;
use hakomemory_server::state::AppState;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

fn temp_root() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "mb-errors-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Option<String>, Value) {
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
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    (status, retry_after, value)
}

#[tokio::test]
async fn bad_requests_carry_structured_error() {
    let dir = temp_root();
    let app = router(AppState::open_mock_with_auth(&dir, AuthConfig::Disabled).unwrap());
    let (status, _, body) = send(&app, "POST", "/v1/projects/open", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "E_INPUT");
    assert!(body["error"]["message"].as_str().is_some());
    assert!(!body["error"]["request_id"].as_str().unwrap().is_empty());

    let (status, _, body) = send(
        &app,
        "POST",
        "/v1/projects/open",
        json!({ "path": dir.to_string_lossy() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pid = body["project_id"].as_str().unwrap();
    let (status, _, body) = send(
        &app,
        "POST",
        "/v1/documents/delete",
        json!({ "project_id": pid, "source_uri": "missing.md" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "E_NOT_FOUND");
    assert!(body["error"]["request_id"].as_str().is_some());
}

#[tokio::test]
async fn rate_limit_response_has_retry_after() {
    let dir = temp_root();
    let app = router(
        AppState::open_mock_with_limits(&dir, AuthConfig::Disabled, 1).unwrap(),
    );
    let (first, _, _) = send(
        &app,
        "GET",
        "/v1/documents/list?project_id=unknown",
        json!({}),
    )
    .await;
    assert_ne!(first, StatusCode::TOO_MANY_REQUESTS);
    let (status, retry_after, body) = send(
        &app,
        "GET",
        "/v1/documents/list?project_id=unknown",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(retry_after.as_deref(), Some("60"));
    assert_eq!(body["error"]["code"], "E_RATE");
    assert!(body["error"]["request_id"].as_str().is_some());
}

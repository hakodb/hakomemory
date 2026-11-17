//! Integration test authN + otorisasi project/path (Fase 1–2).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use mb_server::auth::{mint_token, AuthConfig, Claims};
use mb_server::router;
use mb_server::state::AppState;
use serde_json::{json, Value};
use sha2::Digest;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

const SECRET: &[u8] = b"test-secret-jangan-dipakai-di-produksi";

fn temp_root() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "mb-auth-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn token(sub: &str, projects: &[&str], roots: &[String], admin: bool) -> String {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    mint_token(
        &Claims {
            sub: sub.into(),
            projects: projects.iter().map(|s| s.to_string()).collect(),
            roots: roots.to_vec(),
            exp,
            admin,
        },
        SECRET,
    )
    .unwrap()
}

async fn call(app: &Router, method: &str, path: &str, body: Value, tok: Option<&str>) -> StatusCode {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = tok {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    resp.status()
}

async fn call_json(
    app: &Router,
    method: &str,
    path: &str,
    body: Value,
    tok: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = tok {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    (status, value)
}

fn app_with_auth(dir: &PathBuf) -> Router {
    let state = AppState::open_mock_with_auth(
        dir,
        AuthConfig::Enabled {
            secret: SECRET.to_vec(),
        },
    )
    .unwrap();
    router(state)
}

#[tokio::test]
async fn tanpa_token_ditolak_kecuali_healthz() {
    let dir = temp_root();
    let app = app_with_auth(&dir);
    assert_eq!(
        call(&app, "GET", "/healthz", json!({}), None).await,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/projects/open",
            json!({ "path": dir.to_string_lossy() }),
            None
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn token_tidak_valid_ditolak() {
    let dir = temp_root();
    let app = app_with_auth(&dir);
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/projects/open",
            json!({ "path": dir.to_string_lossy() }),
            Some("mb1.bogus.bogus")
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn project_di_luar_izin_ditolak() {
    let dir = temp_root();
    let app = app_with_auth(&dir);
    let pid = "830d232f9c86"; // tidak ada di klaim token
    let tok = token("alice", &[], &[dir.to_string_lossy().to_string()], false);
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/retrieve",
            json!({ "project_id": pid, "query": "x", "k": 1 }),
            Some(&tok)
        )
        .await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn path_di_luar_root_ditolak_dan_path_di_dalam_diizinkan() {
    let dir = temp_root();
    let outside = temp_root();
    let app = app_with_auth(&dir);
    let pid = {
        let canonical = dir.to_string_lossy().to_string();
        use sha2::{Digest, Sha256};
        hex12(&Sha256::digest(canonical.as_bytes()))
    };
    let tok = token("alice", &[&pid], &[dir.to_string_lossy().to_string()], false);

    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/projects/open",
            json!({ "path": outside.to_string_lossy() }),
            Some(&tok)
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/projects/open",
            json!({ "path": dir.to_string_lossy() }),
            Some(&tok)
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn alias_terikat_user_dan_pid_diotorisasi_lewat_root() {
    let dir = temp_root();
    let canonical = std::fs::canonicalize(&dir).unwrap().to_string_lossy().to_string();
    let pid = hex12(&sha2::Sha256::digest(canonical.as_bytes()));
    let app = app_with_auth(&dir);

    // user A mendaftarkan alias "work" dengan path di root-nya
    let tok_a = token("alice", &[], &[canonical.clone()], false);
    let (status, body) = call_json(
        &app,
        "POST",
        "/v1/projects/open",
        json!({ "alias": "work", "path": canonical }),
        Some(&tok_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project_id"], pid);

    // token A hanya punya root (tanpa projects) → alias + otorisasi lewat by_pid
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/projects/open",
            json!({ "alias": "work" }),
            Some(&tok_a)
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/retrieve",
            json!({ "project_id": pid, "query": "x", "k": 1 }),
            Some(&tok_a)
        )
        .await,
        StatusCode::OK
    );

    // user B tidak bisa memakai alias milik A,也不能 menebak path di luar root-nya
    let other = temp_root();
    let tok_b = token("bob", &[], &[other.to_string_lossy().to_string()], false);
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/projects/open",
            json!({ "alias": "work" }),
            Some(&tok_b)
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/retrieve",
            json!({ "project_id": pid, "query": "x", "k": 1 }),
            Some(&tok_b)
        )
        .await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn memory_terisolasi_per_user() {
    let dir = temp_root();
    let canonical = std::fs::canonicalize(&dir).unwrap().to_string_lossy().to_string();
    let pid = hex12(&sha2::Sha256::digest(canonical.as_bytes()));
    let app = app_with_auth(&dir);
    let tok_a = token("alice", &[], &[canonical.clone()], false);
    let tok_b = token("bob", &[], &[canonical.clone()], false);

    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/projects/open",
            json!({ "path": canonical }),
            Some(&tok_a)
        )
        .await,
        StatusCode::OK
    );

    let (status, written) = call_json(
        &app,
        "POST",
        "/v1/memory/write",
        json!({
            "project_id": pid,
            "kind": "semantic",
            "text": "rahasia khusus alice: kunci部署 abc123",
            "agent_id": "agent-a",
            "session_id": "ses-1"
        }),
        Some(&tok_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    let id = written["id"].as_str().unwrap().to_string();

    let body = json!({ "project_id": pid, "kind": "semantic", "query": "rahasia alice kunci", "k": 5 });
    let (_, a) = call_json(&app, "POST", "/v1/memory/search", body.clone(), Some(&tok_a)).await;
    assert!(a["hits"].as_array().unwrap().len() >= 1, "alice harus melihat memori sendiri");
    let (_, b) = call_json(&app, "POST", "/v1/memory/search", body, Some(&tok_b)).await;
    assert!(b["hits"].as_array().unwrap().is_empty(), "bob tidak boleh melihat memori alice: {b}");

    // bob tidak bisa menghapus milik alice; alice bisa
    let fbody = json!({ "project_id": pid, "kind": "semantic", "id": id });
    assert_eq!(
        call(&app, "POST", "/v1/memory/forget", fbody.clone(), Some(&tok_b)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, "POST", "/v1/memory/forget", fbody, Some(&tok_a)).await,
        StatusCode::OK
    );
}

fn hex12(d: &[u8]) -> String {
    d.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

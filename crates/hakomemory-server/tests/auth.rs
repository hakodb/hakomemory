//! Integration test authN + otorisasi project/path (Fase 1â€“2).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use hakomemory_server::auth::{mint_token, AuthConfig, Claims};
use hakomemory_server::router;
use hakomemory_server::state::AppState;
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

fn token_scoped(
    sub: &str,
    projects: &[&str],
    roots: &[String],
    admin: bool,
    agents: &[&str],
    sessions: &[&str],
) -> String {
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
            jti: Some(format!("jti_{sub}_{}", {
                static N: AtomicU64 = AtomicU64::new(0);
                N.fetch_add(1, Ordering::SeqCst)
            })),
            agents: agents.iter().map(|s| s.to_string()).collect(),
            sessions: sessions.iter().map(|s| s.to_string()).collect(),
        },
        SECRET,
    )
    .unwrap()
}

fn token(sub: &str, projects: &[&str], roots: &[String], admin: bool) -> String {
    token_scoped(sub, projects, roots, admin, &[], &[])
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

async fn write_mem(
    app: &Router,
    pid: &str,
    tok: &str,
    agent: &str,
    text: &str,
) -> (StatusCode, Value) {
    call_json(
        app,
        "POST",
        "/v1/memory/write",
        json!({ "project_id": pid, "kind": "semantic", "text": text,
                "agent_id": agent, "session_id": "s1" }),
        Some(tok),
    )
    .await
}

fn app_with_auth(dir: &std::path::Path) -> Router {
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
    let tok_a = token("alice", &[], std::slice::from_ref(&canonical), false);
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

    // token A hanya punya root (tanpa projects) â†’ alias + otorisasi lewat by_pid
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

    // user B tidak bisa memakai alias milik A,ä¹Ÿä¸èƒ½ menebak path di luar root-nya
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
    let tok_a = token("alice", &[], std::slice::from_ref(&canonical), false);
    let tok_b = token("bob", &[], std::slice::from_ref(&canonical), false);

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
            "text": "rahasia khusus alice: kunciéƒ¨ç½² abc123",
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
    assert!(!a["hits"].as_array().unwrap().is_empty(), "alice harus melihat memori sendiri");
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

#[tokio::test]
async fn backfill_owner_hanya_admin_dan_menyembunyikan_legacy() {
    let dir = temp_root();
    let canonical = std::fs::canonicalize(&dir).unwrap().to_string_lossy().to_string();
    let pid = hex12(&sha2::Sha256::digest(canonical.as_bytes()));
    let state = AppState::open_mock_with_auth(
        &dir,
        AuthConfig::Enabled { secret: SECRET.to_vec() },
    )
    .unwrap();
    let app = router(state.clone());
    let alice = token("alice", &[], std::slice::from_ref(&canonical), false);
    let bob = token("bob", &[], std::slice::from_ref(&canonical), false);
    let admin = token("admin", &[], &[], true);

    assert_eq!(
        call(
            &app, "POST", "/v1/projects/open",
            json!({ "path": canonical }), Some(&alice)
        ).await,
        StatusCode::OK
    );

    // doc legacy: tanpa user_id, disisipkan langsung (bukan lewat API)
    {
        use hakodb::document::hako_doc::HakoDoc;
        use hakodb::document::value::Value;
        let db = state.store.get(&pid).unwrap();
        let mut doc = HakoDoc::default();
        doc.insert("text", Value::String("rahasia legacy tanpa owner".into()));
        doc.insert("ts", Value::Int(1));
        db.put("mem_semantic", "legacy-owner-1", &doc).unwrap();
    }

    let bf = json!({ "project_id": pid, "user_id": "alice", "kind": "semantic" });
    assert_eq!(
        call(&app, "POST", "/v1/admin/backfill", bf.clone(), Some(&bob)).await,
        StatusCode::FORBIDDEN
    );
    let (status, out) = call_json(&app, "POST", "/v1/admin/backfill", bf, Some(&admin)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(out["stamped"].as_u64().unwrap() >= 1);

    let body = json!({ "project_id": pid, "kind": "semantic", "query": "rahasia legacy", "k": 5 });
    let (_, a) = call_json(&app, "POST", "/v1/memory/search", body.clone(), Some(&alice)).await;
    assert!(!a["hits"].as_array().unwrap().is_empty(), "alice harus melihat after backfill: {a}");
    let (_, b) = call_json(&app, "POST", "/v1/memory/search", body, Some(&bob)).await;
    assert!(b["hits"].as_array().unwrap().is_empty(), "bob tidak boleh: {b}");
}

#[tokio::test]
async fn revokasi_token_berlaku() {
    let dir = temp_root();
    let canonical = std::fs::canonicalize(&dir).unwrap().to_string_lossy().to_string();
    let pid = hex12(&sha2::Sha256::digest(canonical.as_bytes()));
    let state = AppState::open_mock_with_auth(
        &dir,
        AuthConfig::Enabled { secret: SECRET.to_vec() },
    )
    .unwrap();
    let app = router(state.clone());
    let admin = token("admin", &[], &[], true);
    let alice = token("alice", &[&pid], std::slice::from_ref(&canonical), false);

    assert_eq!(
        call(
            &app, "POST", "/v1/projects/open",
            json!({ "path": canonical }), Some(&alice)
        ).await,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app, "POST", "/v1/admin/revoke",
            json!({ "token": alice }), Some(&admin)
        ).await,
        StatusCode::OK
    );
    // token lama sudah tidak bisa dipakai
    assert_eq!(
        call(
            &app, "POST", "/v1/retrieve",
            json!({ "project_id": pid, "query": "x", "k": 1 }), Some(&alice)
        ).await,
        StatusCode::UNAUTHORIZED
    );
    // admin tidak ikut tercabut
    assert_eq!(
        call(
            &app, "POST", "/v1/projects/open",
            json!({ "path": dir.to_string_lossy() }), Some(&admin)
        ).await,
        StatusCode::OK
    );
    // non-admin tidak bisa mencabut
    let bob = token("bob", &[], &[], false);
    assert_eq!(
        call(
            &app, "POST", "/v1/admin/revoke",
            json!({ "jti": "jti_bob_0" }), Some(&bob)
        ).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn rate_limit_per_user() {
    let dir = temp_root();
    let canonical = std::fs::canonicalize(&dir).unwrap().to_string_lossy().to_string();
    let pid = hex12(&sha2::Sha256::digest(canonical.as_bytes()));
    let state = AppState::open_mock_with_limits(
        &dir,
        AuthConfig::Enabled { secret: SECRET.to_vec() },
        3,
    )
    .unwrap();
    let app = router(state);
    let tok = token("alice", &[&pid], std::slice::from_ref(&canonical), false);
    let body = json!({ "project_id": pid, "query": "x", "k": 1 });
    assert_eq!(
        call(
            &app, "POST", "/v1/projects/open",
            json!({ "path": canonical }), Some(&tok)
        ).await,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "POST", "/v1/retrieve", body.clone(), Some(&tok)).await,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "POST", "/v1/retrieve", body.clone(), Some(&tok)).await,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "POST", "/v1/retrieve", body, Some(&tok)).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // healthz tidak kena rate limit
    assert_eq!(call(&app, "GET", "/healthz", json!({}), None).await, StatusCode::OK);
}

#[tokio::test]
async fn filtered_search_menjamin_full_k() {
    let dir = temp_root();
    let canonical = std::fs::canonicalize(&dir).unwrap().to_string_lossy().to_string();
    let pid = hex12(&sha2::Sha256::digest(canonical.as_bytes()));
    let app = app_with_auth(&dir);
    let alice = token("alice", &[], std::slice::from_ref(&canonical), false);
    let bob = token("bob", &[], std::slice::from_ref(&canonical), false);
    assert_eq!(
        call(
            &app, "POST", "/v1/projects/open",
            json!({ "path": canonical }), Some(&alice)
        ).await,
        StatusCode::OK
    );
    // bob lebih banyak menulis, tapi alice harus tetap dapat k penuh
    for i in 0..5 {
        let (status, _) = call_json(
            &app, "POST", "/v1/memory/write",
            json!({ "project_id": pid, "kind": "semantic",
                    "text": format!("bob topik bersama dokumen nomor {i}") }),
            Some(&bob),
        ).await;
        assert_eq!(status, StatusCode::OK);
    }
    for i in 0..3 {
        let (status, _) = call_json(
            &app, "POST", "/v1/memory/write",
            json!({ "project_id": pid, "kind": "semantic",
                    "text": format!("alice topik bersama dokumen nomor {i}") }),
            Some(&alice),
        ).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, out) = call_json(
        &app, "POST", "/v1/memory/search",
        json!({ "project_id": pid, "kind": "semantic", "query": "topik bersama", "k": 3 }),
        Some(&alice),
    ).await;
    assert_eq!(status, StatusCode::OK);
    let hits = out["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3, "harus dapat 3 hit penuh: {out}");
    assert!(
        hits.iter().all(|h| h["text"].as_str().unwrap().starts_with("alice")),
        "tidak boleh ada dokumen bob: {out}"
    );
}

#[tokio::test]
async fn memory_scope_agent_dan_session() {
    let dir = temp_root();
    let canonical = std::fs::canonicalize(&dir).unwrap().to_string_lossy().to_string();
    let pid = hex12(&sha2::Sha256::digest(canonical.as_bytes()));
    let app = app_with_auth(&dir);
    let alice = token_scoped("alice", &[&pid], std::slice::from_ref(&canonical), false, &["agent-a"], &[]);
    let bob = token_scoped("bob", &[&pid], std::slice::from_ref(&canonical), false, &["agent-b"], &[]);
    let admin = token("admin", &[], &[], true);
    assert_eq!(
        call(
            &app, "POST", "/v1/projects/open",
            json!({ "path": canonical }), Some(&alice)
        ).await,
        StatusCode::OK
    );

    let (sa, wa) = write_mem(&app, &pid, &alice, "agent-a", "memori milik agent a").await;
    assert_eq!(sa, StatusCode::OK, "{wa}");
    let id_a = wa["id"].as_str().unwrap().to_string();
    let (sb, wb) = write_mem(&app, &pid, &bob, "agent-b", "memori milik agent b").await;
    assert_eq!(sb, StatusCode::OK, "{wb}");
    let id_b = wb["id"].as_str().unwrap().to_string();

    // token scoped hanya melihat agent-nya sendiri bila agent_id tidak diset
    let (_, seen) = call_json(
        &app, "POST", "/v1/memory/search",
        json!({ "project_id": pid, "query": "memori milik agent", "k": 10 }),
        Some(&alice),
    ).await;
    let texts: Vec<&str> = seen["hits"].as_array().unwrap().iter()
        .map(|h| h["text"].as_str().unwrap()).collect();
    assert!(texts.contains(&"memori milik agent a"), "{seen}");
    assert!(!texts.contains(&"memori milik agent b"), "{seen}");

    // meminta agent lain ditolak
    assert_eq!(
        call(
            &app, "POST", "/v1/memory/search",
            json!({ "project_id": pid, "query": "x", "agent_id": "agent-b" }),
            Some(&alice),
        ).await,
        StatusCode::FORBIDDEN
    );

    // menulis sebagai agent lain ditolak
    let (s_ilegal, _) = write_mem(&app, &pid, &alice, "agent-b", "tulis ilegal").await;
    assert_eq!(s_ilegal, StatusCode::FORBIDDEN);

    // forget lintas agent ditolak; forget milik sendiri sukses; admin melihat semua
    assert_eq!(
        call(
            &app, "POST", "/v1/memory/forget",
            json!({ "project_id": pid, "kind": "semantic", "id": id_b }),
            Some(&alice),
        ).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app, "POST", "/v1/memory/forget",
            json!({ "project_id": pid, "kind": "semantic", "id": id_a, "agent_id": "agent-a" }),
            Some(&alice),
        ).await,
        StatusCode::OK
    );
    let (_, all) = call_json(
        &app, "POST", "/v1/memory/search",
        json!({ "project_id": pid, "query": "memori milik agent", "k": 10 }),
        Some(&admin),
    ).await;
    assert!(all["hits"].as_array().unwrap().iter().any(|h| h["text"] == "memori milik agent b"));
}

fn hex12(d: &[u8]) -> String {
    d.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn index_scan_path_di_luar_root_ditolak() {
    let dir = temp_root();      // server data dir
    let root = temp_root();     // project root allowed by the token
    let outside = temp_root();
    std::fs::write(root.join("app.rs"), "pub fn ok() {}\n").unwrap();
    std::fs::write(outside.join("secret.rs"), "pub fn secret() {}\n").unwrap();
    let app = app_with_auth(&dir);
    let canon = std::fs::canonicalize(&root).unwrap();
    let pid = hex12(&sha2::Sha256::digest(canon.to_string_lossy().as_bytes()));
    let tok = token("alice", &[&pid], &[root.to_string_lossy().to_string()], false);
    assert_eq!(
        call(&app, "POST", "/v1/projects/open", json!({ "path": root.to_string_lossy() }), Some(&tok)).await,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/index/scan",
            json!({ "project_id": pid, "path": outside.to_string_lossy() }),
            Some(&tok),
        )
        .await,
        StatusCode::FORBIDDEN,
        "path di luar roots tidak boleh diindeks"
    );
    let (inside_status, inside_body) = call_json(
        &app,
        "POST",
        "/v1/index/scan",
        json!({ "project_id": pid, "path": root.to_string_lossy() }),
        Some(&tok),
    )
    .await;
    assert_eq!(
        inside_status,
        StatusCode::OK,
        "path di dalam roots tetap bisa diindeks: {inside_body}"
    );
}
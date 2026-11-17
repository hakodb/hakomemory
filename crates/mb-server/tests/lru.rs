//! Test LRU ProjectStore: eviksi, reopen data, dan pengaman Arc aktif.

use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use mb_server::store::ProjectStore;
use std::path::PathBuf;
use std::time::Duration;

fn temp_root() -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    use std::sync::atomic::Ordering;
    let p = std::env::temp_dir().join(format!(
        "mb-lru-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn lru_evicts_then_reopens_without_data_loss() {
    let dir = temp_root();
    let store = ProjectStore::with_limits(dir, 1, Duration::ZERO);
    let p1 = temp_root().join("proj-satu").to_string_lossy().to_string();
    let p2 = temp_root().join("proj-dua").to_string_lossy().to_string();

    let (pid1, db1) = store.open_project(&p1).unwrap();
    let mut doc = HakoDoc::default();
    doc.insert("text", Value::String("data yang harus selamat".into()));
    db1.put("mem_semantic", "keep-me", &doc).unwrap();
    let _ = db1.await_quiescent(Duration::from_secs(5));
    drop(db1); // hanya map yang memegang Arc

    let (pid2, db2) = store.open_project(&p2).unwrap();
    assert_ne!(pid1, pid2);
    assert_eq!(store.active_count(), 1, "capacity 1 harus men-eviksi pid1");
    assert!(!store.active_pids().contains(&pid1));
    assert!(store.active_pids().contains(&pid2));
    drop(db2);

    // buka ulang: data lama masih ada
    let db1b = store.get(&pid1).unwrap();
    assert_eq!(store.active_count(), 1, "pid2 ter-eviksi saat pid1 dibuka lagi");
    let got = db1b.get("mem_semantic", "keep-me").unwrap().expect("doc hilang");
    assert_eq!(got.get("text").unwrap(), &Value::String("data yang harus selamat".into()));
}

#[test]
fn lru_tidak_mengeviksi_project_yang_masih_dipakai() {
    let dir = temp_root();
    let store = ProjectStore::with_limits(dir, 1, Duration::ZERO);
    let p1 = temp_root().join("a").to_string_lossy().to_string();
    let p2 = temp_root().join("b").to_string_lossy().to_string();
    let p3 = temp_root().join("c").to_string_lossy().to_string();

    let (pid1, db1) = store.open_project(&p1).unwrap();
    // clone Arc → simulasi request yang masih in-flight
    let _inflight = db1.clone();
    let (pid2, db2) = store.open_project(&p2).unwrap();
    assert!(
        store.active_pids().contains(&pid1),
        "pid1 tidak boleh di-eviksi saat masih ada Arc luar"
    );
    assert!(store.active_pids().contains(&pid2));
    drop(_inflight);
    drop(db1);
    drop(db2);

    let (_pid3, _db3) = store.open_project(&p3).unwrap();
    assert!(
        !store.active_pids().contains(&pid1),
        "setelah Arc dilepas, pid1 boleh di-eviksi"
    );
}

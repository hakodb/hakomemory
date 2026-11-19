//! Acceptance test for #5: a separate Rust consumer can use the engine
//! without the HTTP server and without any `MEMORYBOX_*` env var.

use hakomemory_core::embed::SharedEmbedder;
use hakomemory_core::store::DEFAULT_DIM;
use hakomemory_core::{Engine, EngineConfig};
use std::sync::Arc;

fn temp_dir() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "hakomemory-core-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn engine_works_without_server_or_env() {
    // Ensure ambient env cannot accidentally make this pass.
    for key in [
        "MEMORYBOX_URL",
        "MEMORYBOX_EMBED_URL",
        "MEMORYBOX_AUTH_SECRET",
        "MEMORYBOX_MODEL",
    ] {
        std::env::remove_var(key);
    }

    let data_dir = temp_dir();
    let root = temp_dir();
    std::fs::write(root.join("lib.rs"), "pub fn probe() -> u32 { 7 }\n").unwrap();

    let embedder = Arc::new(SharedEmbedder::mock(DEFAULT_DIM));
    let engine = Engine::open(EngineConfig::new(&data_dir, embedder)).unwrap();
    let project = engine.project(root.to_string_lossy().as_ref()).unwrap();
    let report = project.scan(&root).unwrap();
    assert!(report.chunks > 0, "scan should produce chunks: {report:?}");

    let hits = project.retrieve("probe", 3, 60).unwrap();
    assert!(!hits.is_empty(), "hybrid retrieval should find the indexed file");
    assert_eq!(hits[0].path, "lib.rs");

    // Project ids survive reattachment (same wrap-around lifetime contract).
    let attached = engine.project_by_id(project.pid()).unwrap();
    assert_eq!(attached.pid(), project.pid());
}

//! POC fase 1: validasi hakodb v0.20.6 wave-1 API langsung dari Rust.
//!
//! Membuktikan: hybrid_search (RRF FTS+vector), find_near_scored, find_paths
//! (BFS), subgraph_json, graph_pagerank, graph_communities, dan persist-HNSW
//! (buka-tutup-buka, graph siap tanpa rebuild panjang).
//!
//! Embedding di POC ini pseudo-deterministik 8-dim (hash kata) — embedder
//! MiniLM lokal adalah langkah POC berikutnya.

use anyhow::Result;
use hakodb::config::{DurabilityMode, HakoConfig};
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::Hako;
use hakodb::index::traverse::{TraverseDir, TraverseSpec};
use hakodb::index::vector::{encode_f32s, Metric};

const DIM: usize = 8;

/// Pseudo-embedding deterministik: hash kata ke dimensi (POC saja,
/// digantikan MiniLM di langkah berikut).
fn pseudo_embed(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    for w in text.split_whitespace() {
        let h = fxhash_word(w);
        v[(h % DIM as u64) as usize] += 1.0;
    }
    // normalisasi L2
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

fn fxhash_word(w: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in w.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn emb_val(v: &[f32]) -> Value {
    Value::Binary(encode_f32s(v))
}

fn open_db(dir: &std::path::Path) -> Result<Hako> {
    let cfg = HakoConfig {
        durability_mode: DurabilityMode::Manual,
        ..Default::default()
    };
    Ok(Hako::open(dir, cfg)?)
}

fn wait(db: &Hako) {
    assert!(db.await_quiescent(std::time::Duration::from_secs(120)));
}

fn chunk_doc(path: &str, content: &str) -> HakoDoc {
    let mut d = HakoDoc::default();
    d.insert("path", Value::String(path.into()));
    d.insert("content", Value::String(content.into()));
    d.insert("emb", emb_val(&pseudo_embed(content)));
    d
}

fn symbol_doc(name: &str, kind: &str, refs: &[(&str, &str)]) -> HakoDoc {
    let mut d = HakoDoc::default();
    d.insert("name", Value::String(name.into()));
    d.insert("kind", Value::String(kind.into()));
    for &(col, id) in refs {
        let key = format!("ref_{col}");
        d.insert(&key, Value::Reference { collection: col.into(), doc_id: id.into() });
    }
    d
}

fn edge_doc(from: (&str, &str), to: (&str, &str), kind: &str, w: f64) -> HakoDoc {
    let mut d = HakoDoc::default();
    d.insert("_from", Value::Reference { collection: from.0.into(), doc_id: from.1.into() });
    d.insert("_to", Value::Reference { collection: to.0.into(), doc_id: to.1.into() });
    d.insert("kind", Value::String(kind.into()));
    d.insert("w", Value::Float(w));
    d
}

fn spec(dir: TraverseDir, depth: usize, limit: usize) -> TraverseSpec {
    TraverseSpec {
        direction: dir,
        edge_field: None,
        max_depth: depth,
        limit,
        via_edge: None,
        weight_field: None,
        node_match: None,
        edge_type: None,
        edge_match: None,
        include_path: false,
        include_via: false,
    }
}

/// Walk antar-simbol melalui koleksi edge `edges` (kontrak edge index).
fn spec_v(dir: TraverseDir, depth: usize, limit: usize) -> TraverseSpec {
    TraverseSpec {
        via_edge: Some("edges".into()),
        ..spec(dir, depth, limit)
    }
}

fn main() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("hakomemory-poc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    println!("== 1. open + create indexes (SEBELUM tulis pertama) ==");
    let db = open_db(&dir)?;
    db.create_fts_index("chunks", "content")?;
    db.create_vector_index("chunks", "emb", DIM as u32, Metric::Cosine)?;
    db.create_traverse_index("symbols")?;
    db.create_edge_index("edges")?;

    println!("== 2. seed codebase mini ==");
    let chunks = [
        ("src/auth.rs", "login password session authenticate user token"),
        ("src/db.rs", "database query connection pool storage index"),
        ("src/api.rs", "http route handler request response endpoint"),
        ("src/cache.rs", "cache memory key value eviction ttl"),
        ("src/vector.rs", "embedding vector search nearest neighbor knn index"),
        ("src/graph.rs", "graph traversal bfs dijkstra edges path rank"),
    ];
    for (i, (path, content)) in chunks.iter().enumerate() {
        db.put("chunks", &format!("c{i}"), &chunk_doc(path, content))?;
    }
    // simbol: authenticate -> validate_password -> open_connection (chain) + loop ke auth
    db.put("symbols", "sym_auth", &symbol_doc("authenticate", "fn", &[("chunks", "c0")]))?;
    db.put("symbols", "sym_valpw", &symbol_doc("validate_password", "fn", &[("chunks", "c0")]))?;
    db.put("symbols", "sym_open", &symbol_doc("open_connection", "fn", &[("chunks", "c1")]))?;
    db.put("symbols", "sym_knn", &symbol_doc("knn_search", "fn", &[("chunks", "c4")]))?;
    // edges berbobot: authenticate -> validate_password (call=2), validate_password -> open_connection (import=1)
    db.put("edges", "e1", &edge_doc(("symbols", "sym_auth"), ("symbols", "sym_valpw"), "call", 2.0))?;
    db.put("edges", "e2", &edge_doc(("symbols", "sym_valpw"), ("symbols", "sym_open"), "import", 1.0))?;
    db.put("edges", "e3", &edge_doc(("symbols", "sym_open"), ("symbols", "sym_auth"), "import", 1.0))?;
    wait(&db);
    println!("   seeded {} chunks, 4 symbols, 3 edges", chunks.len());

    println!("== 3. hybrid_search (RRF: FTS + vector) ==");
    let qv = pseudo_embed("authenticate user password");
    let rows = db.hybrid_search("chunks", "content", "authenticate user password", "emb", &qv, 3, 60)?;
    for (id, score, doc) in &rows {
        let path = match doc.get("path") { Some(Value::String(s)) => s.clone(), _ => "?".into() };
        println!("   {id} score={score:.4} path={path}");
    }
    assert_eq!(rows[0].0, "c0", "chunk auth harus paling atas");

    println!("== 4. find_near_scored (KNN scored) ==");
    let knn = db.find_near_scored("chunks", "emb", &qv, 2, None)?;
    for (id, dist, _) in &knn {
        println!("   {id} distance={dist:.4}");
    }

    println!("== 5. find_paths (BFS outgoing dari sym_auth, via edges) ==");
    let hits = db.find_paths("symbols", "sym_auth", &spec_v(TraverseDir::Outgoing, 8, 100))?;
    for h in &hits {
        println!("   depth={} cost={:.1} node={}/{}", h.depth, h.cost, h.collection, h.id);
    }
    assert!(hits.iter().any(|h| h.id == "sym_valpw"), "harus capai sym_valpw");
    assert!(hits.iter().any(|h| h.id == "sym_open"), "harus capai sym_open (2 hop)");

    println!("== 6. subgraph_json (seed sym_auth, 2 hop, via edges) ==");
    let sg = db.subgraph_json("symbols", "sym_auth", &spec_v(TraverseDir::Outgoing, 2, 50))?;
    println!("   {}", sg);

    println!("== 7. graph_pagerank (seed sym_auth, Dijkstra-weighted edges) ==");
    let pr_spec = TraverseSpec {
        weight_field: Some("w".into()),
        ..spec_v(TraverseDir::Outgoing, 8, 100)
    };
    let ranks = db.graph_pagerank("symbols", &[("symbols".into(), "sym_auth".into())], &pr_spec, 0.85, 5)?;
    for (node, score) in &ranks {
        println!("   {}/{} score={score:.5}", node.0, node.1);
    }

    println!("== 8. graph_communities (via edges) ==");
    let comms = db.graph_communities("symbols", &spec_v(TraverseDir::Outgoing, 8, 100))?;
    for (i, c) in comms.iter().enumerate() {
        let names: Vec<String> = c.iter().map(|n| n.1.clone()).collect();
        println!("   community {i}: {names:?}");
    }

    println!("== 9. persist-HNSW: close, reopen, tunggu recovery, langsung KNN ==");
    drop(db);
    let db2 = open_db(&dir)?;
    let t0 = std::time::Instant::now();
    assert!(db2.await_quiescent(std::time::Duration::from_secs(120)), "recovery harus selesai");
    let ready = db2.vector_index_complete("chunks", "emb");
    println!("   vector_index_complete={ready} ({} ms termasuk await_quiescent)", t0.elapsed().as_millis());
    let knn2 = db2.find_near_scored("chunks", "emb", &qv, 2, None)?;
    assert_eq!(knn2[0].0, knn[0].0, "hasil KNN harus identik setelah reopen");
    println!("   KNN pasca-reopen: {} (identik)", knn2[0].0);

    println!("\nPOC wave-1 API: SEMUA LOLOS");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

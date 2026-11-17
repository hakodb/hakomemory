//! POC fase 1 langkah 2: embedder MiniLM-L6 (384d, ONNX lokal via fastembed)
//! + hybrid_search end-to-end dengan embedding sungguhan.
//!
//! Model diunduh sekali ke cache (FASTEMBED_CACHE_DIR), lalu offline.

use anyhow::Result;
use hakodb::config::{DurabilityMode, HakoConfig};
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::Hako;
use hakodb::index::vector::Metric;
use mb_embed::Embedder;

const DIM: u32 = 384;

fn open_db(dir: &std::path::Path) -> Result<Hako> {
    let mut cfg = HakoConfig::default();
    cfg.durability_mode = DurabilityMode::Manual;
    Ok(Hako::open(dir, cfg)?)
}

fn chunk_doc(path: &str, content: &str, emb: &[f32]) -> HakoDoc {
    let mut d = HakoDoc::default();
    d.insert("path", Value::String(path.into()));
    d.insert("content", Value::String(content.into()));
    d.insert("emb", Value::Binary(hakodb::index::vector::encode_f32s(emb)));
    d
}

fn main() -> Result<()> {
    // PENTING: Hako::open harus inisialisasi pool rayon global PERTAMA
    // (hakodb memanggil build_global().unwrap() — panik jika pool sudah
    // diambil pihak lain, mis. candle/tokenizers). Di server nyata:
    // buka semua instance project sebelum embedder aktif.
    let dir = std::env::temp_dir().join(format!("mb-poc-emb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let cache = std::env::var("MEMORYBOX_MODEL_CACHE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| dir.join("model-cache"));
    std::fs::create_dir_all(&cache)?;

    println!("== 0. open HakoDB dulu (pemilik pool rayon global) ==");
    let db = open_db(&dir)?;

    println!("== 1. load MiniLM-L6 (384d, candle pure-Rust) — unduh sekali, lalu cache ==");
    let t0 = std::time::Instant::now();
    let mut model = Embedder::load(&mb_embed::model_from_env(), Some(&cache))?;
    println!("   model siap dalam {:.1} s", t0.elapsed().as_secs_f64());

    let chunks = [
        ("src/auth.rs", "fn authenticate(user, password) — verifikasi kredensial, buat session token"),
        ("src/db.rs", "struct DbPool — kumpulan koneksi database, query helper, transaksi"),
        ("src/api.rs", "router http — handler endpoint request response middleware"),
        ("src/cache.rs", "Cache LRU dengan ttl eviction, penyimpanan key-value di memori"),
        ("src/vector.rs", "embedding vector search — nearest neighbor KNN HNSW index"),
        ("src/graph.rs", "graph traversal bfs dijkstra — jalur terpendek antar node berbobot"),
    ];

    println!("== 2. embed corpus + ukur throughput ==");
    let t0 = std::time::Instant::now();
    let corpus: Vec<String> = chunks.iter().map(|(_, c)| c.to_string()).collect();
    let embs = model.embed(&corpus)?;
    let dt = t0.elapsed();
    println!("   {} dokumen dalam {:.1} ms ({:.0} doc/s), dim={}",
        embs.len(), dt.as_millis(),
        embs.len() as f64 / dt.as_secs_f64(), DIM);
    assert_eq!(embs[0].len(), DIM as usize);

    let dir = std::env::temp_dir().join(format!("mb-poc-emb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    println!("== 3. index ke HakoDB (vector 384d cosine + FTS) ==");
    db.create_fts_index("chunks", "content")?;
    db.create_vector_index("chunks", "emb", DIM, Metric::Cosine)?;
    for (i, ((path, content), emb)) in chunks.iter().zip(&embs).enumerate() {
        db.put("chunks", &format!("c{i}"), &chunk_doc(path, content, emb))?;
    }
    assert!(db.await_quiescent(std::time::Duration::from_secs(120)));

    println!("== 4. hybrid_search dengan query embedding asli ==");
    let queries = [
        "bagaimana cara autentikasi user dengan password?",
        "mencari tetangga terdekat dari vektor embedding",
        "path terpendek dalam graph berbobot",
    ];
    for q in queries {
        let t0 = std::time::Instant::now();
        let qv = &model.embed_one(q)?;
        let emb_ms = t0.elapsed().as_millis();
        let rows = db.hybrid_search("chunks", "content", q, "emb", qv, 3, 60)?;
        println!("   q: {q} (embed {emb_ms} ms)");
        for (id, score, doc) in &rows {
            let path = match doc.get("path") { Some(Value::String(s)) => s.clone(), _ => "?".into() };
            println!("      {id} score={score:.4} {path}");
        }
    }

    println!("\nPOC embedder: LOLOS");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

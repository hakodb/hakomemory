//! Bandingkan hasil embed batch (satu forward) vs embed_one (per teks).
use anyhow::Result;
use hakomemory_embed::Embedder;

fn main() -> Result<()> {
    let mut e = Embedder::load(&hakomemory_embed::model_from_env(), None)?;
    let texts = vec![
        "bagaimana watcher melakukan debounce sebelum scan".to_string(),
        "where is the project id computed from the canonical path".to_string(),
        "candle pure Rust embedding with mean pooling".to_string(),
    ];
    let single: Vec<Vec<f32>> = texts
        .iter()
        .map(|t| e.embed_one(t))
        .collect::<Result<Vec<_>>>()?;
    let batch = e.embed(&texts)?;
    assert_eq!(single.len(), batch.len());
    for (i, (a, b)) in single.iter().zip(batch.iter()).enumerate() {
        let cos: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        println!("text {i}: cos(single,batch)={cos:.6} dims={}/{}", a.len(), b.len());
        assert!(cos > 0.999, "batch berbeda dari sequential pada teks {i}");
    }
    println!("OK: batch embedding setara dengan sequential");
    Ok(())
}

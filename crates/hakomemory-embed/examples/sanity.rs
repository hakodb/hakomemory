//! Sanity model: kemiripan ID↔ID harus > ID↔EN dan > EN↔EN antar-topik.
use hakomemory_embed::Embedder;

fn main() -> anyhow::Result<()> {
    let mut emb = Embedder::load(&hakomemory_embed::model_from_env(), None)?;
    println!("model={} dim={}", hakomemory_embed::MINILM_MULTI, emb.dim);
    let texts = [
        ("id-a", "bagaimana debounce watcher menahan event sebelum scan"),
        ("id-b", "watcher menunggu debounce sebelum memindai file yang berubah"),
        ("en-a", "how does the watcher debounce events before scanning"),
        ("id-x", "resep nasi goreng dengan bumbu dan cabai"),
    ];
    let mut vecs = Vec::new();
    for (tag, t) in texts {
        let v = emb.embed_one(t)?;
        vecs.push((tag, v));
    }
    let cos = |a: &Vec<f32>, b: &Vec<f32>| {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        dot // sudah L2-normalized
    };
    let get = |tag: &str| vecs.iter().find(|(t, _)| *t == tag).unwrap().1.clone();
    let (ida, idb, ena, idx) = (get("id-a"), get("id-b"), get("en-a"), get("id-x"));
    println!("ID-ID   (sama topik)  : {:.4}", cos(&ida, &idb));
    println!("ID-EN   (sama topik)  : {:.4}", cos(&ida, &ena));
    println!("ID-EN   (beda topik)  : {:.4}", cos(&idx, &ena));
    println!("ID-ID   (beda topik)  : {:.4}", cos(&ida, &idx));
    Ok(())
}

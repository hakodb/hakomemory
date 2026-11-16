# memory-box — RAG Server untuk OpenCode

> Dokumen kerja. Status: fase 2 inti server berjalan (teruji end-to-end via HTTP).
> Terakhir diperbarui: 2026-11-16

## 0. Temuan POC (terverifikasi dari source v0.20.6 + eksekusi)

### 0.1 Hasil POC fase 1 (semua lolos)

- **Wave-1 API** (`crates/mb-poc/src/main.rs`): hybrid RRF, KNN scored,
  BFS via `via_edge`, subgraph_json, graph_pagerank, graph_communities,
  persist-HNSW (reopen + await_quiescent siap ~10 ms).
- **Embedder MiniLM pure-Rust** (`crates/mb-embed` + `src/bin/embed_poc.rs`):
  candle 0.11 + hf-hub (rustls) + tokenizers; mean pooling + L2 normalize;
  end-to-end hybrid_search dengan embedding asli — hasil relevan.
- **Perf (release, unbatched, 1 forward per teks)**: ~23 doc/s indexing,
  query embed 37–114 ms. Batching = pekerjaan fase 2.
- **Lewat OSS: fastembed ditolak** — binary ONNX prebuilt (ort-sys) butuh
  glibc 2.32+/libstdc++ baru; box CentOS 7 (glibc 2.17). → candle pure-Rust.

### 0.2 Kendala yang dikunci

1. **Hako::open harus inisialisasi rayon global PERTAMA** (engine memanggil
   `build_global().unwrap()` di `RAYON_INIT.call_once` — panik jika pool
   sudah diambil candle/tokenizers). Server: buka instance project sebelum
   embedder aktif; embedder tidak boleh jalan sebelum open pertama.
2. **API candle 0.11**: `BertModel::load(vb, config)` auto-deteksi prefix;
   `forward(input_ids, token_type_ids, attention_mask)`; `pow` butuh shape
   sama (pakai `sqr()`); normalisasi L2 manual.
3. **MiniLM English-primary** — query Indonesia sedikit melemah (observasi
   POC); komentar kode umumnya Inggris, tetap layak. Opsi upgrade:
   `jinaai/jina-embeddings-v2-base-code` (kode) atau model bilingual.

```rust
// engine (src/engine/engine.rs)
hybrid_search(collection, text_field, text_query, vec_field, query: &[f32],
              k, rrf_k) -> Result<Vec<(String, f64, HakoDoc)>>      // RRF, score desc
find_near_scored(collection, field, query: &[f32], k, max_distance)
              -> Result<Vec<(String, f64, HakoDoc)>>                // score = distance, lower = closer
find_paths(collection, id, spec: &TraverseSpec) -> Vec<TraverseHit>  // TraverseHit{collection,id,depth,cost,path,via,doc}
subgraph_json(collection, seed_id, spec) -> Result<String>           // JSON {"nodes":[...],"edges":[...]}
graph_pagerank(collection, seeds: &[(String,String)], spec, damping, top)
              -> Vec<((String,String), f64)>
graph_communities(collection, spec) -> Vec<Vec<(String, String)>>
create_vector_index(col, field, dim, Metric) | create_vector_index_with_params
create_fts_index(col, field) | create_traverse_index(col) | create_edge_index(col)
vector_index_complete(col, field) | await_quiescent(timeout)
```

- **Relasi antar-node via koleksi edge** wajib `via_edge: Some("edges")` pada
  `TraverseSpec` (BFS/Dijkstra); walk inline `Reference` berjalan tanpa itu.
- **Recovery index async setelah open** — selalu `await_quiescent()` sebelum
  query pertama; persist-HNSW membuat reopen siap ~10 ms (POC 6 chunk).
- Vektor disisipkan sebagai `Value::Binary(encode_f32s(&v))`.
- `TraverseSpec` butuh `max_depth` 1..=256, `limit` >= 1, `spec.validate()`.


## 1. Tujuan

Satu server terpusat ("memory box") yang memberi model AI **prolonged memory, live
context, codebase memory, dan self-learning skill** — sehingga model cepat mendapat
konteks codebase dan hemat token/request.

- **Integrasi utama:** plugin OpenCode V2 (klien tipis HTTP).
- **Integrasi sekunder:** MCP adapter tipis (portabel, opsional).
- **Server:** Rust (hemat memori, binary statis), multi-OpenCode, multi-project.

## 2. Keputusan desain (terkunci)

| Keputusan | Pilihan | Alasan |
|---|---|---|
| Runtime server | **Rust + axum**, link crate `hakodb` langsung | Hemat memori; 1 binary statis; hakobackend tidak memadai (lihat §3) |
| Storage | HakoDB **v0.20.6** in-process, 1 instance per project | Isolasi bersih, backup/enkripsi per project; lazy open + LRU |
| Embedding | **MiniLM-L6 384d lokal (ONNX)** — fastembed-rs / ort | Gratis, offline, hemat token; kualitas cukup untuk kode |
| Cakupan | Semua sekaligus (codebase memory + long-term memory + skill) | — |
| Data root | `~/.memory-box/projects/<projectId>/` | `projectId` = hash dari `project.canonical` (klien kirim) |

## 3. Kajian ringkas (sumber: release notes, PR, HTTP_CONTRACT)

### 3.1 HakoDB v0.20.5/0.20.6 — "LLM-era retrieval" (belum di docs/CHANGELOG main)

- **Hybrid RRF** (`hybrid_search`, FFI `hk_engine_hybrid_search`): FTS (sparse,
  tf-idf-lite atas inverted index) + vector KNN (dense) lebur via Reciprocal Rank
  Fusion. Satu sisi kosong = urutan sisi lain. → lapisan retriever hybrid custom
  TIDAK perlu dibangun.
- **GraphRAG primitives**: `graph_communities` (label propagation),
  `graph_pagerank` (PPR, teleport mass pada seeds, FFI
  `hk_engine_graph_pagerank`).
- **Subgraph satu operasi**: `Hako::subgraph_json` (FFI `hk_engine_subgraph`) —
  N-hop induced neighborhood dari seed, aturan expansion/filter/liveness sama
  dengan `find_paths`.
- **HNSW persist di index snapshot** → restart tanpa rebuild; **graph GC on
  compact**.
- **Filtered-HNSW traversal** + **binary stage-1 popcount search** (int8).
- **SIMD**: AVX2 (x86) + NEON (aarch64) kernels; parallel bulk HNSW build;
  CSR adjacency interned-id untuk whole-graph analytics (~4.8× repeat).
- v0.20.0: **soft delete / sweeper / restore** → dasar fitur "forget".
- v0.19.4: hybrid (filter+vector) satu query; `M`/`ef` configurable; Inner
  Product; batch `find_near`; multi vector-field per koleksi.
- Aturan penting: index dibuat **sebelum** tulis pertama; dim corpus=query
  (hard error); traverse/edge index dibuat sebelum reference ditulis;
  `vector_index_complete()` sebelum klaim siap.

### 3.2 OpenCode V2 plugin (sumber: docs opencode.ai/v2)

- Server part di `.opencode/plugins/`, `@opencode/plugin`, `ctx.tool.transform`
  (tools namespaced, `codemode: true`).
- **Hooks**: `session.hook("context")` (injeksi konteks pre-dispatch — inilah
  hemat token utama), `session.hook("prompt")` (capture episodic),
  `session.hook("compaction")` (ringkasan → semantic memory; bisa `event.result`).
- `ctx.session.instructions.entry.put` — konteks sticky tanpa message turn.
- `ctx.experimental.generate.text` — LLM background untuk distilasi skill;
  `ctx.skill.transform` — daftarkan skill dinamis.
- MCP: `opencode mcp add` (stdio/remote), tools bernama `<server>_<tool>`.

### 3.3 Mengapa bukan hakobackend?

Sudah punya `/api/vector-index|vector-search|traverse-*|edge-index`, FTS,
batch/tx, WS/SSE, soft delete, binary musl. Tapi: (1) wave-1 retrieval tidak
terekpos; (2) multi-tenancy dinyatakan out-of-scope & driver hako hanya melayani
`default` db; (3) tanpa embedding; (4) permukaan auth/policy web-app tidak
dipakai. → Dipakai sebagai **referensi arsitektur** saja.

## 4. Arsitektur

```
┌─ OpenCode A (project X) ┐   ┌─ OpenCode B (project Y) ┐
│  plugin TS:             │   │  plugin TS:             │
│  hooks context/prompt/  │   │  (klien tipis HTTP)     │
│  compaction + tools +   │   │                         │
│  skill learner          │   │                         │
└───────────┬─────────────┘   └───────────┬─────────────┘
            └──────── HTTP + WS ───────────┘
                 ▼
   memory-box server (Rust, axum, binary statis)
   ├─ embedder: MiniLM-L6 384d (ONNX, lokal, 1 instance dibagi semua klien)
   ├─ indexer: notify (watch) + tree-sitter chunker → chunks/symbols/edges
   ├─ retrieval: hybrid_search (RRF native) + subgraph_json + graph_pagerank
   │             + scoring recency/importance (lapisan custom kecil)
   ├─ memory: write/search episodic·semantic·procedural (soft delete = forget)
   └─ storage: hakodb 0.20.6 in-process, 1 instance per project (LRU)
```

### 4.1 Skema koleksi per project

| Collection | Isi | Index |
|---|---|---|
| `chunks` | potongan kode (path, symbol, start_line, content, embedding, hash, lang) | vector(384, Cosine) + FTS(content) + simple(path/hash) |
| `files` | manifest: hash, mtime, lang, status | simple(path) |
| `symbols` | simbol; field `ref` → chunks; edges antar simbol | traverse + simple(kind) |
| `edges` | `_from`/`_to` + `w` (import=1, call=2, same_module=0.5) | edge index |
| `mem_episodic` | event sesi: pertanyaan, keputusan, file disentuh | vector + FTS |
| `mem_semantic` | fakta/ringkasan jangka panjang (dari compaction) | vector + FTS |
| `mem_procedural` | skill hasil distilasi (judul, langkah, skor sukses) | vector + FTS |
| `meta` | versi skema, konfigurasi, hakoignore, model embedding | — |

### 4.2 Endpoint server (draf)

```
GET  /healthz
POST /v1/projects/open            {canonical_path} → {project_id}
POST /v1/index/scan               {project_id, path?} → {queued}
GET  /v1/index/status             ?project_id=
POST /v1/retrieve                 {project_id, query, filters?, k?, budget_tokens?}
POST /v1/graph/context            {project_id, seed, hops?, k?}          (subgraph)
POST /v1/graph/rank               {project_id, seeds[], damping?, top?}  (PPR)
POST /v1/memory/write             {project_id, kind, text, metadata}
POST /v1/memory/search            {project_id, query, kind?, k?}
POST /v1/memory/forget            {project_id, id}                       (soft delete)
WS   /v1/events                   progress indexing, invalidasi
```

Auth: bind `127.0.0.1` default; bearer token opsional untuk LAN.

## 5. Fase

1. **POC (sekarang)** — build hakodb 0.20.6; validasi `hybrid_search`,
   `subgraph_json`, `graph_pagerank`, persist-HNSW; embedder MiniLM lokal;
   index repo kecil end-to-end.
   *Sumber kebenaran API: source + `tests/hybrid_search.rs`, `tests/traverse.rs`
   (docs tidak update).*
2. **Server** — axum + WS, instance manager, indexer inkremental, endpoint `/v1/*`.
3. **Plugin OpenCode** — hooks + tools `memory.*` + compaction-to-memory.
4. **Self-learning skill** + MCP adapter tipis.
5. **Hardening & evaluasi** — enkripsi opsional, token LAN, benchmark penghematan
   token vs baseline grep+read, tuning kualitas retrieval.

## 6. Risiko & mitigasi

| Risiko | Mitigasi |
|---|---|
| API v0.20.5 muda, tanpa docs | Kunci tag v0.20.6; shim tipis `mb-hako` agar perubahan API tidak merembet |
| Kualitas retrieval | Evaluasi kecil di fase 2; `autoInject` config default-off dulu |
| ONNX runtime berat | fastembed-rs (ort) unduh model sekali, cache di data dir |
| Banyak instance Hako terbuka | LRU close; instance per project hanya saat aktif |
| Node tidak terpasang (plugin dev) | OpenCode memuat TS sendiri; untuk test pakai runtime OpenCode |

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

### 0.4 Hasil fase 4a: indexer tree-sitter + call-graph (teruji)

- **Koleksi baru**: `symbols` (name, kind, path, start_line, `ref_chunks` →
  chunks Reference) + `edges` (`_from`/`_to` Reference, kind=call, w=2).
- **Edge id stabil** = hash(caller|callee|kind) → re-scan idempotent.
- **Percobaan & pemecahan bug** (penting untuk pengembangan lanjut):
  - Satu pattern invalid mematikan seluruh `Query::new` (silent) → query
    per-pattern dengan skip+log.
  - Node Rust: `type_item` (bukan `type_alias_declaration`); TS
    `class_declaration name: (type_identifier)` (bukan `identifier`).
  - Span simbol = **parent** node capture `@name` (node definisi), bukan node
    nama — wajib agar call di body terhitung "di dalam" simbol.
  - Innermost enclosing: urutkan simbol `start_byte` **DESC**, lalu
    `position()` menemukan simbol terdalam lebih dulu.
  - Call berkualifikasi (`indexer::scan`): pattern
    `(call_expression function: (scoped_identifier name: (identifier) @callee))`.
- **Traverse `Both`** untuk graph/context → caller + callee dalam satu subgraph
  (edge index incoming butuh backlink? tidak — v0.15.0: incoming via edge
  collection tidak perlu backlink).
- **Struktur `subgraph_json`** (penting): `nodes` memuat juga **edge docs**
  (collection `edges`, field `_from`/`_to` = `{"__ref__": "col/id"}`);
  `edges[]` = pasangan (edge-doc → endpoint). Rendering plugin harus
  mendereferensi `_from`/`_to` dari edge doc, bukan langsung `edges[]`.
- **Perf**: 13 file → 84 simbol + 103 edge call + 46 chunk, sekali scan.
- Plugin `memory.graph`: seed nama simbol di-resolve via `/v1/symbols/lookup`
  (Eq filter), subgraph dirender "caller → callee (call)" dengan path:line.


### 0.5 Hasil fase 4b: skill learner (teruji, kecuali panggilan LLM)

- **Runtime OpenCode 2.0.24**: `ctx.experimental` hanya berisi `terminal`;
  `generate.text` tersedia sebagai **`ctx.generate.text`** (alias top-level,
  deprecated di tipe 2.3.0 tapi satu-satunya yang ada di runtime ini).
  Plugin distill pakai fallback `ctx.experimental?.generate?.text ??
  ctx.generate?.text`.
- **Tool `memory.distill`** memvalidasi rantai: input → generate.text →
  `remember("procedural")`. Saat diuji live: generate membalas error kuota
  model gratis (`Generate.UnavailableError: Free usage limit reached`) —
  blocker lingkungan, bukan kode; error tertangani & dilaporkan verbatim.
- **Sisa rantai divalidasi via API langsung**: write `mem_procedural` +
  hybrid search kind=procedural ✅.
- **Loop tertutup tervalidasi**: pelajaran prosedural yang tersimpan ikut
  terinjeksikan oleh hook `context` sebagai blok "# Pelajaran tersimpan"
  dan dibaca model ✅.
- Hook `compaction` menulis snapshot `semantic` (raw) + menjalankan
  `distill(tail)` fire-and-forget → `mem_procedural` otomatis.

### 0.6 Hasil fase 4c: auto-indexing watcher (teruji)

- Crate `notify` 8.2 (inotify); thread std per project (registry per-pid di
  AppState; `MEMORYBOX_WATCH=0` untuk mematikan; watcher dibuka di
  `projects/open`).
- **Debounce 1.5 s**: flush saat sunyi → scan ulang **per file** yang
  berubah (bukan seluruh root) → murah.
- Filter event: hanya `Create|Modify`, file source, ≤256 KB, bukan di
  SKIP_DIRS (target/node_modules/.git/...).
- **Bug penting**: scan file tunggal → `rel = strip_prefix(root) == ""` →
  ekstensi tak terbaca → parser di-skip (simbol 0). Fix: rel kosong →
  pakai `file_name`. Konsekuensi: simbol/edge hasil re-scan file tunggal
  memakai rel pendek (`parser.rs`) beda dengan scan root penuh
  (`mb-server/src/parser.rs`) → id simbol beda untuk file yang sama.
  **Selesai di fase lanjut**: rel dinormalisasi terhadap root project
  (koleksi `meta`) dan cleanup stale diimplementasikan — lihat §0.8.
- Terverifikasi live: edit file → log "watcher re-scan selesai files=1" →
  simbol baru langsung ditemukan `/v1/symbols/lookup`.
- Watcher hidup per-proses — setelah restart server, aktif lagi saat
  `projects/open` dipanggil (plugin memanggil tiap sesi, jadi otomatis).

### 0.7 Kajian upgrade hakodb v0.20.6 → v0.20.8 (teruji, diadopsi)

**v0.20.7 (#210, fix #209):** create-time traverse backfill kini scan SEMUA
shard. Dua gap berlapis diperbaiki: (1) doc yang ditulis sebelum
`create_traverse_index` tak pernah masuk backlink map (tidak ada live IndexOp
untuk di-replay); (2) race urutan create — backfill thread dari create
sebelumnya membaca `tracked` asinkron; bila `create_edge_index(edges)` jalan
sebelum `create_traverse_index(nodes)`, backfill edges melihat set targets
yang belum di-track dan menjatuhkan semua ref secara silent. Gejala:
`find_paths` mengembalikan `[]` ~20% flaky di engine terenkripsi.

**v0.20.8 (#212):** memoize fetch doc per-walk di traverse expansion. Setelah
backlink map lengkap (efek #209), tiap tetangga difetch 2× per ekspansi
(backlink verify + edge hop) — bfs 65k → 38k qps. Memo melipatnya jadi satu
fetch; entries tidak lintas-anchor sehingga semantik staleness tetap.

**Verdict "regresi" BFS** (dicatat di PR #212 + RESULTS.md bench-lab):
penurunan 63–66k → 35–37k qps BUKAN regresi — baseline lama mengukur
backlink map yang SETENGAH KOSONG (race #209 menghapus baris edge pra-index
di lane benchmark itu sendiri: respons n_0 4→8 baris, p50/call persis 2×,
12µs→23µs). 0.20.7 memulihkan KEBENARAN data — qps turun karena kerja per
call lebih banyak (data penuh). Memo #212 netral tanpa `via_edge`, disimpan
untuk walk campuran via.

**Dampak ke memory-box:**
- `ensure_schema` kita membuat traverse index SEBELUM edge index dan SEMUA
  index sebelum tulis pertama → kebetulan di sisi aman dari race #209.
  v0.20.7 membuat urutan apa pun aman (hardening gratis).
- Kita memakai `via_edge` → jalur memo #212 berlaku.
- **Adopsi tervalidasi**: tag `v0.20.8` (9e1de90), API identik (diff hanya
  internal engine.rs + test). POC wave-1: SEMUA LOLOS (hybrid RRF, KNN,
  BFS, subgraph, pagerank, communities, persist-HNSW reopen 10 ms).
  e2e server: retrieve 39–47 ms (baseline 37–44), graph 3 ms, write/watcher/
  cleanup OK.

**Bonus fix saat kaji**: `subgraph_json` mengembalikan doc utuh termasuk
`emb` (384 float) per chunk → respons graph 79.7 KB. Server kini strip `emb`
dari node sebelum dikirim → **7.5 KB (−90%)**.

### 0.8 Re-resolusi edge lintas-file + model multibahasa (teruji)

**Edge lintas-file.** Dua masalah inkremental yang sebelumnya diketahui:
(a) rescan file tunggal kehilangan edge keluar lintas-file (resolusi hanya
melihat simbol batch); (b) edge masuk ke file callee yang berubah jadi
dangling. Solusi — tiga bagian:
- Edge id kini stabil by NAME: hash(caller|callee_name|call) — target boleh
  pindah file tanpa mengubah id edge; doc edge membawa `callee` + `to_path`.
- `resolve_callee` dengan fallback DB: nama tidak ada di batch → query
  `symbols name==` (prefer path caller) → edge keluar lintas-file tetap
  terbentuk saat rescan file tunggal.
- `settle_edges` PASCA-TULIS: rekonsiliasi edge milik rel batch terhadap
  state DB final — caller hilang → hapus; callee hilang → hapus; callee
  pindah file → rewrite (_to, to_path). Dipanggil setelah write_batch di
  scan() dan remove_rel → **bebas race urutan file** dalam drain watcher
  (resolusi melihat hasil tulisan seluruh batch, bukan state antara).
- `stale_deletes` edge: hapus format lama (tanpa `callee`) + yang tidak
  diregenerasi batch; migrasi format lewat rescan penuh (settled=7 saat
  rescan migrasi).

Teruji live (tanpa menyentuh file caller):
- pindah `helper_x` lib_a.rs → lib_c.rs: edge ikut (_to lib_c, to_path
  ter-update) hanya dari event watcher kedua file itu.
- hapus lib_c.rs: edge call terhapus. (Edge "sisa" yang tampak di subgraph
  adalah pasangan symbol→chunk ref, bukan call.)

**Model multibahasa.** MiniLM-L6 English-primary membuat recall Indonesia
lemah (eval §0.6: ID 4/12 vs EN 10/12). Diganti default ke
`paraphrase-multilingual-MiniLM-L12-v2` (384 dim, BERT standar + mean
pooling, tidak gated, ~470 MB):
- Sanity: paraphrase lintas-bahasa ID↔EN 0.91, ID↔ID 0.76, beda topik ≈ 0.
- `MEMORYBOX_MODEL=multi|en|<hf-repo>` (default multi); cache model kini
  `~/.cache/huggingface/hub` (dipakai bersama antar proses, override
  `MEMORYBOX_MODEL_CACHE`) — dulu per-data-dir sehingga tiap server
  mengunduh ulang 470 MB.
- Embedding model-specific → ganti model WAJIB wipe data + rescan penuh.
- Hasil eval ulang: **ID 8/12, EN 8/12** (seimbang; sebelumnya 4/10) —
  hemat 58–69%. Latensi retrieve justru turun: 39–47 ms → **29–31 ms**.
- Contoh sanity di `crates/mb-embed/examples/sanity.rs`.



### 0.9 Uji distill end-to-end di tengah kuota LLM habis

Masalah: hook compaction memanggil `ctx.generate.text` untuk merangkum
pelajaran. Di box ini kuota **generate** model free habis (chat sesi masih
jalan) — semua model free (`hy4`, `deepseek-v4.1`, `flint-40b`,
`inkling-2-small`, …) mengembalikan `Generate.UnavailableError`. Ini blocker
lingkungan, bukan kode.

Dua rute uji:

1. **Rute LLM asli** (saat kuota tersedia / model berbayar). Plugin kini
   menerima `MEMORYBOX_DISTILL_MODEL=providerID/modelID` — diteruskan sebagai
   opsi `model` ke `ctx.generate.text` (divalidasi: error menyertakan
   `model={"providerID":"opencode","id":"inkling-2-small-free"}`). Jadi model
   distill bisa berbeda dari model sesi. JalankanTool `memory.compactionTest`
   (tidak dipanggil) dengan env `MEMORYBOX_TEST_TOOLS=1` dan
   `MEMORYBOX_DISTILL_MODEL=<model-berkuota>`, verifikasi `mem_procedural`
   dan injeksi seperti di bawah.
2. **Rute mock (dipakai sekarang)**: `MEMORYBOX_DISTILL_MODE=mock` mengganti
   panggilan LLM dengan ekstraksi bullet deterministik dari transkrip —
   rantai `compaction → snapshot semantic → tulis mem_procedural → injeksi
   blok "# Pelajaran tersimpan" ke sesi berikutnya` diuji penuh tanpa LLM.

Skrip: `bash scripts/test-distill-e2e.sh` (env `MARK`, `MODEL`, `SVR`).
Alur: sesi 1 dengan marker unik → `memory.compactionTest` → cek marker di
`/v1/memory/search kind=procedural` → sesi baru → cek marker muncul dari
blok injeksi. **Hasil: LULUS** (marker `MARKER-UJI-...` ditemukan di
`mem_procedural` dan muncul di jawaban sesi baru).

Catatan: plugin terdaftar lewat `opencode.json` project; `ctx.session.compact`
tersedia di runtime 2.0.24 dan memicu hook `compaction` (dokumentasi resmi).



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

### 0.3 Hasil fase 2–3 (server + plugin, teruji end-to-end)

- **Perf release**: index 804 chunk (72 file) / 33 s (~24 chunk/s, unbatched);
  retrieve end-to-end (embed query + hybrid) ≈ 60 ms; debug build 40× lebih
  lambat → selalu release.
- **Plugin OpenCode V2.0.24 tervalidasi live** (`plugins/memory-box/index.ts`):
  - Muat via `plugins: [<path>]` di `opencode.json` — **tanpa node/npm**:
    plugin = default export `{ id, setup }` (Plugin.define identity; import
    `@opencode/plugin` gagal di-resolve karena box tanpa node).
  - hook `prompt` → episodic ✅ (terverifikasi via /v1/memory/search)
  - hook `context` → injeksi blok `<memory-box>` ✅ (model melaporkan isi blok)
  - tools `memory.search/save/graph` dipanggil model via Code Mode ✅
  - Kegagalan server = plugin pasif (fail-safe) ✅
- **Sumber kebenaran tipe plugin**: tarball npm `@opencode/plugin` 2.3.0 +
  `@opencode/schema` 2.3.0 (node_modules di-gitignore); kesimpulan tipe:
  - `SessionPrompt.prompt = { text, ... }` (PromptInput.Prompt)
  - `SessionContext = { system: SystemPart[], messages: Message[], options }`;
    `SystemPart = { type: "text", text }`
  - Message user/assistant: `{ role, content: ContentPart[] }` — part teks di
    `content`, BUKAN `parts`
  - `Tool.Info = { name, input, description, execute(input, ctx),
    options: { namespace?, codemode? } }`; `Result = { content?: string }`

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
| Storage | HakoDB **v0.20.8** in-process, 1 instance per project | Isolasi bersih, backup/enkripsi per project; lazy open (instance tetap terbuka sampai proses mati — LRU belum diimplementasi, lihat §6) |
| Embedding | **paraphrase-multilingual-MiniLM-L12-v2 384d lokal (candle)**; `MEMORYBOX_MODEL=en` → MiniLM-L6 | Gratis, offline, bilingual (ID/EN); candle pure-Rust untuk glibc 2.17 |
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
            └──────── HTTP ──────────────────┘
                 ▼
   memory-box server (Rust, axum, binary statis)
   ├─ embedder: MiniLM 384d multibahasa (candle, lokal, 1 instance dibagi semua klien)
   ├─ indexer: notify (watch) + tree-sitter chunker → chunks/symbols/edges
   ├─ retrieval: hybrid_search (RRF native) + subgraph_json + graph_pagerank
   │             + scoring recency/importance (lapisan custom kecil)
   ├─ memory: write/search episodic·semantic·procedural (soft delete = forget)
   └─ storage: hakodb 0.20.8 in-process, 1 instance per project (open seumur proses; LRU belum diimplementasi)
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
POST /v1/memory/forget            {project_id, kind, id}                 (soft delete)
WS   /v1/events                   progress indexing, invalidasi
```

Catatan: `WS /v1/events` masih **draf** — belum terdaftar di `router()`
(`crates/mb-server/src/lib.rs`). Semua klien saat ini memakai HTTP biasa.

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
| API hakodb muda, tanpa docs | Kunci tag v0.20.8 (API tervalidasi POC); catat perubahan di docs/PLAN.md §0.7 |
| Kualitas retrieval | Evaluasi di scripts/eval-token.py; `autoInject` default-on, bisa dimatikan via opsi plugin |
| Model ~470 MB | candle + cache HF standar; salin cache ke box lain untuk instalasi offline |
| Banyak instance Hako terbuka | Belum ada eviksi — instance ditutup saat proses mati. LRU direncanakan; pantau jumlah project aktif. |
| Node tidak terpasang (plugin dev) | OpenCode memuat TS sendiri; untuk test pakai runtime OpenCode |

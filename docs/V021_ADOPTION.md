# Adopsi HakoDB v0.21.0 di memory-box — analisis issue #217 + rencana eksekusi

Tanggal: 2026-11-17
Sumber: https://github.com/hakodb/hakodb/issues/217 (wave ditutup, PR #218–#222)
Tag: `v0.21.0` (commit `445495d`); baseline `v0.20.8` (`9e1de90`).

## 1. Kronologi issue #217

Issue #217 dibuka dari kajian `~/hakodb_issue_new.md` (memory-box multi-user).
 maintainer memverifikasi klaim terhadap `main` (0.20.8 + #215/#216) dan **mengoreksi dua hal**:

1. Klaim kajian "search_filtered tidak pernah dipanggil engine" **terusang**: Query executor sudah memakainya sejak #192 (filtered-ANN lengkap: admission + x4/x16 widening + exact fallback, `executor.rs:2075/2108`). Jadi `Operator::Near + Query.filters` sudah menghasilkan filtered vector search dengan semantic full-k. Yang kurang hanya di jalur **RRF** dan convenience API-nya.
2. Klaim "tenant escape lewat graph edge" **terlalu pesimistis**: `TraverseSpec.node_match` sudah diterapkan ke tiap tetangga sebelum dikunjungi (#156). Celahnya bukan scoping per-node, tapi multi-predikat AND/OR.

Rekomendasi diterima dengan prioritas ulang, lalu dieksekusi sebagai wave PR:

| PR | Item | Hasil (dari issue) |
|---|---|---|
| #218 | P0-C walk/render split + count FFI | `bfs` walk ≈4.7 µs vs full ≈24 µs — sisa ≈81% ada di output-side (fetch+render), **bukan engine walk**. Cardinality assert PASS 16 vs 16. Menutup pertanyaan #214. |
| #219 | P0-A `hybrid_search_filtered` + `find_near_scored_filtered` | Text side filter **sebelum** truncate; vector side numpang executor filtered-ANN; `extras_match` jadi matcher tunggal di `query/filter.rs`. 10/10 acceptance test hijau. |
| #220 | P0-B `subgraph_json_projected` | `None` = legacy byte-identik; `Some(fields)` proyeksi node-doc. Test `subgraph_json_projection_scopes_fields`. |
| #221 | P1 `open_exclusive` | OS advisory lock `hako.lock`, error keras, release-on-drop; `open()` biasa tetap unchanged. |
| #222 | P2 `ensure_indexes` + idempotent `create_*` | Def identik → no-op `Ok(false)`; def vector beda → `InvalidInput`. `IndexSchema {fts, vector, tracked}`. Menutup race class #209. |

Sengaja **deferred** (consumer belum aktif): `update_where`+`query_stream`, `stats()`, edge-doc normalization / `relationships[]`, rich `FilterExpr`, selectivity qps bench, FFI mirror.

## 2. Verifikasi terhadap kode v0.21.0 (checkout lokal)

Diff `v0.20.8..v0.21.0`: +904/−66 baris di 14 file (engine, filter, executor, manager, traverse, FFI, 4 test file). API baru yang relevan:

```rust
// engine.rs
pub fn hybrid_search_filtered(
    &self, collection, text_field, text_query, vec_field, query: &[f32],
    k, rrf_k, filters: &[Filter]) -> Result<Vec<(String, f64, HakoDoc)>>;
pub fn find_near_scored_filtered(
    &self, collection, field, query, k, max_distance, filters: &[Filter]) -> Result<...>;
pub fn subgraph_json_projected(
    &self, collection, seed_id, spec, fields: Option<&[String]>) -> Result<String>;
pub fn open_exclusive(path, config) -> Result<Self>;
pub fn ensure_indexes(&self, schema: &IndexSchema) -> Result<()>;
pub fn find_paths_count(...) -> ...; // dari P0-C
```

`hybrid_search()` sekarang **delegasi** ke `hybrid_search_filtered(..., &[])` — byte-identik untuk caller lama.
`Filter { field, op, value }` dan `Operator::{Eq, Ne, Gt, ...}` tersedia via `hakodb::query::filter`.
`IndexSchema { fts: Vec<(col,field)>, vector: Vec<(col,field,dim,Metric)>, tracked: Vec<col> }` ada di `hakodb::engine`.

## 3. Dampak langsung ke memory-box ( apa yang bisa dihapus )

| Workaround memory-box | Status | Tindakan |
|---|---|---|
| Over-fetch `k*5` + buang doc `user_id` lain di `memory_search` | **Bisa dihapus** | Pakai `hybrid_search_filtered(..., &[Filter{field:"user_id", op:Eq, value:String(sub)}])`, k langsung k. Full-k, tidak ada decode/RRF sia-sia, tidak ada data tenant lain di heap. |
| Strip `emb` dari `subgraph_json` di `api.rs` (79.7 KB → 7.5 KB) | **Bisa dihapus** | Pakai `subgraph_json_projected(..., Some(&FIELDS))`; dengan proyeksi, `emb` tidak pernah ikut serialized. Hemat JSON parse di server juga. |
| `ensure_schema` manual tiap open (create FTS/vector/traverse) | **Bisa disederhanakan** | Deklarasi sekali lewat `IndexSchema` + `ensure_indexes(&schema)` — idempotent, menghilangkan class bug #209. `is_indexes_ready()` sudah ada untuk readiness. |
| LRU ditunda karena risiko double-open | **Prasyarat terpenuhi** | `open_exclusive` tersedia. Tapi LRU masih butuh refactor watcher → `pid` + `Weak<Hako>` dulu; engine sekarang tidak lagi penghalang. |
| Backfill via `db.query(limit 100k)` + loop `put` | **Tetap** | `update_where`/`query_stream` sengaja deferred. Backfill sekarang sudah jalan dan teruji; tidak perlu diubah. |

Tidak ada perubahan pada plugin/MCP: token, alias, scope user, dan auth tetap app-level.

## 4. Rekomendasi eksekusi memory-box (urutan)

### E1 — Upgrade pin ke v0.21.0 + verifikasi (lakukan dulu)
- `Cargo.toml`: `tag = "v0.21.0"`, `cargo update -p hakodb`.
- Jalankan `cargo test --workspace` (20 test) + POC wave-1 `mb-poc` (hybrid, KNN, BFS, subgraph, pagerank, communities, persist-HNSW). Catat: `hybrid_search` kini delegasi → hasil harus identik.
- Build release, restart, smoke: retrieve, graph, memory search, MCP tools.
- Catat latensi sebelum/sesudah untuk regression check.

### E2 — Ganti `memory_search` ke `hybrid_search_filtered` (P0-A adoption)
- Hapus kandidat `k*5` dan filter manual. Filter langsung di query:
  `Filter { field: "user_id", op: Operator::Eq, value: Value::String(sub) }`.
- Admin: untuk melihat semua dokumen (termasuk legacy tanpa `user_id`), jangan kirim filter — `sub` tidak dipakai. Perilaku ini setara dengan `auth.admin` saat ini.
- Tambah integration test: hasil subset user, **full-k** saat data tersedia, tidak ada id user lain bocor, paritas dengan jalur lama.
- Hapus komentar "over-fetch sementara" di `docs/AUTH.md`.

### E3 — Ganti strip `emb` ke `subgraph_json_projected` (P0-B adoption)
- Definisikan `const GRAPH_FIELDS: &[&str] = ["name","kind","path","start_line","content","_from","_to","callee","to_path","w","text","ts"]`; pass `Some(&fields.iter().map(...).collect::<Vec<String>>())`.
- Verifikasi `graph/context` tetap mengembalikan edge-doc dengan `_from`/`_to` + `callee`/`to_path` (plugin dan `mb-mcp` bergantung pada itu).
- Hapus blok `doc.remove("emb")` di `api.rs` dan catatan workaround di PLAN/AUTH.

### E4 — `ensure_indexes` saat startup (P2 adoption, hardening)
- Bangun `IndexSchema` dari konstanta `MEMORY_COLLECTIONS` + koleksi chunks/symbols/edges.
- Ganti `ensure_schema` (create per index) dengan satu `db.ensure_indexes(&schema)`.
- Hati-hati: `create_vector_index` dengan def berbeda sekarang **error keras** — ini bagus, tapi pastikan schema memory-box identik dengan yang lama (DIM=384, metric cosine/dot sesuai `store.rs`) supaya reopen data lama tidak gagal. Uji reopen data existing wajib.

### E5 — LRU aman (opsional, setelah E1–E4 stabil)
- Engine tidak lagi penghalang (`open_exclusive`), tapi lakukan prasyarat aplikasi: watcher simpan `pid`, ambil `store.get()` tiap flush / pakai `Weak<Hako>`.
- Baru tambah capacity + idle TTL + `Arc::strong_count == 1` guard.
- Test evict→reopen dan watcher re-acquire.
- Butuh `stats()` untuk choosing threshold — masih deferred di engine; untuk sekarang pakai jumlah project aktif dari registry.

### Tidak dilakukan sekarang
- `update_where`/`query_stream`, `stats()`, edge-doc normalization, rich `FilterExpr`, FFI mirror: sesuai alasan deferred issue #217. Backfill dan deref edge-doc sudah berjalan; tidak ada tekanan nyata.

## 5. Risiko & mitigasi

| Risiko | Mitigasi |
|---|---|
| `ensure_indexes` error "different vector def" saat reopen data lama | E4 harus diawali uji reopen.save data; bila metric lama berbeda, migrasi eksplisit (wipe+rescan) atau biarkan `ensure_schema` lama untuk project existing. |
| `subgraph_json_projected` menghilangkan field yang dipakai plugin | E3: daftar field eksplisit + test `mb-mcp` dan plugin session. Jangan lupa `content`/`path` untuk chunk. |
| Perilaku `hybrid_search_filtered` untuk admin tanpa filter | E2: kirim `&[]` = persis `hybrid_search()` lama,包括 dokumen legacy tanpa `user_id`. |
| Regresi performa engine | E1: bandingkan latensi retrieve/graph sebelum-sesudah; POC wave-1 + cardinality assert (issue sudah menambah ini di gate bench). |

## 6. Kesimpulan

Issue #217 menutup semua rekomendasi P0–P2 dari kajian kita, dengan dua koreksi penting: filtered-ANN sudah lama ada di executor (tinggal disambung ke RRF), dan `node_match` graph sudah scoped ke tetangga. Untuk memory-box, dampak paling besar adalah **menghapus dua workaround**: over-fetch filter owner di `memory_search` dan strip `emb` di `graph/context` — keduanya kini jadi satu panggilan engine.
Urutan aman: **upgrade & uji (E1) → filtered search (E2) → proyeksi graph (E3) → ensure_indexes (E4) → LRU (E5, opsional)**.

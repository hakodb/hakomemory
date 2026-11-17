# Item engine yang sengaja deferred — keputusan final memory-box

Sumber: hakodb issue #217 (wave P0–P2 sudah diadopsi di `docs/V021_ADOPTION.md`).
Tabel ini menutup item yang **tidak** diimplementasikan di memory-box, beserta
alasan dan pemicu untuk membukanya kembali.

| Item upstream | Status memory-box | Keputusan | Trigger revisi |
|---|---|---|---|
| `update_where` + `query_stream` | Backfill `user_id` memakai `Query::new(col).limit(100k)` → loop `put` (admin endpoint). | **Tidak diadopsi.** Cukup untuk skala saat ini (ratusan doc/project). | Backfill/migrasi > ~100k doc/project, atau WAL tumbuh saat scan panjang → butuh API atomic-aware + cursor. |
| `stats()` → `EngineStats` | LRU memakai `active_count()`/registry + `MEMORYBOX_MAX_PROJECTS`/idle TTL sendiri. | **Tidak diadopsi.** Belum ada keputusan eviksi berbasis memori HNSW/WAL. | LRU aktif dengan banyak project dan threshold perlu dipilih dari metrik engine nyata. |
| edge-doc normalization / `relationships[]` | Plugin OpenCode dan `mb-mcp` sudah melakukan deref `_from`/`_to` dua lapis; `subgraph_json_projected` sudah dipakai. | **Tidak diadopsi.** Tidak ada pain nyata bagi pemakai saat ini. | Revisi plugin/MCP berikutnya (berpasangan dengan adopsi proyeksi subgraph). |
| Rich `FilterExpr` (AND/OR lintas query/hybrid/traverse) | Filter yang dipakai: `user_id` Eq + `agent_id`/`session_id` Eq/In lewat `hybrid_search_filtered`. | **Tidak diadopsi.** Sudah cukup untuk scope multi-user + multi-agent. | Traversal butuh predikat majemuk yang tidak bisa dinyatakan dengan field filter saat ini. |
| Selectivity qps bench (10/50/90%) | Tidak ada. | **Tidak diadopsi** — berada di bench-lab/hako-backend, bukan repo ini. | Ada keputusan performa filtered-ANN yang menunggu angka. |
| FFI mirror untuk P0-A/B/P1/P2 | Tidak ada. | **Tidak diadopsi.** memory-box adalah konsumen Rust in-process; satellite sudah diarsipkan. | Ada konsumen C/NIM yang benar-benar membutuhkan. |

## Kesimpulan

Semua item P4 tetap **deferred di upstream issue #217**, dan memory-box sengaja
tidak menirunya. Ini bukan utang teknis yang menghalangi fitur — masing-masing
punya trigger eksplisit di atas. Jika trigger terjadi, kerjakan di hakodb
dulu, lalu adopsi di sini seperti pola v0.20.8 → v0.21.0.

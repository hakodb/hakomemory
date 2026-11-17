# memory-box plugin (OpenCode V2)

Plugin server OpenCode yang menghubungkan sesi ke memory-box server
(Rust) untuk prolonged memory + konteks codebase otomatis.

## Cara kerja

| Hook | Fungsi |
|---|---|
| `prompt` | pertanyaan user → `mem_episodic` |
| `context` | sebelum dispatch model: retrieve top-k → blok `<memory-box>` di system (hemat token: model tidak perlu grep/read berulang) |
| `compaction` | ringkasan percakapan → `mem_semantic` |
| tools `memory.*` | `search` (hybrid retrieval), `save`, `graph` (subgraph) |

Semua jaringan gagal-aman: kalau server mati, plugin pasif dan sesi tetap jalan.

## Instalasi

1. Jalankan server:

   ```bash
   cd ~/memory-box && ./target/release/mb-server   # default http://127.0.0.1:7412
   ```

2. Daftarkan plugin di `opencode.json` project (atau global):

   ```json
   {
     "plugins": ["./plugins/memory-box"]
   }
   ```

3. (Opsional) konfigurasi lewat opsi plugin:

   ```json
   {
     "plugins": [{ "path": "./plugins/memory-box", "options": {
       "server": "http://127.0.0.1:7412",
       "autoInject": true
     }}]
   }
   ```

## Catatan teknis

- Tidak butuh node/npm: plugin tidak meng-import apa pun; bentuk plugin
  hanyalah `{ id, setup }` default export (sesuai `@opencode/plugin`
  v2.3.0 — `Plugin.define` identity function).
- Tipe referensi ada di `node_modules/@opencode/plugin` + `@opencode/schema`
  (di-gitignore; cara ambil: unduh tarball npm `@opencode/plugin`).
- Indeks codebase diisi via `/v1/index/scan` (sekarang manual; nanti otomatis
  oleh server watcher + tree-sitter).

## Uji distill end-to-end

`bash scripts/test-distill-e2e.sh` menguji rantai penuh:
compaction → snapshot episodic/semantic → tulis `mem_procedural` → injeksi
`# Pelajaran tersimpan` di sesi berikutnya.

Saat kuota `ctx.generate.text` habis, set `MEMORYBOX_DISTILL_MODE=mock`
(skrip sudah melakukannya) — distill memakai ekstraksi bullet deterministik
tanpa LLM, jadi plumbing tetap tervalidasi. Saat kuota/model tersedia, jalankan
sesi normal dengan `MEMORYBOX_DISTILL_MODEL=providerID/modelID` untuk
mengarahkan generate ke model dengan kuota (boleh beda dari model sesi).
Tool `memory.compactionTest` hanya aktif dengan `MEMORYBOX_TEST_TOOLS=1`.

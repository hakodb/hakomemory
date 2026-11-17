# Deploy memory-box ke box lain

Target: CentOS 7 (glibc 2.17, GCC 4.8.5), **tanpa node/npm**. Semua binary
murni Rust; plugin OpenCode hanya butuh runtime OpenCode (bukan Node).

## 1. Prasyarat di box tujuan

- `git`, `curl`, toolchain Rust (rustup stable) + `gcc`/`make` untuk build crate
  native (tree-sitter dll.).compiler here sudah terbukti di CentOS 7.
- Network keluar ke: GitHub (git hakodb tag `v0.20.8` + crates.io) dan
  huggingface.co (unduh model sekali, ~470 MB) **atau** salin cache model dari
  box lama (lihat langkah 4).
- OpenCode V2 sudah terpasang (untuk plugin). Klien MCP opsional.

## 2. Build

```bash
git clone <repo memory-box> ~/memory-box
cd ~/memory-box
cargo build --release -p mb-server
# hasil: target/release/mb-server + target/release/mb-mcp
```

`Cargo.lock` mengunci hakodb `v0.20.8` — jangan dihapus jika ingin hasil
identik. Tidak perlu node/npm untuk menjalankan plugin maupun server.

## 3. Jalankan server

```bash
MEMORYBOX_DATA=~/.memory-box \
MEMORYBOX_PORT=7412 \
MEMORYBOX_MODEL=multi \
MEMORYBOX_WATCH=1 \
./target/release/mb-server
```

Default: data `~/.memory-box`, port `7412`, model `multi`
(`paraphrase-multilingual-MiniLM-L12-v2`, 384d), auto-watch aktif.
Variabel lain: `MEMORYBOX_MODEL_CACHE` (lokasi cache HF),
`MEMORYBOX_WATCH=0` (matikan watcher), `MEMORYBOX_MODEL=en` (model Inggris).

Server **hanya bind 127.0.0.1 dan tanpa autentikasi** — jangan diekspos ke
jaringan. Untuk akses jarak jauh pakai SSH tunnel / reverse proxy dengan auth.

Verifikasi build & endpoint:

```bash
cargo test --workspace          # unit + integration test (mock embedder, offline)
curl -s -X POST http://127.0.0.1:7412/v1/projects/open \
  -H 'content-type: application/json' \
  -d '{"path":"/path/ke/codebase"}'
curl -s -X POST http://127.0.0.1:7412/v1/index/scan \
  -H 'content-type: application/json' \
  -d '{"project_id":"<12-hex dari respons atas>","path":"/path/ke/codebase"}'
```

### Systemd (user)

`~/.config/systemd/user/memory-box.service`:

```ini
[Unit]
Description=memory-box RAG server
After=network-online.target

[Service]
Environment=MEMORYBOX_DATA=%h/.memory-box
Environment=MEMORYBOX_PORT=7412
Environment=MEMORYBOX_MODEL=multi
ExecStart=%h/memory-box/target/release/mb-server
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now memory-box
systemctl --user status memory-box
```

## 4. Model & data

- **Cache model**: default `~/.cache/huggingface/hub`. Supaya tidak mengunduh
  470 MB di box baru, salin direktori
  `~/.cache/huggingface/hub/models--sentence-transformers--paraphrase-multilingual-MiniLM-L12-v2`
  dari box lama (atau set `MEMORYBOX_MODEL_CACHE` ke lokasi salinan).
- **Project id** = 12-hex pertama SHA-256 dari **path canonical** codebase.
  Kalau path di box baru sama persis, data lama bisa disalin:
  `~/.memory-box/projects/<id>/` (matikan server dulu saat menyalin).
  Kalau path berbeda → id berbeda → lakukan `projects/open` + `index/scan`
  ulang; itu juga yang terjadi otomatis saat plugin pertama kali jalan.
- **Ganti model embedding bersifat breaking**: vektor lama tidak kompatibel —
  wipe data project + rescan penuh.

## 5. Pasang plugin OpenCode

Plugin tidak perlu Node; cukup satu file `index.ts`. Di direktori project yang
dipakai sesi OpenCode:

```bash
mkdir -p <direktori-project>/.opencode/plugins/memory-box
cp ~/memory-box/plugins/memory-box/index.ts \
   <direktori-project>/.opencode/plugins/memory-box/index.ts
```

Buat/ubah `opencode.json` di direktori project tersebut:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugins": [{
    "package": "./.opencode/plugins/memory-box",
    "options": {
      "server": "http://127.0.0.1:7412",
      "projectPath": "/path/ke/codebase",
      "autoInject": true
    }
  }]
}
```

`projectPath` mengikat plugin ke codebase tertentu meski sesi dibuka dari
direktori lain. **Salin** `index.ts` (atau arahkan `package` ke direktori plugin
asli) — jangan hanya membuat symlink `index.ts`: loader plugin OpenCode tidak
mengikuti symlink, sehingga tool `memory.*` tidak akan terdaftar. Tanpa itu plugin memakai direktori project sesi. Restart sesi
OpenCode agar plugin termuat. Opsi lingkungan: `MEMORYBOX_DISTILL_MODEL`
(model untuk ringkasan distill; kuota mengikuti akun OpenCode masing-masing),
`MEMORYBOX_DISTILL_MODE=mock` (uji tanpa LLM).

Uji cepat: minta model memanggil `memory.search` / `memory.graph` lalu
menyebut file+baris hasilnya.

## 6. Pasang MCP adapter (opsional)

Salin `target/release/mb-mcp`, lalu daftarkan ke klien MCP:

```json
{
  "mcpServers": {
    "memory-box": {
      "command": "/path/ke/memory-box/target/release/mb-mcp",
      "env": {
        "MEMORYBOX_PROJECT": "/path/ke/codebase",
        "MEMORYBOX_URL": "http://127.0.0.1:7412"
      }
    }
  }
}
```

MCP tidak memanggil LLM sama sekali — hanya akses memory/graph server.

## 7. Catatan kuota LLM

Plugin memakai model OpenCode milik pemakai untuk distill saat compaction.
Model embedding (candle) berjalan lokal dan **tanpa kuota**. Jika kuota
`generate` habis, plugin tetap berfungsi untuk retrieval/graph/penyimpanan;
hanya ringkasan prosedural otomatis yang tertunda. Uji plumbing tanpa LLM:
`bash scripts/test-distill-e2e.sh`.

## Multi-user mode

```bash
export MEMORYBOX_AUTH_SECRET=<random-secret>
# server: same binary, auth enabled automatically

# mint a token for one user (secret from env, never argv)
MEMORYBOX_AUTH_SECRET=<random-secret> ./target/release/mb-token \
  --sub alice --roots /home/alice/project --projects <pid> --days 30

# client (plugin config or env)
export MEMORYBOX_TOKEN=<token>
export MEMORYBOX_ALIAS=work
```

`projects/open` accepts `{alias, path}` on first use and `{alias}` afterwards;
the server validates the path against the token roots and computes the project
id itself. Memory is stamped with `user_id` and filtered per user. Legacy docs
without `user_id` are visible to admin tokens only. See `docs/AUTH.md`.

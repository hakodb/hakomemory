# memory-box

RAG server untuk OpenCode: prolonged memory, live context, codebase memory,
dan self-learning skill — di atas HakoDB v0.20.8 (vector HNSW + graph, hybrid
RRF, GraphRAG primitives) dengan embedding lokal MiniLM 384d multibahasa (paraphrase-multilingual-MiniLM-L12-v2, default).

Baca [PLAN.md](PLAN.md) untuk keputusan desain, arsitektur, dan fase kerja.

## Struktur

```
crates/
  mb-poc/       POC fase 1: validasi hakodb wave-1 API + embedder lokal (SELESAI)
  mb-embed/     embedder teks lokal: MiniLM 384d multibahasa via candle (pure Rust)
  mb-server/    server axum (fase 2)
plugins/        plugin OpenCode TS (fase 3)
```

## Status

- [x] Inisialisasi repo + dokumen kerja
- [x] POC: hakodb 0.20.8 (hybrid_search, subgraph_json, graph_pagerank)
- [x] POC: embedder MiniLM lokal (candle pure-Rust, mean pooling)
- [x] Server axum inti (teruji): multi-project store, /v1/*, memory CRUD
- [x] Server: lazy-open project (robust restart), request logging
- [x] Server: indexer tree-sitter (simbol + call-graph, lookup) — tervalidasi
- [x] Server: auto-indexing watcher (notify, debounce, re-scan per file) — tervalidasi
- [x] Plugin OpenCode V2 (teruji): hooks context/prompt/compaction + tools memory.*
- [x] Plugin: skill learner (distilasi → mem_procedural; loop tervalidasi,
      generate.text terblokir kuota model gratis — blocker lingkungan)
- [x] Server: pembersihan simbol/edge stale + normalisasi rel watcher
- [x] MCP adapter tipis (mb-mcp, stdio)
- [x] Evaluasi penghematan token vs baseline (scripts/eval-token.py)

## Quickstart

```bash
# 1. build (Rust stable; tanpa node/npm)
cargo build --release -p mb-server

# 2. jalankan server (bind 127.0.0.1:7412)
MEMORYBOX_DATA=~/.memory-box ./target/release/mb-server

# 3. daftarkan project & index codebase
curl -s -X POST http://127.0.0.1:7412/v1/projects/open \
  -H 'content-type: application/json' -d '{"path":"/path/ke/codebase"}'
curl -s -X POST http://127.0.0.1:7412/v1/index/scan \
  -H 'content-type: application/json' \
  -d '{"project_id":"<12-hex dari respons>","path":"/path/ke/codebase"}'

# 4. cari konteks
curl -s -X POST http://127.0.0.1:7412/v1/retrieve \
  -H 'content-type: application/json' \
  -d '{"project_id":"<id>","query":"bagaimana watcher bekerja","k":5}'
```

Pasang plugin OpenCode dan/atau MCP adapter sesuai [DEPLOY.md](DEPLOY.md).
Contoh konfigurasi plugin ada di [opencode.json](opencode.json) dan
[plugins/memory-box/README.md](plugins/memory-box/README.md).

## Konfigurasi

| Env | Default | Fungsi |
|---|---|---|
| `MEMORYBOX_DATA` | `~/.memory-box` | root data per project |
| `MEMORYBOX_PORT` | `7412` | port server (localhost saja) |
| `MEMORYBOX_MODEL` | `multi` | `multi`, `en`, atau repo-id HF |
| `MEMORYBOX_MODEL_CACHE` | `~/.cache/huggingface/hub` | cache model lokal |
| `MEMORYBOX_WATCH` | aktif | `0` = matikan auto-index watcher |
| `MEMORYBOX_URL` | `http://127.0.0.1:7412` | URL server (plugin/MCP) |
| `MEMORYBOX_PROJECT` | direktori sesi | path codebase untuk plugin/MCP |

Server **tanpa autentikasi** — jangan diekspos langsung ke jaringan.

## Endpoint utama

| Method | Path | Fungsi |
|---|---|---|
| GET | `/healthz` | health check |
| POST | `/v1/projects/open` | buka/buat project (id = SHA-256 path) |
| POST | `/v1/index/scan` | index file/direktori (chunk + simbol + edge) |
| POST | `/v1/index/remove` | hapus index milik satu file |
| GET | `/v1/index/status` | status index per project |
| POST | `/v1/retrieve` | hybrid retrieval kode (FTS + vektor + RRF) |
| POST | `/v1/memory/write` | tulis memory episodic/semantic/procedural |
| POST | `/v1/memory/search` | cari memory (semua kind atau tertentu) |
| POST | `/v1/memory/forget` | hapus memory by id |
| POST | `/v1/symbols/lookup` | lookup simbol by nama |
| POST | `/v1/graph/context` | subgraph sekitar simbol |

## Pengujian

```bash
cargo test --workspace
bash scripts/test-distill-e2e.sh   # self-learning tanpa kuota LLM (mock)
python3 scripts/eval-token.py      # eval hemat token vs grep/read
```

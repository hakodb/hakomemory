# memory-box

A centralized RAG server for OpenCode: prolonged memory, live context,
codebase memory, and self-learning skills — built on HakoDB v0.20.8
(vector HNSW + graph, hybrid RRF, GraphRAG primitives) with a local
multilingual MiniLM embedding (384d, `paraphrase-multilingual-MiniLM-L12-v2`,
pure Rust via candle). No cloud embedding API, no Node/npm at runtime.

See [docs/PLAN.md](docs/PLAN.md) for design decisions, architecture, and the
work log. Deployment instructions are in [DEPLOY.md](DEPLOY.md).

## Layout

```
crates/
  mb-poc/       Phase-1 POC: HakoDB wave-1 API + local embedder (done)
  mb-embed/     local text embedding: multilingual MiniLM 384d (candle, pure Rust)
  mb-server/    axum server + mb-mcp stdio MCP adapter
plugins/
  memory-box/   OpenCode V2 plugin (TypeScript, no npm needed)
.opencode/
  skills/memory-box/  skill that teaches agents when/how to use the tools
  agents/memory-box.md read-only codebase-analysis subagent
AGENTS.md       repository instructions for any coding agent
scripts/        token evaluation + distill end-to-end test
docs/           design log + auth assessment
```

## Status

- [x] Repo init, MIT license, design log
- [x] POC: HakoDB v0.20.8 (`hybrid_search`, `subgraph_json`, `graph_pagerank`)
- [x] Local MiniLM embedder (candle, mean pooling + L2 normalization)
- [x] Core axum server: multi-project store, `/v1/*`, memory CRUD
- [x] Lazy project open (restart-safe), request logging
- [x] Tree-sitter indexer: symbols + call-graph + symbol lookup
- [x] Auto-indexing watcher (notify, 1.5 s debounce, per-file rescan)
- [x] OpenCode V2 plugin: `context` / `prompt` / `compaction` hooks + `memory.*` tools
- [x] Skill learner: distill → `mem_procedural`, injected into the next session
      (LLM call itself was blocked by free-tier quota in this environment)
- [x] Stale symbol/edge cleanup + project-root-relative paths
- [x] Cross-file edge re-resolution (follows moved/removed callees)
- [x] Thin MCP adapter (`mb-mcp`, stdio JSON-RPC)
- [x] Token-saving evaluation vs grep/read ([scripts/eval-token.py](scripts/eval-token.py))
- [x] Automated integration tests for all `/v1/*` endpoints
- [x] Agent guidance: skill, read-only subagent, `AGENTS.md`
- [x] Deployment guide ([DEPLOY.md](DEPLOY.md))

## Quickstart

```bash
# 1. build (Rust stable; no node/npm)
cargo build --release -p mb-server

# 2. run the server (binds 127.0.0.1:7412)
MEMORYBOX_DATA=~/.memory-box ./target/release/mb-server

# 3. open & index a codebase
curl -s -X POST http://127.0.0.1:7412/v1/projects/open \
  -H 'content-type: application/json' -d '{"path":"/path/to/codebase"}'
curl -s -X POST http://127.0.0.1:7412/v1/index/scan \
  -H 'content-type: application/json' \
  -d '{"project_id":"<12-hex id from above>","path":"/path/to/codebase"}'

# 4. retrieve context
curl -s -X POST http://127.0.0.1:7412/v1/retrieve \
  -H 'content-type: application/json' \
  -d '{"project_id":"<id>","query":"how does the watcher work","k":5}'
```

Install the OpenCode plugin and/or the MCP adapter as described in
[DEPLOY.md](DEPLOY.md). Example plugin config: [opencode.json](opencode.json)
and [plugins/memory-box/README.md](plugins/memory-box/README.md).

## Configuration

| Env | Default | Purpose |
|---|---|---|
| `MEMORYBOX_DATA` | `~/.memory-box` | data root (one subdir per project) |
| `MEMORYBOX_PORT` | `7412` | server port (localhost only) |
| `MEMORYBOX_MODEL` | `multi` | `multi`, `en`, or a Hugging Face repo id |
| `MEMORYBOX_MODEL_CACHE` | `~/.cache/huggingface/hub` | local model cache |
| `MEMORYBOX_WATCH` | enabled | `0` disables the auto-index watcher |
| `MEMORYBOX_AUTH_SECRET` | unset | HMAC secret; unset = legacy open mode |
| `MEMORYBOX_TOKEN` | unset | token used by plugin / MCP clients |
| `MEMORYBOX_ALIAS` | unset | project alias sent by plugin / MCP |
| `MEMORYBOX_URL` | `http://127.0.0.1:7412` | server URL (plugin / MCP) |
| `MEMORYBOX_PROJECT` | session directory | codebase path (plugin / MCP) |

Set `MEMORYBOX_AUTH_SECRET` for multi-user deployments and mint per-user
tokens with `mb-token`. Without it the server keeps the legacy open/local
behavior. See [docs/AUTH.md](docs/AUTH.md) for the isolation design.

## API

| Method | Path | Purpose |
|---|---|---|
| GET | `/healthz` | health check |
| POST | `/v1/projects/open` | open/create project (id = SHA-256 of path) |
| POST | `/v1/index/scan` | index a file/directory (chunks + symbols + edges) |
| POST | `/v1/index/remove` | remove the index of one file |
| GET | `/v1/index/status` | index status for a project |
| POST | `/v1/retrieve` | hybrid code retrieval (FTS + vector + RRF) |
| POST | `/v1/memory/write` | write episodic/semantic/procedural memory |
| POST | `/v1/memory/search` | search memory (all kinds or one kind) |
| POST | `/v1/memory/forget` | delete memory by id |
| POST | `/v1/symbols/lookup` | exact symbol lookup by name |
| POST | `/v1/graph/context` | subgraph context around a symbol |

## Testing

```bash
cargo test --workspace                 # unit + /v1/* integration tests (mock embedder, offline)
bash scripts/test-distill-e2e.sh       # self-learning chain with a mock distiller (no LLM quota)
python3 scripts/eval-token.py          # token savings vs grep/read
```

Integration tests live in
[crates/mb-server/tests/api.rs](crates/mb-server/tests/api.rs) and use a
deterministic mock embedder, so CI needs neither network nor the 470 MB model.

## License

MIT — see [LICENSE](LICENSE).

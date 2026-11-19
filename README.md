# memory-box

A centralized RAG server for OpenCode: prolonged memory, live context,
codebase memory, and self-learning skills — built on HakoDB v0.21.0
(vector HNSW + graph, hybrid RRF, GraphRAG primitives) with a local
multilingual MiniLM embedding (384d, `paraphrase-multilingual-MiniLM-L12-v2`,
pure Rust via candle). No cloud embedding API, no Node/npm at runtime.

See [docs/PLAN.md](docs/PLAN.md) for design decisions, architecture, and the
work log. HakoDB v0.21.0 adoption (issue #217) is tracked in
[docs/V021_ADOPTION.md](docs/V021_ADOPTION.md). Deployment instructions are in [DEPLOY.md](DEPLOY.md).

## Layout

```
crates/
  hakomemory-core/      library facade: Engine/EngineConfig + engine modules (no server, no env)
  hakomemory-poc/       Phase-1 POC: HakoDB wave-1 API + local embedder (done)
  hakomemory-embed/     local text embedding: multilingual MiniLM 384d (candle, pure Rust)
  hakomemory-server/    axum server + hakomemory-mcp stdio MCP adapter
  hakomemory-embed-adapter/  internal HTTP -> Cohere/OpenAI/Gemini embedding bridge
plugins/
  memory-box/   OpenCode V2 plugin (TypeScript, no npm needed)
.opencode/
  skills/memory-box/  skill that teaches agents when/how to use the tools
  agents/memory-box.md read-only codebase-analysis subagent
AGENTS.md       repository instructions for any coding agent
scripts/        token evaluation + distill end-to-end test
docs/           design log + auth assessment
```

## Library use (no server)

```rust
use hakomemory_core::embed::SharedEmbedder;
use hakomemory_core::{Engine, EngineConfig};

let embedder = std::sync::Arc::new(SharedEmbedder::mock(384)); // or your own backend
let engine = Engine::open(EngineConfig::new("./data", embedder))?;
let project = engine.project("/absolute/project/root")?;
let report = project.scan(std::path::Path::new("/absolute/project/root"))?;
let hits = project.retrieve("how does hybrid search work?", 8, 60)?;
```

`EngineConfig` carries data dir, embedder, embedding spec and LRU limits; no
`MEMORYBOX_*` env var is read by the library. The HTTP server is a thin adapter
that shares the same `ProjectStore` and modules through `hakomemory-core`.

Document endpoints (all under `/v1`): `documents/upsert`, `documents/batch`,
`documents/get`, `documents/list`, `documents/delete`, `documents/scan`,
`documents/search` and `admin/migrate`. `GET /v1/documents/get` returns chunks
with a `citation` object; `documents/scan` skips unchanged files via the
`files` manifest. `documents/search` supports `vector`, `fts` and `hybrid`
modes, pushes filters into the engine before candidate truncation, and applies
`budget_tokens` before returning hits.

Generic graph endpoints: `/v1/graph/rank`, `/community`, `/components`,
`/degrees`, `/path`, `/subgraph`, `/cycle`, `/topo` and `/export`. All accept
`collection`, `seed`, `via`, `direction`, `max_depth`, `limit`, `projection`
(and `damping`/`top` for PageRank). Responses are normalized to
`{entities, relations}` with references resolved server-side; no raw
`__ref__` objects reach clients.

## Status

- [x] Repo init, MIT license, design log
- [x] POC: HakoDB v0.21.0 (`hybrid_search_filtered`, `subgraph_json_projected`, `graph_pagerank`)
- [x] Local MiniLM embedder (candle, mean pooling + L2 normalization)
- [x] Core axum server: multi-project store, `/v1/*`, memory CRUD
- [x] Generic documents: Markdown frontmatter, header/fixed chunking, `files`
      manifest for idempotent reindex, citations (`source_uri`, heading, lines)
- [x] Lazy project open (restart-safe), request logging
- [x] Tree-sitter indexer: symbols + call-graph + symbol lookup
- [x] Auto-indexing watcher (notify, 1.5 s debounce, per-file rescan)
- [x] OpenCode V2 plugin: `context` / `prompt` / `compaction` hooks + `memory.*` tools
- [x] Skill learner: distill → `mem_procedural`, injected into the next session
      (LLM call itself was blocked by free-tier quota in this environment)
- [x] Stale symbol/edge cleanup + project-root-relative paths
- [x] Cross-file edge re-resolution (follows moved/removed callees)
- [x] Thin MCP adapter (`hakomemory-mcp`, stdio JSON-RPC)
- [x] Token-saving evaluation vs grep/read ([scripts/eval-token.py](scripts/eval-token.py))
- [x] Automated integration tests for all `/v1/*` endpoints
- [x] Multi-user auth: signed tokens, project/path scope, per-user memory,
      alias, backfill, revocation, rate limit
- [x] Multi-agent scope: `agent_id`/`session_id` filters + scoped tokens
- [x] Safe LRU: `open_exclusive`, Weak watcher, idle TTL + strong-count guard
- [x] Model-agnostic embeddings: persisted `EmbeddingSpec` (384–4096d), remote
      HTTP embedder backend, hard error on dimension mismatch
- [x] Vendor adapter: `hakomemory-embed-adapter` bridges internal HTTP to Cohere,
      OpenAI, Gemini, or NVIDIA NIM with query/document roles
- [x] `hakomemory-embed-srv`: local MiniLM exposed over internal HTTP for cross-box deploy
- [x] Agent guidance: skill, read-only subagent, `AGENTS.md`
- [x] Deployment guide ([DEPLOY.md](DEPLOY.md))

## Quickstart

```bash
# 1. build (Rust stable; no node/npm)
cargo build --release -p hakomemory-server

# 2. run the server (binds 127.0.0.1:7412)
MEMORYBOX_DATA=~/.memory-box ./target/release/hakomemory-server

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
| `MEMORYBOX_RATE_PER_MIN` | `0` | per-user request limit (0 = disabled) |
| `MEMORYBOX_EMBED_URL` | unset | HTTP embedder base URL (unset = local candle) |
| `MEMORYBOX_EMBED_DIM` | required if URL | vector dimension of the HTTP embedder |
| `MEMORYBOX_EMBED_TOKEN` | required for adapter | bearer token for the HTTP embedder; adapter refuses to start without it |
| `MEMORYBOX_EMBED_MODEL` | `http-embedder` | model id recorded in project spec |
| `MEMORYBOX_EMBED_HOST` | `127.0.0.1` | `hakomemory-embed-srv` bind host |
| `MEMORYBOX_EMBED_PORT` | `7416` | `hakomemory-embed-srv` bind port |
| `MEMORYBOX_EMBED_TOKEN` | unset (required for adapter) | shared token between server and embedder |
| `MEMORYBOX_AGENT_ID` | `opencode` | agent id sent by plugin/MCP on scoped writes |
| `MEMORYBOX_SESSION_ID` | generated | session id sent by plugin/MCP on scoped writes |
| `MEMORYBOX_ADAPTER_PROVIDER` | — | adapter: `cohere\|openai\|gemini\|nim` |
| `MEMORYBOX_ADAPTER_API_KEY` | — | vendor API key (adapter process only) |
| `MEMORYBOX_ADAPTER_DIM` | `384` | output dimension |
| `MEMORYBOX_ADAPTER_MODEL` | per provider | vendor model id |
| `MEMORYBOX_ADAPTER_BASE_URL` | vendor default | override/proxy URL |
| `MEMORYBOX_MAX_PROJECTS` | `8` | max open Hako instances (0 = no eviction) |
| `MEMORYBOX_PROJECT_IDLE_SECS` | `1800` | min idle before LRU eviction |
| `MEMORYBOX_URL` | `http://127.0.0.1:7412` | server URL (plugin / MCP) |
| `MEMORYBOX_PROJECT` | session directory | codebase path (plugin / MCP) |

Set `MEMORYBOX_AUTH_SECRET` for multi-user deployments and mint per-user
tokens with `hakomemory-token`. Without it the server keeps the legacy open/local
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
[crates/hakomemory-server/tests/api.rs](crates/hakomemory-server/tests/api.rs) and use a
deterministic mock embedder, so CI needs neither network nor the 470 MB model.

## License

MIT — see [LICENSE](LICENSE).

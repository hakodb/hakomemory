# API

## HTTP (server adapter)

All `/v1/*` routes (except `/healthz`) require a bearer token when
`MEMORYBOX_AUTH_SECRET` is set. Errors use one envelope:

```json
{
  "error": {
    "code": "E_INPUT | E_AUTH | E_NOT_FOUND | E_STORAGE | E_RATE | E_EMBED | E_SCHEMA",
    "message": "human readable detail",
    "request_id": "hex timestamp-counter"
  }
}
```

`429` responses also carry `Retry-After: 60`.

### Projects & indexing
- `POST /v1/projects/open` — `path` or `alias` → `project_id`
- `POST /v1/index/scan`, `/v1/index/remove`, `GET /v1/index/status`
- `GET /v1/symbols/lookup`

### Memory
- `POST /v1/memory/write` — `wait_for_index: true` makes the response wait
  for index/WAL settle (read-after-write); default false for latency

### Documents
- `POST /v1/documents/upsert` / `/v1/documents/batch`
- `GET /v1/documents/get?project_id=&source_uri=`
- `GET /v1/documents/list?project_id=&collection=&limit=`
- `POST /v1/documents/delete` / `/v1/documents/scan`
- `POST /v1/documents/search` — `mode` (`vector|fts|hybrid|hybridgraph`),
  `filters`, `k`, `budget_tokens`, `citations`, `expand_parent`, `graph_via`,
  `graph_hops`
- `POST /v1/admin/migrate` — `dry_run`

### Graph
- `POST /v1/graph/context` (legacy) and `/v1/graph/rank`, `/community`,
  `/components`, `/degrees`, `/path`, `/subgraph`, `/cycle`, `/topo`, `/export`
- All accept `collection`, `seed`, `via`, `direction`, `max_depth`, `limit`,
  `projection` (`damping`/`top` for rank). Responses contain `entities` and
  `relations` with references resolved.

Machine-readable spec: [`openapi.json`](openapi.json).

## Configuration

Optional JSON file (`hakomemory.json` in the data dir, or `MEMORYBOX_CONFIG`
path) with env overrides:

```json
{
  "provider": "local",
  "model": "intfloat/multilingual-e5-small",
  "dim": 384,
  "metric": "Cosine",
  "max_projects": 8,
  "idle_ttl_secs": 1800
}
```

Env overrides: `MEMORYBOX_MODEL`, `MEMORYBOX_EMBED_URL`,
`MEMORYBOX_EMBED_DIM`, `MEMORYBOX_EMBED_MODEL`, `MEMORYBOX_MODEL_CACHE`,
`MEMORYBOX_MAX_PROJECTS`, `MEMORYBOX_PROJECT_IDLE_SECS`.

## Rust library (non-OpenCode example)

```rust
use hakomemory_core::embed::SharedEmbedder;
use hakomemory_core::{Engine, EngineConfig};
use std::sync::Arc;

let embedder = Arc::new(SharedEmbedder::mock(384));
let engine = Engine::open(EngineConfig::new("./data", embedder))?;
let project = engine.project("/absolute/project/root")?;
let hits = project.retrieve("hybrid search", 8, 60)?;
```

# memory-box

RAG server untuk OpenCode: prolonged memory, live context, codebase memory,
dan self-learning skill — di atas HakoDB v0.20.6 (vector HNSW + graph, hybrid
RRF, GraphRAG primitives) dengan embedding lokal MiniLM 384d.

Baca [PLAN.md](PLAN.md) untuk keputusan desain, arsitektur, dan fase kerja.

## Struktur

```
crates/
  mb-poc/       POC fase 1: validasi hakodb wave-1 API + embedder lokal
  mb-server/    server axum (fase 2)
plugins/        plugin OpenCode TS (fase 3)
```

## Status

- [x] Inisialisasi repo + dokumen kerja
- [ ] POC: hakodb 0.20.6 (hybrid_search, subgraph_json, graph_pagerank)
- [ ] POC: embedder MiniLM lokal (fastembed-rs / ort)
- [ ] Server axum + endpoint /v1
- [ ] Plugin OpenCode
- [ ] Skill learner + MCP adapter

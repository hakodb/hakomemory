# memory-box

RAG server untuk OpenCode: prolonged memory, live context, codebase memory,
dan self-learning skill — di atas HakoDB v0.20.6 (vector HNSW + graph, hybrid
RRF, GraphRAG primitives) dengan embedding lokal MiniLM 384d.

Baca [PLAN.md](PLAN.md) untuk keputusan desain, arsitektur, dan fase kerja.

## Struktur

```
crates/
  mb-poc/       POC fase 1: validasi hakodb wave-1 API + embedder lokal (SELESAI)
  mb-embed/     embedder teks lokal: MiniLM-L6 384d via candle (pure Rust)
  mb-server/    server axum (fase 2)
plugins/        plugin OpenCode TS (fase 3)
```

## Status

- [x] Inisialisasi repo + dokumen kerja
- [x] POC: hakodb 0.20.6 (hybrid_search, subgraph_json, graph_pagerank)
- [x] POC: embedder MiniLM lokal (candle pure-Rust, mean pooling)- [x] Server axum inti (teruji): multi-project store, /v1/*, memory CRUD
- [x] Server: lazy-open project (robust restart), request logging
- [x] Plugin OpenCode V2 (teruji): hooks context/prompt/compaction + tools memory.*
- [ ] Server: indexer tree-sitter (simbol + call-graph), WS progress
- [ ] Server: skill learner (ctx.experimental.generate.text → procedural)
- [ ] MCP adapter tipis
- [ ] Evaluasi penghematan token vs baseline

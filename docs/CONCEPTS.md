# Concepts

HakoMemory is a local-first embedded knowledge engine. One **project** is one
HakoDB directory derived from the canonical path of the indexed root
(`pid = sha256(path)[..12]`). A single writer owns that directory via
`Hako::open_exclusive`; use the daemon for multi-process access.

## Three modalities in one project

- **Memory**: `mem_episodic`, `mem_semantic`, `mem_procedural` with owner/agent/
  session scope, FTS + vector indexes.
- **Documents**: `documents`, `chunks`, `files`. Chunks carry `source_uri`,
  `heading_path`, `start_line`, `end_line` and a `files` manifest for
  idempotent reindexing.
- **Graph**: `symbols`/`edges` (call graph) plus generic `entities`/`relations`
  via caller-supplied `collection`/`via`. Traversal and analytics use HakoDB's
  Rust API; responses are normalized to `{entities, relations}`.

## Retrieval

`documents/search` composes `vector`, `fts` and `hybrid` modes. Filters are
pushed into the engine before candidate truncation; `budget_tokens` is applied
after ranking. Every hit can carry a citation (`document_id`, `source_uri`,
heading, line range).

## Embedder

Embedding is injected: the library accepts any `SharedEmbedder` (local candle,
HTTP adapter, or deterministic mock used in tests). `EmbeddingSpec`
(`model_id`, `provider`, `dim`, `metric`) is persisted per project; changing
any of the four fields requires explicit migration/re-embed.

## Library first

`hakomemory-core` is usable without the HTTP server and without any
`MEMORYBOX_*` env var. The axum server is an adapter over the same modules.
See `README.md` for the `Engine::open(EngineConfig)` example.

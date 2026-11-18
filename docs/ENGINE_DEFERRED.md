# Engine items deliberately deferred — final memory-box decisions

Source: hakodb issue #217 (the P0-P2 wave is already adopted; see
[docs/V021_ADOPTION.md](V021_ADOPTION.md)). This file closes out the items
that memory-box intentionally does **not** implement, with the reason and
the trigger for revisiting each one.

| Upstream item | Status in memory-box | Decision | Revisit trigger |
|---|---|---|---|
| `update_where` + `query_stream` | The `user_id` backfill uses `Query::new(col).limit(100k)` + a `put` loop (admin endpoint). | **Not adopted.** Sufficient for the current scale (hundreds of docs per project). | Backfill/migration beyond ~100k docs per project, or WAL growth during long scans -> need the atomic-aware API + cursors. |
| `stats()` -> `EngineStats` | The LRU relies on `active_count()`/registry plus its own `MEMORYBOX_MAX_PROJECTS` / idle TTL. | **Not adopted.** No memory-based eviction decision yet (HNSW/WAL footprints). | LRU active with many projects and the threshold needs to be picked from real engine metrics. |
| Edge-doc normalization / `relationships[]` | The OpenCode plugin and `hakomemory-mcp` already deref `_from`/`_to` two layers deep; `subgraph_json_projected` is in use. | **Not adopted.** No real user pain today. | Next plugin/MCP revision (paired with the subgraph-projection adoption). |
| Rich `FilterExpr` (AND/OR across query/hybrid/traverse) | Filters in use: `user_id` Eq + `agent_id`/`session_id` Eq/In through `hybrid_search_filtered`. | **Not adopted.** Sufficient for the multi-user + multi-agent scope. | Traversal needs compound predicates that current field filters cannot express. |
| Selectivity qps bench (10/50/90%) | None. | **Not adopted** — it belongs in bench-lab/hako-backend, not this repo. | A filtered-ANN performance decision is waiting on the numbers. |
| FFI mirrors for P0-A/B/P1/P2 | None. | **Not adopted.** memory-box is an in-process Rust consumer; the bindings live in their own repos. | A C/NIM consumer actually needs them. |

## Conclusion

Every P4 item stays **deferred in upstream issue #217**, and memory-box
deliberately does not mirror them. This is not blocking technical debt — each
entry has an explicit trigger above. When a trigger fires, do the work in
hakodb first, then adopt it here, following the same pattern as the
v0.20.8 -> v0.21.0 adoption.

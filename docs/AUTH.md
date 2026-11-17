# Authentication assessment (memory-box server)

## Current state

- The server binds **`127.0.0.1` only** (`crates/mb-server/src/main.rs`).
- There is **no authentication or authorization** on any route. The only
  middleware is request logging (method + path + status + duration; no headers
  or bodies are logged).
- All `/v1/*` endpoints accept a `project_id` supplied by the caller. The
  `projects/open` and `index/scan` endpoints accept an arbitrary filesystem
  path supplied by the caller.

So the current security boundary is the host itself: any local process (or any
user able to reach the loopback port) has full access.

## Risks if the server is exposed beyond localhost

| Risk | Why it matters |
|---|---|
| Arbitrary path indexing | A caller can `projects/open` + `index/scan` any path readable by the server user, then `retrieve` snippets of it — an information-disclosure path. |
| Cross-project memory access | `memory/write`, `memory/search`, `memory/forget` trust the supplied `project_id`; there is no ownership check. |
| No rate limiting | A caller can saturate CPU with retrieval / indexing requests. |
| Plain HTTP | Bearer tokens or payloads sent over a LAN would be in cleartext without TLS. |

## Options

### 1. Keep loopback-only + SSH tunnel / local proxy (single-user only)

No code changes. Run the server as today, and for remote access use:

```bash
ssh -L 7412:127.0.0.1:7412 user@box
```

The plugin and `mb-mcp` then keep using `http://127.0.0.1:7412`. This preserves
the existing threat model and is enough for **one trusted user**, but it does
not identify users and is therefore not sufficient for the multi-user target.

### 2. Optional static bearer token (small, backwards-compatible change)

Add an env `MEMORYBOX_TOKEN`. When it is **unset**, behavior stays identical
(no auth); when set, a middleware rejects requests to `/v1/*` unless
`Authorization: Bearer <token>` matches (constant-time comparison). `/healthz`
stays public.

Sketch (to be placed before `.with_state(...)` in `router()`):

```rust
.axum::middleware::from_fn(|req, next| async move {
    let expected = std::env::var("MEMORYBOX_TOKEN").ok();
    if let Some(expected) = expected {
        let ok = req.headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map_or(false, |got| {
                // constant-time compare; identical length check first
                got.len() == expected.len()
                    && got.bytes().zip(expected.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
            });
        if !ok {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
    }
    next.run(req).await
})
```

Client changes needed alongside it:
- plugin `post()`: add `Authorization` header from `MEMORYBOX_TOKEN` or
  `options.token`;
- `mb-mcp`: add the same header in `http_post_json()` from
  `MEMORYBOX_TOKEN`;
- docs: README, DEPLOY.md, plugin README.

Trade-offs: simple and explicit, but the token is a shared secret in env, is
not scoped per project, and **still requires TLS or a tunnel** on the wire.

### 3. Reverse proxy with TLS + auth (recommended for LAN / multi-user)

Terminate TLS and authenticate at Caddy/nginx (basic auth, mTLS, or OIDC),
then proxy to `127.0.0.1:7412`. No application code changes; keeps the
loopback-only guarantee on the server itself. This is the best fit if the
server must be reachable from other machines.

### 4. Scoped/per-project tokens (larger change)

Issue an HMAC-signed token containing allowed project ids (`sub`, `projects`,
`exp`) and check it against the `project_id` in each request. This is the
recommended app-level direction for the multi-user target (see the isolation
model below), not an optional extra.

## Data isolation: what exists today

- **Physical isolation per project**: every project has its own directory
  `~/.memory-box/projects/<pid>/` with its own `chunks`, `symbols`, `edges`,
  `mem_*` and `meta` collections. `pid` is the first 12 hex of SHA-256 of the
  **canonical path** (`store.rs`). Two different paths → two different DBs.
- **But `pid` is not a user boundary.** It is derived from a path and supplied
  by the caller; no user/owner is attached to it. Two users pointing at the
  same path get the **same pid and the same memory**. Any caller can request any
  existing `pid` because there is no authorization check.
- **Memory collections are project-scoped, not user/agent-scoped.** Docs have
  no `user_id`, `agent_id`, or `session_id` field. The `prompt` hook writes
  every user prompt into one shared `mem_episodic`; the `context` hook injects
  the same lessons into every session bound to that project.
- A single shared `SharedEmbedder` (behind a `Mutex`) and one watcher per `pid`
  serve all callers — safe from data races, but not an isolation boundary.

So today's isolation is **"one DB per project path"**, not "one DB per user".
That is enough for multiple projects on one trusted machine, **not** for
multiple users or multiple independent agents.

## Target isolation model for multi-user / multi-agent

Recommended design (not yet implemented):

1. **Authentication (who)**: per-user signed token (HMAC/JWT) with `sub`,
   allowed `projects` or path `roots`, and `exp`. A single shared
   `MEMORYBOX_TOKEN` is **not sufficient** — it identifies the bearer, not the
   user. If an OIDC reverse proxy is used, it must forward a signed identity
   header and the server must trust only that proxy.
2. **Authorization (what)**: every handler resolves `pid` and checks it against
   the token's allowed set before touching the DB. Do not trust a client-supplied
   `project_id` alone. `GET /v1/index/status` carries it in the query string;
   POST routes carry it in the body — both need the same check.
3. **`projects/open`**: the client sends a project alias; the server validates
   the path against that user's allowed roots and computes `pid` **server-side**.
   This closes the arbitrary-path indexing hole.
4. **Memory scoping**: add `user_id` and optional `agent_id` / `session_id` to
   documents in `mem_*`. Writes stamp them from the auth context; searches and
   forgets filter by them automatically. Legacy docs need a backfill (assign an
   owner) or an explicit decision to remain project-shared.
5. **Codebase index**: keep it shared per project — code does not belong to a
   user. Gate `retrieve`, `symbols/lookup`, and `graph/context` by the same
   project authorization. Two users on the same repo share the index but not
   their memories.
6. **Clients**: plugin `post()` and `mb-mcp` send the token header from an
   env/option (`MEMORYBOX_TOKEN` or per-user config). One OpenCode process /
   MCP process = one user identity; do not mix users in one process.
7. **Hard-isolation alternative**: one server process and data root per user.
   Simpler to reason about, heavier to operate; suitable when tenants must not
   share even the index or watcher threads.

Order of work: authN → authorize `project_id` → validate `projects/open` paths
→ add user fields + filters → client header support → migration of legacy docs.
Without steps 1–3, adding `user_id` fields alone gives **no real isolation**,
because a caller can still choose another user's `pid`.

**Status: design assessment only — no auth or isolation code has been added.**

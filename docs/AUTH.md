# Authentication assessment (memory-box server)

## Current state

- The server binds **`127.0.0.1` only** (`crates/hakomemory-server/src/main.rs`).
- Authentication is **implemented** via `MEMORYBOX_AUTH_SECRET` (HMAC bearer
  tokens). When the secret is unset the server is open on loopback; when set,
  all `/v1/*` routes except `/healthz` require a bearer token with scoped
  `roots`, `projects`, `agents` and `sessions` (`src/auth.rs`, `src/lib.rs`).
- Request logging records method + path + status + duration; headers and bodies
  are not logged.
- All `/v1/*` endpoints accept a `project_id` supplied by the caller and check
  it against the token's `projects`. `projects/open` and `index/scan` also
  validate filesystem paths against the token's `roots`.

So the security boundary is: loopback bind + HMAC token when `MEMORYBOX_AUTH_SECRET` is configured; without it, any local process can reach the API.

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

The plugin and `hakomemory-mcp` then keep using `http://127.0.0.1:7412`. This preserves
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
- `hakomemory-mcp`: add the same header in `http_post_json()` from
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
  same path get the **same pid and the same memory**. When auth is disabled,
  any caller can request any existing `pid`; with `MEMORYBOX_AUTH_SECRET` set,
  `authorize_project` checks the token's `projects`/`roots` first.
- **Memory collections are project-scoped.** Historically docs had no
  `user_id`/`agent_id`/`session_id` field; that is now fixed in the implemented
  sections below (stamping + filtering). The `prompt` hook still writes user
  prompts to `mem_episodic`, and `context` injects the same project lessons.
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
6. **Clients**: plugin `post()` and `hakomemory-mcp` send the token header from an
   env/option (`MEMORYBOX_TOKEN` or per-user config). One OpenCode process /
   MCP process = one user identity; do not mix users in one process.
7. **Hard-isolation alternative**: one server process and data root per user.
   Simpler to reason about, heavier to operate; suitable when tenants must not
   share even the index or watcher threads.

Order of work: authN → authorize `project_id` → validate `projects/open` paths
→ add user fields + filters → client header support → migration of legacy docs.
Without steps 1–3, adding `user_id` fields alone gives **no real isolation**,
because a caller can still choose another user's `pid`.

## Implemented status

Auth + isolation have been implemented in phases:

1. `auth.rs`: HMAC-signed token (`mb1.<payload>.<sig>`) with `sub`, `roots`,
   `projects`, `exp`, `admin`; `MEMORYBOX_AUTH_SECRET` enables auth (unset =
   legacy open mode). `hakomemory-token` mints tokens.
2. Middleware verifies the token; `/healthz` stays public. All `/v1/*`
   handlers call `state.authorize_project(&auth, pid)` and `projects/open`
   validates the path against the token roots.
3. `projects/open` accepts a per-user alias; the server computes `pid` and
   persists `project_aliases.json` (alias + pid→root map). Client cannot pick
   an arbitrary `pid`.
4. Memory docs are stamped with `user_id` (+ optional `agent_id`/`session_id`);
   search filters by owner **and** optional agent/session (Eq or token-scoped
   In), `write` requires an in-scope agent/session, and `forget` rejects
   another user's or another agent's doc. Legacy documents without `user_id`
   are hidden from non-admin users until stamped via
   `POST /v1/admin/backfill` (admin only).
5. Plugin and `hakomemory-mcp` send `Authorization: Bearer` from `MEMORYBOX_TOKEN`
   (or `options.token`) and can open by `MEMORYBOX_ALIAS` / `options.alias`.
   Tokens carry a `jti` and can be revoked with `POST /v1/admin/revoke`
   (rotation = mint a new token with `hakomemory-token`, then revoke the old one).
6. `MEMORYBOX_RATE_PER_MIN` sets a per-user fixed-window (60 s) limit;
   exceeding it returns HTTP 429. `/healthz` is exempt.

Tests: 9 auth integration tests + 5 endpoint tests + 4 MCP unit tests +
2 parser tests — all pass via `cargo test --workspace`.

Since HakoDB v0.21.0, owner filtering runs **in the engine** via
`hybrid_search_filtered(..., &[Filter { user_id }])`; the old k×5 over-fetch
workaround and its "fewer than k hits" caveat are gone. Graph responses use
`subgraph_json_projected`, so raw `emb` vectors never reach clients. See
`docs/V021_ADOPTION.md` and hakodb issue #217.


## Implemented: multi-agent scope (P3)

Tokens may carry `agents` and `sessions` claims (`hakomemory-token --agents a1,a2
--sessions s1`):

- token with `agents=[a1]` can only write with `agent_id=a1`, only sees `a1`
  memories when `agent_id` is omitted, and gets 403 for `agent_id=a2`;
- `sessions` behaves the same way;
- admin tokens bypass all scope filters;
- `user_id` ownership is always enforced for non-admin tokens.

Cross-agent isolation test lives in `tests/auth.rs::memory_scope_agent_dan_session`.

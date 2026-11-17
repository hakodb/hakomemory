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

### 1. Keep loopback-only + SSH tunnel / local proxy (recommended for single user)

No code changes. Run the server as today, and for remote access use:

```bash
ssh -L 7412:127.0.0.1:7412 user@box
```

The plugin and `mb-mcp` then keep using `http://127.0.0.1:7412`. This preserves
the existing threat model and is enough for the intended "one developer, one
box" use case.

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
`exp`) and check it against the `project_id` in each request body. Needed only
for a genuinely multi-tenant deployment; disproportionate for the current
single-user design.

## Recommendation

1. **Now**: keep loopback-only binding; document "no auth, do not expose".
   Use SSH tunneling for remote access (option 1).
2. **If remote over LAN is required**: option 3 (TLS + auth proxy), not a raw
   bearer token over plain HTTP.
3. **If an app-level switch is wanted later**: implement option 2 as an
   opt-in via `MEMORYBOX_TOKEN`, with the plugin and MCP header changes in the
   same commit. Keep it disabled by default to avoid breaking existing users.

Additional hardening worth doing regardless of auth: validate that
`projects/open` paths stay under an allowed root, and add a request-size /
rate limit for the indexing endpoints.

**Status: assessment only — no auth code has been added.**

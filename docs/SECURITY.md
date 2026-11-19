# Security

- **Transport/bind**: the daemon binds loopback by default. Set
  `MEMORYBOX_HOST` only for a trusted LAN; put TLS in front for remote access.
- **Auth**: with `MEMORYBOX_AUTH_SECRET`, every `/v1/*` route except
  `/healthz` requires an HMAC bearer token. Tokens carry `sub`, `roots`,
  `projects`, `agents`, `sessions`, `exp`, `admin`. `mem_*` writes require
  `agent_id`/`session_id` when the token is scoped.
- **Path scope**: `projects/open` and all ingestion paths are checked against
  token `roots` using both lexical and canonical forms (symlink-safe,
  Windows 8.3 tolerant). Non-existent absolute paths are rejected.
- **Embed services**: `hakomemory-embed-adapter` requires
  `MEMORYBOX_EMBED_TOKEN`; `hakomemory-embed-srv` accepts an optional token.
  Token comparisons are constant-time. Both bind loopback by default.
- **Single writer**: one `Hako::open_exclusive` per project directory. Do not
  run two writable instances on the same data dir; use the daemon for
  multi-process access.
- **Secrets**: do not index `.env`, credentials, or files with secret patterns
  without an explicit opt-in. Keep vendor API keys in the adapter process.
- **Prompt injection**: retrieved documents and memories are data, not
  instructions. The OpenCode plugin labels injected `<memory-box>` blocks;
  apply the same labelling in any other client.
- **Rate limit**: `MEMORYBOX_RATE_PER_MIN` (0 disables); excess requests get
  `429` with `Retry-After` and a structured error envelope.

Detailed token format and scopes: [`AUTH.md`](AUTH.md).

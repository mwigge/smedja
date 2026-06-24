## Context

smedja is an MCP client today. The relevant surface:

- `McpHttpClient` (`bin/smdjad/src/mcp_http.rs`): a JSON-RPC 2.0 client that POSTs `tools/list`/`tools/call` to an HTTP MCP server, attaching a static Bearer token when non-empty. The request/response shape is hand-rolled `serde_json::json!`, not the `smedja-rpc` types.
- `dispatch_mcp_tool` (`bin/smdjad/src/executor/mod.rs:459`): for any tool name not in `LOCAL_TOOLS`, looks up `ingot.find_mcp_server_for_tool(tool_name)`, builds `McpHttpClient::new(&server.url, "")` (empty token), and calls it. The `transport` field on `McpServer` is ignored.
- `LOCAL_TOOLS` (`executor/mod.rs:32`): `bash`, `run_command`, `read_file`, `write_file`, `edit_file`, `list_files`, `smedja_vault_search`, `smedja_vault_store`, `smedja_retrieve`, `graph_query`, `otel_query`, `metric_query`, `log_tail`. `execute_tool` dispatches these natively and enforces a least-privilege guard: `WRITE_TOOLS` (`edit_file`, `bash`, `write_file`, `run_command`) are blocked when `session.mode == Some("review")`.
- `mcp_oauth.rs`: `Token { access_token, token_type, refresh_token, expires_in }`; `TokenStore` persists tokens as JSON under `$XDG_CONFIG_HOME/smedja/<sha256-prefix>.json` at mode 0600 (atomic, UNIX). `start_pkce` returns `Err(PkceError::NotImplemented)`. `PkceError` already has `Http`, `Storage`, `Cancelled`, `NotImplemented` variants.
- `handlers/mcp.rs`: `register` (validates `is_safe_mcp_url`, stores `transport` defaulting to `"http"`), `list`, `remove`, `refresh` (rebuilds an HTTP client per server with an empty token).
- `acp.rs`: `submit_prompt` (`acp.rs:107`) creates a queued `Task`, briefly starts a `TurnHandle` so `spawn_worker` picks up the turn, and returns `{turn_id, session_id}` for polling. `AcpState` holds the `Ingot` and an `Arc<Dispatcher>`.
- `smedja-rpc`: `Request { jsonrpc, id, params, method }`, `Response::{ok, err}`, `Error/RpcError`, `codes`. MCP is JSON-RPC 2.0, so these types are the canonical message envelope to reuse rather than re-deriving the wire shape.

## Goals / Non-Goals

Goals:
- Expose smedja's read-safe native tools as an MCP server (`tools/list`, `tools/call`).
- Implement the OAuth 2.0 Authorization Code + PKCE flow and authenticate outbound MCP calls with stored, refreshable tokens.
- Add a stdio (child-process) transport alongside HTTP, selected by `McpServer.transport`.
- Stream ACP turn events over SSE instead of requiring polling.

Non-Goals:
- Exposing write/exec tools (`bash`, `run_command`, `write_file`, `edit_file`) over server mode — sandboxing those is owned by the `exec-sandbox` change.
- Dynamic client registration (RFC 7591) or OAuth scopes negotiation — only the static-client PKCE flow.
- AES-256-GCM at-rest token encryption — `TokenStore` keeps the 0600-file confidentiality model already in place; encryption is a separate hardening item.
- A standalone `smedja mcp serve` binary on stdio — server mode is served on the existing daemon listener; a stdio-served smedja server can follow once the transport abstraction lands.
- WebSocket MCP transport.

## Decisions

**Decision: server mode exposes a read-safe subset, not all of `LOCAL_TOOLS`.**
Add `MCP_SERVER_TOOLS: &[&str]` = the read-only entries of `LOCAL_TOOLS`: `graph_query`, `read_file`, `list_files`, `smedja_vault_search`, `smedja_retrieve`, `otel_query`, `metric_query`, `log_tail`. The mutating/exec tools (`write_file`, `edit_file`, `bash`, `run_command`, `smedja_vault_store`) are excluded.
- Rationale: an MCP server gives arbitrary external clients tool access; only tools that cannot mutate the workspace or run shell commands are safe to share by default. This mirrors the existing `WRITE_TOOLS` review-mode guard — `tools/call` routes through `execute_tool` with an effective read-only session, so the guard is enforced a second time even if the subset list drifts.
- Alternative: expose everything and rely on caller trust. Rejected — violates least privilege; exec/write exposure is the `exec-sandbox` change's concern.

**Decision: the MCP server reuses `smedja-rpc` types and is served on the existing HTTP listener.**
`mcp_server.rs` parses an incoming `smedja_rpc::Request`, matches `method` (`initialize`, `tools/list`, `tools/call`), and replies with `Response::ok`/`Response::err`. It mounts as an axum route on the same listener that serves ACP, behind the same auth check.
- Rationale: MCP is JSON-RPC 2.0; `smedja-rpc` already models that envelope and is used across the daemon. Co-mounting on the ACP listener avoids a second bind/port and reuses `require_auth`.
- Alternative: a fresh JSON-RPC stack in `mcp_http.rs`. Rejected — duplicates the envelope and the listener.

**Decision: PKCE flow uses S256, an ephemeral localhost redirect listener, and a one-shot channel.**
`start_pkce(server_url)`:
1. Generate a 32-byte random `code_verifier` (base64url, no pad); derive `code_challenge = base64url(SHA256(verifier))` via the existing `sha2` dependency; generate a random `state`.
2. Bind a `tokio` TCP listener on `127.0.0.1:0`; the bound port forms the `redirect_uri` (`http://127.0.0.1:<port>/callback`).
3. Log the authorization URL (`tracing::info!`) for the operator to open — no auto-launch dependency.
4. Accept exactly one redirect request, validate `state`, extract `code`; deliver it over a `oneshot` channel; respond to the browser with a minimal "you may close this tab" body. Apply a wall-clock timeout → `PkceError::Cancelled`.
5. POST the token endpoint with `grant_type=authorization_code`, `code`, `code_verifier`, `redirect_uri`; map transport failures to `PkceError::Http`.
6. Persist the resulting `Token` via `TokenStore::save(server_url, &token)`; map save failures to `PkceError::Storage`. Return the `Token`.
- Rationale: PKCE (RFC 7636) is mandatory for public clients per the MCP auth spec; the localhost-loopback redirect is the standard native-app pattern (RFC 8252). The verifier never leaves the process; only the challenge is sent in the authorization request.
- Refresh: a separate `refresh_token(server_url, &token)` performs `grant_type=refresh_token` when `expires_in` indicates expiry, re-saving the new `Token`. Callers (`dispatch_mcp_tool`, `mcp.refresh`) load the stored token, refresh if needed, and pass `token.access_token` to `McpHttpClient`.
- Alternative: device-code flow. Rejected — loopback redirect is simpler for a local daemon with a browser available; device code can be added later for headless hosts.

**Decision: token store lookup is keyed by server URL; outbound calls become authenticated.**
`dispatch_mcp_tool` and `mcp.refresh` call `TokenStore::default_store().load(&server.url)`; on `Some(token)` they pass `token.access_token` to `McpHttpClient::new`, on `None` they fall back to the `MCP_TOKEN` env var, then to empty (current behaviour).
- Rationale: makes the existing static-token path a special case of the token store; no breaking change for unauthenticated servers.

**Decision: stdio transport spawns a child process and frames JSON-RPC over its pipes.**
`McpStdioClient` spawns the configured command (stored in the server's `url` field as a `cmd://`-style spec, or a dedicated command column — see tasks) via `tokio::process::Command` with piped stdin/stdout. It writes a `smedja_rpc::Request` as one line of JSON to stdin and reads one line of JSON from stdout per call (newline-delimited framing, the lowest-common-denominator MCP stdio framing). The child is spawned lazily on first call and held for the process lifetime; a `Drop`/teardown kills it. All I/O uses `tokio::process`/`tokio::io` — never blocking `std::io` inside async.
- Rationale: stdio is the dominant local MCP-server transport; child-process management is the only added lifecycle concern. A `McpTransport` enum (`Http(McpHttpClient)` | `Stdio(McpStdioClient)`) gives `dispatch_mcp_tool` a single dispatch point keyed by `server.transport`.
- Alternative: shell out per call. Rejected — re-spawning per tool call loses server-side session state and adds latency; a held child matches MCP stdio semantics.

**Decision: ACP `submit_prompt` returns an SSE stream subscribed to the dispatcher.**
Replace the `turn_id`-only JSON body with an axum `Sse` response. The handler still creates the queued `Task` and starts the `TurnHandle`, then subscribes to the dispatcher's event channel for that `turn_id` and forwards each `TurnEvent` (`Started`, deltas, tool calls, `Completed`/`Failed`) as an SSE `event`. The stream ends after the terminal event. A heartbeat keep-alive prevents idle-timeout disconnects.
- Rationale: the dispatcher already publishes the exact events the SSE stream needs; `submit_prompt` only needs to bridge that channel to the HTTP response. Polling remains available via the existing task/turn read paths for clients that do not consume SSE.
- Alternative: a separate `GET /acp/v1/session/{id}/events` SSE endpoint. Folded in: streaming directly from `submit_prompt` matches the ACP "prompt returns a stream" contract and avoids a race between task creation and subscription.

## Risks / Trade-offs

- [Risk] Server mode could expose a tool that mutates the workspace if `MCP_SERVER_TOOLS` drifts → Mitigation: `tools/call` routes through `execute_tool` with an effective `review`-mode session, so the existing `WRITE_TOOLS` guard rejects mutating tools regardless of the subset list; a test asserts a write tool is rejected over the server endpoint.
- [Risk] The PKCE redirect listener could accept a forged callback → Mitigation: validate the `state` parameter against the generated value before accepting the `code`; bind only to `127.0.0.1`; accept exactly one request then close.
- [Risk] A stored token outlives its validity and outbound calls 401 → Mitigation: `refresh_token` runs when `expires_in` indicates expiry; on refresh failure the call falls back to re-running `start_pkce` (logged), not a silent hang.
- [Risk] A stdio child process hangs or dies mid-call → Mitigation: per-call read timeout maps to a tool-error string (matching the HTTP error path); child death is detected and the next call re-spawns; teardown kills the child to avoid orphans.
- [Risk] SSE streaming changes the ACP `submit_prompt` response contract (BREAKING for polling-only clients) → Mitigation: SSE is additive over the same turn lifecycle; the turn is still recorded as a `Task`, so polling clients can read the result through the existing task read path; the README documents the new stream shape.
- [Risk] Co-mounting the MCP server on the ACP listener widens that port's attack surface → Mitigation: the MCP route sits behind the same `require_auth` check; unauthenticated requests are rejected before any tool dispatch.

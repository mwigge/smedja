## Why

smedja speaks the Model Context Protocol (MCP) only as a **client**: `McpHttpClient` (`bin/smdjad/src/mcp_http.rs`) POSTs JSON-RPC 2.0 `tools/list`/`tools/call` to external HTTP servers, and `dispatch_mcp_tool` (`bin/smdjad/src/executor/mod.rs`) forwards any tool name absent from `LOCAL_TOOLS` to a registered server. Three gaps keep smedja from being a first-class MCP participant:

1. **No server mode.** smedja's own native tools — `graph_query`, `read_file`, `list_files`, and the other entries in `executor::LOCAL_TOOLS` — are reachable only by smedja's own turn loop. No external agent or client can call them, even though they are exactly the kind of capability MCP exists to share.
2. **OAuth PKCE is a stub.** `bin/smdjad/src/mcp_oauth.rs` `start_pkce` logs a warning and returns `Err(PkceError::NotImplemented)`; the `start_pkce_returns_not_implemented` test pins this. `TokenStore` (XDG, 0600) is real, but the daemon can only authenticate to remote MCP servers with a static Bearer token — `dispatch_mcp_tool` and `mcp.refresh` both build clients with `McpHttpClient::new(&server.url, "")` (empty token).
3. **HTTP-only transport.** `mcp.register` accepts a `transport` field and stores it on `McpServer`, but every code path assumes HTTP — there is no way to talk to a stdio MCP server launched as a child process, which is the most common local-server transport.

Separately, the Agent Client Protocol (ACP) surface in `bin/smdjad/src/acp.rs` defers streaming: `submit_prompt` carries the comment `// ponytail: full SSE streaming deferred; return turn_id for polling` and clients must poll for turn results instead of receiving server-sent events.

This change makes smedja a bidirectional MCP participant — a server that exposes its native tools, a client that can authenticate via PKCE, a transport layer that supports stdio child processes — and completes the deferred ACP SSE stream.

## What Changes

- **MCP server mode**: expose a curated subset of `executor::LOCAL_TOOLS` (the read-safe tools: `graph_query`, `read_file`, `list_files`, plus the read-only vault/telemetry tools) over a JSON-RPC 2.0 MCP server endpoint, served on the existing ACP/HTTP listener. `tools/list` advertises their input schemas; `tools/call` routes into `executor::execute_tool` under the same least-privilege guard that blocks write tools for read-only sessions.
- **MCP OAuth PKCE**: implement the Authorization Code + PKCE flow in `mcp_oauth.rs` — generate a code verifier/challenge (S256), spawn a localhost redirect listener, exchange the authorization code for a `Token`, and persist via the existing `TokenStore`. Replace the `NotImplemented` return. `dispatch_mcp_tool`/`mcp.refresh` load a stored token for the server URL and pass it to `McpHttpClient` instead of the empty string, with a refresh-token grant when the access token has expired.
- **MCP stdio transport**: add a stdio transport that spawns a configured MCP server as a child process and frames JSON-RPC 2.0 over its stdin/stdout. The `transport` field on `McpServer` (`"http"` | `"stdio"`) selects the transport; `dispatch_mcp_tool` and `mcp.refresh` dispatch through the matching transport. Child-process lifecycle (spawn, reuse, teardown) is managed per registered server.
- **ACP SSE streaming**: replace the deferred polling stub in `submit_prompt` with a Server-Sent Events response that streams turn events (`Started`, token deltas, tool calls, `Completed`) to the ACP client, subscribing to the same dispatcher the turn loop already publishes to.

## Capabilities

### New Capabilities

- `mcp-server`: smedja exposes its read-safe native tools over a JSON-RPC 2.0 MCP server endpoint (`tools/list`, `tools/call`) so external clients can discover and invoke them.
- `mcp-oauth`: smedja authenticates to MCP HTTP servers via the OAuth 2.0 Authorization Code + PKCE flow, persisting and refreshing tokens through `TokenStore`.
- `mcp-transports`: smedja dispatches MCP tool calls over a selectable transport — HTTP (existing) or stdio child process (new) — chosen by the registered server's `transport` field; this capability also covers the ACP SSE streaming response.

## Impact

- `bin/smdjad/src/mcp_oauth.rs`: implement `start_pkce` (verifier/challenge, redirect listener, token exchange, refresh grant); remove the `NotImplemented` path; un-ignore `token_store_round_trips_access_token`.
- `bin/smdjad/src/mcp_http.rs`: no protocol change; clients keep consuming `Token.access_token` as the Bearer value.
- `bin/smdjad/src/mcp_server.rs` (new): JSON-RPC 2.0 MCP server handler exposing the read-safe tool subset; reuses `smedja_rpc::{Request, Response, RpcError}` types.
- `bin/smdjad/src/mcp_stdio.rs` (new): stdio child-process transport with newline/length-framed JSON-RPC 2.0.
- `bin/smdjad/src/executor/mod.rs`: `dispatch_mcp_tool` selects transport by `McpServer.transport` and loads a stored token via `TokenStore`; a new `MCP_SERVER_TOOLS` subset (drawn from `LOCAL_TOOLS`) defines what server mode exposes.
- `bin/smdjad/src/handlers/mcp.rs`: `mcp.refresh` dispatches via the registered transport and authenticated client.
- `bin/smdjad/src/acp.rs`: `submit_prompt` returns an SSE stream of turn events instead of a `turn_id`-only JSON body.
- `bin/smdjad/Cargo.toml`: add the PKCE/stdio support crates (S256 hashing already available via `sha2`; base64url, async child process via `tokio::process`, SSE via the existing `axum`).
- README: the MCP section reflects server mode, PKCE auth, and stdio transport; the ACP section reflects streaming.

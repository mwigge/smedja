## 1. MCP server tool subset

- [x] 1.1 Add a failing test asserting `MCP_SERVER_TOOLS` contains only read-safe tools (`graph_query`, `read_file`, `list_files`, `smedja_vault_search`, `smedja_retrieve`, `otel_query`, `metric_query`, `log_tail`) and excludes every entry of `WRITE_TOOLS` plus `smedja_vault_store`
- [x] 1.2 Define `pub(crate) const MCP_SERVER_TOOLS: &[&str]` in `bin/smdjad/src/executor/mod.rs` as the read-safe subset of `LOCAL_TOOLS`; make the 1.1 test pass
- [x] 1.3 Add a failing test asserting every name in `MCP_SERVER_TOOLS` is also present in `LOCAL_TOOLS` (subset invariant); make it pass

## 2. MCP server mode (tools/list, tools/call)

- [x] 2.1 Add a failing test: a `smedja_rpc::Request` with method `tools/list` produces a `Response::ok` whose `result.tools` lists exactly `MCP_SERVER_TOOLS` with input schemas
- [x] 2.2 Create `bin/smdjad/src/mcp_server.rs` with a handler that parses a `smedja_rpc::Request`, matches `initialize`/`tools/list`/`tools/call`, and returns `smedja_rpc::Response`; implement `tools/list` to satisfy 2.1
- [x] 2.3 Add a failing test: a `tools/call` request for `read_file` routes into `execute_tool` and returns the file content in the MCP `result.content` shape
- [x] 2.4 Implement `tools/call` to invoke `execute_tool` under an effective read-only (`review`-mode) session; satisfy 2.3
- [x] 2.5 Add a failing test: a `tools/call` for a write tool (e.g. `write_file`) returns an MCP error result (blocked by the read-only guard); make it pass
- [x] 2.6 Add a failing test: a `tools/call` for a tool absent from `MCP_SERVER_TOOLS` returns a method/tool-not-found MCP error; make it pass
- [x] 2.7 Mount the MCP server handler as an axum route on the existing ACP listener behind the shared `require_auth` check; add a request-level test that an unauthenticated request is rejected before dispatch

## 3. OAuth PKCE flow

- [x] 3.1 Add a failing test for a `code_challenge` helper: `base64url(SHA256(verifier))` (no padding) matches the RFC 7636 S256 test vector
- [x] 3.2 Implement the verifier/challenge generation (random 32-byte verifier, S256 challenge via `sha2`, base64url-no-pad) in `bin/smdjad/src/mcp_oauth.rs`; satisfy 3.1
- [x] 3.3 Add a failing test: the redirect listener accepts a single loopback callback, validates `state`, extracts `code`, and rejects a callback whose `state` does not match
- [x] 3.4 Implement the localhost (`127.0.0.1:0`) redirect listener with a `oneshot` channel, `state` validation, single-request accept, and a wall-clock timeout mapping to `PkceError::Cancelled`; satisfy 3.3
- [x] 3.5 Add a failing test against a mock token endpoint: `start_pkce` exchanges the code (`grant_type=authorization_code`, `code_verifier`, `redirect_uri`) and returns the parsed `Token`
- [x] 3.6 Implement the token exchange in `start_pkce`, mapping HTTP failures to `PkceError::Http`; persist via `TokenStore::save` mapping save failures to `PkceError::Storage`; remove the `PkceError::NotImplemented` return path
- [x] 3.7 Add a failing test for `refresh_token`: an expired `Token` with a `refresh_token` triggers a `grant_type=refresh_token` exchange and re-saves the new token
- [x] 3.8 Implement `refresh_token(server_url, &token)`; satisfy 3.7
- [x] 3.9 Update `start_pkce_returns_not_implemented` to assert the implemented behaviour and un-ignore `token_store_round_trips_access_token`

## 4. Authenticated outbound MCP calls

- [x] 4.1 Add a failing test: `dispatch_mcp_tool` loads a stored token for the server URL and passes its `access_token` as the Bearer to `McpHttpClient`
- [x] 4.2 In `bin/smdjad/src/executor/mod.rs`, make `dispatch_mcp_tool` load the token via `TokenStore::default_store().load(&server.url)`, fall back to the `MCP_TOKEN` env var, then to empty; build the client with the resolved token; satisfy 4.1
- [x] 4.3 Update `bin/smdjad/src/handlers/mcp.rs` `refresh` to build its client with the resolved token instead of the empty string; add a test that a refresh against a token-protected mock server sends the Bearer header

## 5. stdio transport

- [x] 5.1 Add a failing test: `McpStdioClient` spawns a scripted child MCP server, sends a newline-framed `tools/list` JSON-RPC request, and parses the newline-framed response
- [x] 5.2 Create `bin/smdjad/src/mcp_stdio.rs` with `McpStdioClient` that spawns the configured command via `tokio::process::Command` (piped stdin/stdout), writes one JSON line per request and reads one JSON line per response; satisfy 5.1
- [x] 5.3 Add a failing test: `call_tool` over stdio returns the tool result, and a per-call read timeout maps to an error string
- [x] 5.4 Implement `call_tool`/`list_tools` over stdio with a per-call read timeout; lazy spawn on first call; `Drop`/teardown kills the child; satisfy 5.3
- [x] 5.5 Introduce a `McpTransport` dispatch point (HTTP vs stdio) selected by `McpServer.transport`; route `dispatch_mcp_tool` and `mcp.refresh` through it; add a test that a `transport: "stdio"` server dispatches via `McpStdioClient`
- [x] 5.6 Add a failing test that an unknown/empty `transport` value defaults to HTTP (back-compat); make it pass

## 6. ACP SSE streaming

- [x] 6.1 Add a failing test: a `submit_prompt` request receives an SSE response whose first event is `Started` and which terminates on the turn's terminal event
- [x] 6.2 Replace the `turn_id`-only body in `bin/smdjad/src/acp.rs` `submit_prompt` with an axum `Sse` response subscribed to the dispatcher channel for the new `turn_id`; forward each `TurnEvent` as an SSE event; satisfy 6.1
- [x] 6.3 Add a keep-alive heartbeat to the SSE stream and a test that the stream still ends after the terminal event despite the heartbeat
- [x] 6.4 Confirm the turn is still recorded as a `Task` so polling clients can read the result; add a test asserting the task exists after the stream completes

## 7. Verify

- [x] 7.1 Run `cargo test -p smdjad` — all new and existing tests green
- [x] 7.2 Run `cargo test --workspace` — no regressions introduced by the wiring
- [x] 7.3 Run `cargo clippy -p smdjad -- -D warnings` — clean for the touched modules
- [x] 7.4 Run `openspec validate mcp-server-mode --strict` — clean

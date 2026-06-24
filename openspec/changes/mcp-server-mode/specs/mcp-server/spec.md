## ADDED Requirements

### Requirement: smedja exposes read-safe native tools over an MCP server endpoint

smedja SHALL serve a JSON-RPC 2.0 MCP server endpoint that advertises and invokes a read-safe subset of its native tools (`MCP_SERVER_TOOLS`). The endpoint MUST support the `tools/list` and `tools/call` methods and MUST reuse the `smedja-rpc` request/response envelope.

#### Scenario: tools/list advertises the read-safe subset

- **WHEN** an MCP client sends a `tools/list` JSON-RPC request to the server endpoint
- **THEN** the response SHALL be a `Response::ok` whose `result.tools` enumerates exactly the names in `MCP_SERVER_TOOLS`
- **AND** each advertised tool SHALL include its input schema

#### Scenario: tools/call invokes a native tool

- **WHEN** an MCP client sends a `tools/call` request for a tool in `MCP_SERVER_TOOLS` with valid arguments
- **THEN** the server SHALL route the call into the native tool executor
- **AND** the response SHALL return the tool output in the MCP `result.content` shape

#### Scenario: unknown tool is rejected

- **WHEN** a `tools/call` request names a tool that is not in `MCP_SERVER_TOOLS`
- **THEN** the server SHALL return a JSON-RPC error result rather than dispatching the call

### Requirement: MCP server mode MUST NOT expose mutating or exec tools

The exposed subset SHALL exclude every write/exec tool (`write_file`, `edit_file`, `bash`, `run_command`, `smedja_vault_store`), and `tools/call` MUST execute under an effective read-only session so the least-privilege guard rejects mutating tools even if the subset list drifts.

#### Scenario: write tool is blocked over the server endpoint

- **WHEN** a `tools/call` request names a write or exec tool such as `write_file`
- **THEN** the server SHALL return an MCP error result indicating the tool is blocked
- **AND** no mutation of the workspace SHALL occur

#### Scenario: subset is a strict subset of the local tools

- **WHEN** `MCP_SERVER_TOOLS` is enumerated
- **THEN** every name in it SHALL also be present in `LOCAL_TOOLS`
- **AND** none of the write/exec tools SHALL appear in it

### Requirement: MCP server endpoint requires authentication

The MCP server endpoint SHALL sit behind the same authentication check as the ACP listener it is co-mounted on, so unauthenticated requests are rejected before any tool dispatch.

#### Scenario: unauthenticated request is rejected

- **WHEN** a request reaches the MCP server endpoint without a valid auth token
- **THEN** the request SHALL be rejected with an unauthorized status
- **AND** no `tools/call` dispatch SHALL occur

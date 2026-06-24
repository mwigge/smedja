## ADDED Requirements

### Requirement: MCP tool calls dispatch over a transport selected by the registered server

The daemon SHALL select the MCP transport from the registered server's `transport` field. A value of `"http"` SHALL use the HTTP client; a value of `"stdio"` SHALL use the stdio child-process client; an unknown or empty value SHALL default to HTTP for back-compatibility.

#### Scenario: stdio server dispatches via the stdio client

- **WHEN** a tool is dispatched to a server whose `transport` is `"stdio"`
- **THEN** the call SHALL be routed through the stdio child-process client

#### Scenario: http server dispatches via the HTTP client

- **WHEN** a tool is dispatched to a server whose `transport` is `"http"`
- **THEN** the call SHALL be routed through the HTTP client

#### Scenario: unknown transport defaults to HTTP

- **WHEN** a tool is dispatched to a server whose `transport` is empty or unrecognised
- **THEN** the call SHALL default to the HTTP client

### Requirement: stdio transport spawns and frames JSON-RPC over a child process

The stdio transport SHALL spawn the configured MCP server as a child process using async process APIs, frame JSON-RPC 2.0 messages as newline-delimited JSON over the child's stdin/stdout, and SHALL NOT use blocking I/O inside async code paths.

#### Scenario: request and response are newline-framed

- **WHEN** a `tools/list` or `tools/call` request is sent over the stdio transport
- **THEN** the request SHALL be written as a single line of JSON to the child's stdin
- **AND** the response SHALL be read as a single line of JSON from the child's stdout

#### Scenario: child process is reused across calls

- **WHEN** a second tool call is dispatched to the same stdio server
- **THEN** the existing child process SHALL be reused rather than re-spawned

#### Scenario: a stalled child maps to a tool error

- **WHEN** the child does not produce a response within the per-call timeout
- **THEN** the call SHALL return an error result rather than blocking indefinitely

#### Scenario: child is torn down on teardown

- **WHEN** the stdio client is dropped or the server entry is removed
- **THEN** the child process SHALL be terminated, leaving no orphan process

### Requirement: ACP prompt submission streams turn events over SSE

The ACP `submit_prompt` endpoint SHALL return a Server-Sent Events stream of turn events subscribed to the dispatcher, replacing the deferred polling-only response, while still recording the turn so polling clients can read the result.

#### Scenario: prompt submission returns a streaming response

- **WHEN** a client submits a prompt to an ACP session
- **THEN** the response SHALL be an SSE stream
- **AND** the first turn event delivered SHALL be the `Started` event

#### Scenario: stream terminates on the terminal turn event

- **WHEN** the turn reaches a terminal event (completed or failed)
- **THEN** the SSE stream SHALL deliver that event and then end

#### Scenario: turn remains pollable after streaming

- **WHEN** the SSE stream has completed
- **THEN** the turn SHALL still be recorded as a task so a polling client can read its result

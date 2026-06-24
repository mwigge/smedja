## ADDED Requirements

### Requirement: Shell tools execute inside a per-platform isolation boundary

`smdjad` SHALL execute `bash` and `run_command` tool calls inside an isolation backend selected by capability detection. The selection order MUST be: the Docker backend when the operator has opted into it and the Docker daemon is reachable; otherwise the current platform's OS-native backend (macOS Seatbelt or Linux Landlock); otherwise no backend. Read-only tools (`read_file`, `list_files`, `graph_query`) MUST remain exempt from the sandbox.

#### Scenario: native backend selected when Docker is absent

- **WHEN** the daemon starts on macOS or Linux with no reachable Docker daemon and the sandbox mode is not `off`
- **THEN** the selected backend SHALL be the OS-native backend for that platform (Seatbelt on macOS, Landlock on Linux)
- **AND** a `bash` tool call SHALL be executed through that backend rather than directly on the host

#### Scenario: Docker preferred when the operator opts in

- **WHEN** the operator has opted into Docker and the Docker daemon is reachable
- **THEN** the selected backend SHALL be the Docker backend
- **AND** the `smedja.sandbox.exec` span SHALL record `backend = "docker"`

#### Scenario: read-only tool bypasses the sandbox

- **WHEN** a `read_file`, `list_files`, or `graph_query` tool is executed
- **THEN** it SHALL NOT be routed through any sandbox backend

### Requirement: Filesystem confined to the active worktree

The sandbox SHALL grant write access only to the resolved confined root — the task's worktree path when a task owns a worktree, otherwise the session workspace — plus an ephemeral `/tmp`. The confined root MUST be canonicalised using the same boundary contract as `assert_within_workspace`, and the repository `.git` directory (when present) MUST be exposed read-only. Writes outside the confined root MUST be denied by the backend.

#### Scenario: write outside the workspace is denied

- **WHEN** a sandboxed command attempts to write to a path outside the confined root (for example `/etc/passwd` or a sibling directory)
- **THEN** the write SHALL be denied by the isolation backend
- **AND** the command's failure SHALL be reflected in the tool result

#### Scenario: confined root follows the active worktree

- **WHEN** a tool call executes for a task that owns a git worktree
- **THEN** the confined writable root SHALL be that worktree's path, not the shared session checkout

### Requirement: Declarative network policy sharing the SSRF allowlist

The sandbox SHALL apply a network policy of `none`, `allowlist`, or `open`, defaulting to `none`. Under `allowlist` the permitted egress set MUST be exactly the destinations not rejected by the daemon's `is_blocked_ip` predicate, so the sandbox and the SSRF guard share one source of truth. Destinations rejected by `is_blocked_ip` (loopback, RFC-1918 private ranges, link-local including the cloud metadata endpoint `169.254.169.254`, CGNAT, and IPv6 ULA/link-local) MUST remain unreachable even under the `open` policy.

#### Scenario: default policy denies all egress

- **WHEN** no network policy is configured and a sandboxed command attempts any outbound connection
- **THEN** the connection SHALL be denied

#### Scenario: metadata endpoint blocked even when egress is open

- **WHEN** the network policy is `open` and a sandboxed command attempts to reach `169.254.169.254` or a loopback/private address
- **THEN** the connection SHALL be denied because the destination is rejected by `is_blocked_ip`

#### Scenario: allowlist permits public egress only

- **WHEN** the network policy is `allowlist` and a sandboxed command attempts to reach a publicly routable address
- **THEN** the connection SHALL be permitted
- **AND** a connection to a private or link-local address SHALL be denied

### Requirement: Explicit fallback when no backend is available

The sandbox mode SHALL be one of `auto`, `required`, or `off`, defaulting to `auto`, and SHALL govern behaviour when no isolation backend is available. Under `required` the tool call MUST fail closed with a diagnostic naming the missing capability and MUST NOT execute the command. Under `auto` the command MAY execute on the host but the tool result MUST be stamped with an unconfined marker and a `smedja.sandbox.unconfined` event MUST be emitted. Under `off` the sandbox SHALL be skipped. The prior silent host fallthrough MUST be removed.

#### Scenario: required mode fails closed without a backend

- **WHEN** the sandbox mode is `required` and no isolation backend is available
- **THEN** the tool call SHALL return an `error:`-form result naming the missing backend capability
- **AND** the command SHALL NOT be executed

#### Scenario: auto mode falls back with a visible marker

- **WHEN** the sandbox mode is `auto` and no isolation backend is available
- **THEN** the command MAY be executed on the host
- **AND** the tool result SHALL be prefixed with an unconfined marker
- **AND** a `smedja.sandbox.unconfined` event SHALL be emitted recording the reason no backend was available

#### Scenario: off mode skips the sandbox

- **WHEN** the sandbox mode is `off`
- **THEN** the command SHALL be executed without any sandbox backend and without an unconfined marker

### Requirement: Sandbox execution is observable

Every sandboxed execution SHALL emit a `smedja.sandbox.exec` span carrying the selected `backend`, the `network_policy`, the `mode`, and the `confined_root`. The daemon MUST use structured logging only and MUST NOT print to stdout/stderr from library code. Operators SHALL be able to query the active configuration via a `smj sandbox status` command that reports the selected backend, its availability, the active network policy, and the fallback mode.

#### Scenario: span records the sandbox configuration

- **WHEN** a sandboxed `bash` command executes
- **THEN** a `smedja.sandbox.exec` span SHALL be emitted with `backend`, `network_policy`, `mode`, and `confined_root` attributes

#### Scenario: status command reports the active backend

- **WHEN** an operator runs `smj sandbox status`
- **THEN** the output SHALL report the selected backend, whether it is available, the active network policy, and the fallback mode

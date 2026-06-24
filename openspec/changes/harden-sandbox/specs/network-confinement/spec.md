## ADDED Requirements

### Requirement: NetworkPolicy::None denies all egress for the sandboxed subprocess

When the active network policy is `none`, the sandbox backend SHALL prevent the sandboxed command from reaching any network destination. On Linux this MUST be enforced by running the command in a fresh network namespace; on macOS Seatbelt by `(deny network*)`; on Docker by `--network none`.

#### Scenario: Linux network namespace denies egress under none

- **WHEN** the Landlock backend executes a command under `NetworkPolicy::None` on a host where a network namespace can be created
- **THEN** the command SHALL run in a fresh network namespace with no route to any external host
- **AND** an outbound connection attempt SHALL fail

#### Scenario: Seatbelt denies network under none

- **WHEN** the Seatbelt profile is rendered under `NetworkPolicy::None`
- **THEN** the profile SHALL contain `(deny network*)`

#### Scenario: Docker isolates the network under none

- **WHEN** the Docker backend executes a command under `NetworkPolicy::None`
- **THEN** the `docker run` invocation SHALL include `--network none`

### Requirement: is_blocked_ip ranges stay blocked under allowlist and open

Under `allowlist` and `open`, the `is_blocked_ip` SSRF floor (loopback, RFC-1918 private, link-local including IMDS `169.254.169.254`, CGNAT, IPv6 ULA and link-local) SHALL remain blocked for smedja's own clients via `NetworkPolicy::permits_dest`, and this change SHALL NOT widen those ranges. For a raw sandboxed subprocess, `allowlist` SHALL be treated as `open`-minus-blocked-ranges, because a subprocess cannot be per-destination IP-filtered without a filtering proxy; this limitation MUST be documented.

#### Scenario: blocked ranges remain blocked under open

- **WHEN** `NetworkPolicy::Open.permits_dest` is evaluated for the IMDS address `169.254.169.254` or a loopback address
- **THEN** it SHALL return `false`
- **AND** for a publicly routable address it SHALL return `true`

#### Scenario: allowlist for a subprocess is documented as open-minus-blocked

- **WHEN** a sandboxed subprocess runs under `NetworkPolicy::Allowlist` with no filtering proxy configured
- **THEN** the subprocess SHALL retain host network egress except for the `is_blocked_ip` ranges
- **AND** the documentation SHALL state that per-host allow-listing is not enforced for subprocesses in this change

### Requirement: Network confinement honours the mode contract when unavailable

When `NetworkPolicy::None` is requested but the platform cannot create a network namespace, the backend SHALL NOT silently grant egress. Under `SandboxMode::Required` the execution SHALL fail closed; under `SandboxMode::Auto` it SHALL fall back per the existing unconfined-marker contract; and the `smedja.sandbox.exec` span SHALL record whether network confinement was applied.

#### Scenario: required fails closed when netns is unavailable

- **WHEN** `NetworkPolicy::None` is requested, `SandboxMode::Required` is active, and no network namespace can be created
- **THEN** the command SHALL NOT execute with network access
- **AND** the result SHALL be an error naming the missing confinement capability

#### Scenario: auto falls back with the unconfined marker

- **WHEN** `NetworkPolicy::None` is requested, `SandboxMode::Auto` is active, and no network namespace can be created
- **THEN** the command SHALL fall back per the existing fallback contract with the unconfined marker
- **AND** the `smedja.sandbox.exec` span SHALL carry `net_confined = false`

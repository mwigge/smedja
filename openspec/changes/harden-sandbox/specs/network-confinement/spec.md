## ADDED Requirements

### Requirement: NetworkPolicy::None denies all egress for the sandboxed subprocess

When the active network policy is `none`, the sandbox backend SHALL prevent the sandboxed command from reaching any network destination wherever the platform supports it. On Linux this SHALL be enforced by a fresh network namespace when one can be created (best-effort — see the degradation requirement below); on macOS Seatbelt by `(deny network*)`; on Docker by `--network none`.

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

### Requirement: Network confinement is best-effort; the filesystem boundary is the hard guarantee

Network namespaces require capabilities (`CAP_NET_ADMIN` or unprivileged user namespaces) that many hosts and CI runners lack. When `NetworkPolicy::None` is requested but the platform cannot create a network namespace, the backend SHALL NOT fail the command; it SHALL run the command **filesystem-confined on the host network** rather than blocking it or running it fully unconfined. The filesystem boundary remains the hard guarantee on the degraded path. (Failing the command closed under `SandboxMode::Required` when only the network cannot be confined is a documented non-goal of this change; the filesystem-confinement guarantee still fails closed under `Required` when no isolation backend is available at all.)

#### Scenario: netns unavailable degrades to filesystem-confined host network

- **WHEN** `NetworkPolicy::None` is requested but no network namespace can be created
- **THEN** the command SHALL still execute, filesystem-confined to the worktree, on the host network
- **AND** a write outside the confined root SHALL still be denied (the filesystem boundary holds on the degraded path)

#### Scenario: netns available enforces no egress

- **WHEN** `NetworkPolicy::None` is requested on a host that can create a network namespace
- **THEN** the command SHALL run in a fresh network namespace with no route to any external host

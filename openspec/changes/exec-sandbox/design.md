## Context

Shell/tool execution in `smdjad` flows through `execute_tool` (`bin/smdjad/src/executor/mod.rs`). For `bash`/`run_command` it constructs a `SandboxExecutor` (`bin/smdjad/src/sandbox.rs`) and, when `sandbox.available && !is_exempt(tool)`, runs `sandbox.exec(cmd, workspace)`; otherwise it calls `super::exec_bash`, which runs `sh -c <cmd>` on the host (`bin/smdjad/src/main.rs`).

The current sandbox surface:

- `SandboxExecutor::new()` — opt-in via `SMEDJA_TOOL_SANDBOX=docker`; `available = false` unless `docker` is on `PATH` and the image inspects clean. Image pinned via `SMEDJA_SANDBOX_IMAGE`.
- `SandboxExecutor::exec(cmd, workspace)` — canonicalises the workspace, optionally checks it against `SMEDJA_WORKSPACE_ROOT`, then runs `docker run --rm --network none --cpus 0.5 --memory 256m --pids-limit 64 --read-only --cap-drop ALL --security-opt no-new-privileges --tmpfs /tmp -v <ws>:/workspace:rw -w /workspace` with `.git` shadowed read-only. 30-second timeout.
- `EXEMPT_TOOLS = [read_file, list_files, graph_query]` bypass the sandbox.

Adjacent precedents:

- **Filesystem boundary**: `assert_within_workspace` (`bin/smdjad/src/executor/fs_tools.rs`) canonicalises and asserts a path stays within the workspace root — already enforced on `read_file`/`write_file`/`edit_file`/`list_files`.
- **Network boundary**: `is_blocked_ip` (`bin/smdjad/src/main.rs`) classifies loopback / RFC-1918 / link-local (incl. IMDS `169.254.169.254`) / CGNAT / IPv6 ULA+link-local as blocked, with IPv4-mapped-IPv6 unwrapping. `is_safe_mcp_url` builds on it. This is the SSRF defence.
- **Approval boundary**: `CoworkGate::intercept` (`bin/smdjad/src/cowork.rs`) suspends a tool call for human approve/deny/modify in cowork mode — the human-in-the-loop precedent.
- **Command-risk boundary**: `CommandRisk::{Safe, Confirm, Blocked}` (`crates/smedja-ingot/src/guard.rs`) classifies shell commands by a regex blocklist (`rm -rf /`, `curl … | sh`, …). `smedja_assayer::classify_bash` gates write-arity commands for review sessions.
- **Worktree isolation**: `WorktreePool` (`bin/smdjad/src/handlers/task.rs`, state in `handlers/mod.rs`) gives each task its own git worktree at `worktree_path`.

The sandbox is therefore the *last* boundary in a layered model: command-risk classification and the cowork gate decide *whether* a command runs; the sandbox decides *what it can touch* once it does.

## Goals / Non-Goals

Goals:
- Confine shell/tool execution on both macOS and Linux, not Docker-only.
- Make the writable filesystem root the active worktree (or session workspace), enforced by the kernel boundary.
- Replace `--network none` with a declarative `none | allowlist | open` policy that shares `is_blocked_ip`.
- Make the no-backend behaviour explicit and observable (`auto | required | off`), removing the silent host fallthrough.
- Give operators a `smj sandbox status` command that reports the active backend and policy.

Non-Goals:
- Per-syscall seccomp filtering or gVisor/Kata-class hardening — out of scope; the backends use the platform-native primitives (Docker flags, Seatbelt profile, Landlock ruleset).
- Sandboxing MCP-dispatched tools that run in another process/server — only locally executed `bash`/`run_command` are confined here.
- Changing the command-risk classifier or cowork gate — those upstream boundaries are unchanged.
- Windows support — the workspace targets macOS and Linux only.
- A custom container runtime or rootless-Docker bootstrap — Docker availability stays an operator concern.

## Decisions

**Decision: isolation mechanism is per-platform, behind a `SandboxBackend` trait.**
There is no single primitive that confines a child process on both macOS and Linux without root. The pragmatic cross-platform answer is a trait with three implementations selected by capability detection:
- **Docker** (existing, any platform with the daemon): strongest isolation; opt-in; usually absent on macOS workstations.
- **macOS Seatbelt** (`sandbox-exec` with a generated `.sb` profile): ships with macOS, no install, no root. Confines fs writes to the worktree and network per policy. `sandbox-exec` is deprecated-but-present and remains the only no-dependency option on macOS, so we accept it with a documented migration path.
- **Linux Landlock** (`landlock` LSM via the `landlock` crate, plus an unprivileged user namespace for network): available on kernels ≥ 5.13, no root. Filesystem confinement via Landlock rules; network confinement via a `none`/slirp-style egress depending on policy.

Selection order (when `SandboxMode` is `auto` or `required`): Docker if the operator opted in and it is reachable, else the OS-native backend for the current platform, else no backend. Rationale: Docker is the strongest and the operator opted into it explicitly; the OS-native backend is the zero-config default that makes confinement real on a stock workstation.
- Alternative considered: WASM/WASI sandboxing. Rejected — running arbitrary developer shell commands (git, ripgrep, build tools) inside WASI is not viable; WASI suits embedding pure computations, not a POSIX shell.
- Alternative considered: Docker-only with a hard requirement. Rejected — it makes the feature unusable on the macOS half of the target matrix and pushes users back to unconfined execution.

**Decision: the confined filesystem root is the active worktree, resolved through the existing boundary contract.**
`execute_tool` already receives `workspace: &Path`. When a task owns a worktree (`WorktreePool`), the caller passes the `worktree_path` as the confined root; otherwise the session workspace. The root is canonicalised exactly as `assert_within_workspace` does, and the backend grants write access only to that subtree (plus a tmpfs `/tmp`), with `.git` read-only as today.
- Rationale: confinement should match the boundary the rest of the daemon already enforces, and worktree-per-task is the existing isolation unit. A kernel-enforced root makes the path check defence-in-depth rather than the only line.
- Alternative: a fixed `/workspace` mount regardless of worktree. Rejected — it lets a task in one worktree write into the shared checkout, defeating worktree isolation.

**Decision: network policy is `none | allowlist | open`, sharing `is_blocked_ip`.**
`SMEDJA_SANDBOX_NETWORK` (default `none`) sets the policy.
- `none`: no egress (Docker `--network none`; Seatbelt deny `network*`; Landlock + no network namespace egress).
- `allowlist`: egress permitted only to destinations not rejected by `is_blocked_ip`, so private/loopback/IMDS/CGNAT/ULA ranges stay blocked. The same predicate the SSRF guard uses is the sandbox's egress filter — one source of truth.
- `open`: general egress, but `is_blocked_ip` ranges remain blocked (IMDS and loopback are never reachable from a sandboxed command).
- Rationale: a misbehaving command must not reach the metadata endpoint or internal services even when the operator wants outbound internet. Reusing `is_blocked_ip` keeps the daemon's two network boundaries consistent.
- Alternative: full DNS/CIDR allowlist config. Deferred — the `is_blocked_ip` floor covers the SSRF threat; richer allowlists can layer on later without changing the contract.

**Decision: opt-in vs default is governed by `SandboxMode = auto | required | off`.**
`SMEDJA_SANDBOX_MODE` (default `auto`):
- `auto`: use the best available backend; if none is available, run on the host but mark the result unconfined and emit an event. Confinement is best-effort and never blocks work.
- `required`: use the best available backend; if none is available, **fail the tool call closed** with a diagnostic naming what is missing (no Docker, kernel too old, etc.). For environments that must guarantee confinement.
- `off`: skip the sandbox entirely (today's behaviour when the env var is unset).
The legacy `SMEDJA_TOOL_SANDBOX=docker` is honoured as a compatibility alias that pins the backend to Docker and sets mode `auto`.
- Rationale: defaulting to `auto` with a native backend makes confinement real out of the box on macOS/Linux without breaking workflows where no backend exists; `required` serves regulated/CI contexts; `off` is the escape hatch.
- Alternative: default `required`. Rejected — it would hard-fail on any host lacking a usable backend, regressing usability.

**Decision: the fallback is explicit and stamped, never silent.**
When `auto` falls back to host execution, the tool result is prefixed with a single-line unconfined marker and a `smedja.sandbox.unconfined` event is emitted (backend-unavailable reason included). When `required` cannot confine, the tool returns an error string in the established `error: …` form (matching the existing executor error convention) and does not execute the command.
- Rationale: the agent and the operator must be able to see that a command ran without confinement; the current silent `tracing::warn!` is invisible to the agent and to telemetry consumers.

**Decision: read-only exempt tools stay exempt.**
`read_file`, `list_files`, `graph_query` continue to bypass the sandbox (`is_exempt`), since they have no side effects and already pass `assert_within_workspace`.

## Risks / Trade-offs

- [Risk] `sandbox-exec` is deprecated on macOS and could be removed in a future OS release → Mitigation: it is the only no-dependency option today; `smj sandbox status` surfaces the backend so operators can switch to Docker; the trait makes adding a successor backend a localized change.
- [Risk] Landlock is unavailable on kernels < 5.13 or when disabled → Mitigation: capability detection at startup downgrades to no-backend; under `required` the tool fails closed with a clear reason; under `auto` it falls back with the unconfined marker.
- [Risk] Worktree-rooted confinement could break commands that legitimately read shared repo state outside the worktree → Mitigation: grant the worktree subtree read-write and the shared git object store read-only (the `.git` mount already models this); document the boundary.
- [Risk] Network `allowlist`/`open` still permits DNS exfiltration paths the IP filter cannot see → Mitigation: scope is explicitly the SSRF/IMDS floor; default remains `none`; richer egress control is a deferred non-goal.
- [Risk] Three backends increase surface and platform-specific test burden → Mitigation: the dispatch contract is small (one `exec` method); backend availability is unit-tested via capability stubs, and end-to-end confinement is asserted per platform in the verification group.
- [Risk] Changing the default from off to `auto` alters behaviour for existing users → Mitigation: `auto` never blocks work (falls back with a marker); `SMEDJA_SANDBOX_MODE=off` restores the prior behaviour exactly; the legacy `SMEDJA_TOOL_SANDBOX=docker` alias keeps working.

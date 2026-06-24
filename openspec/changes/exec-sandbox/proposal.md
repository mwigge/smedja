## Why

`smdjad` already runs shell/tool commands behind a `SandboxExecutor` (`bin/smdjad/src/sandbox.rs`), wired into `execute_tool` for `bash`/`run_command` (`bin/smdjad/src/executor/mod.rs`). But that boundary is Docker-only and largely advisory:

- It is **Docker-only**. `SandboxExecutor::new()` reports `available = false` unless `SMEDJA_TOOL_SANDBOX=docker` is set *and* the `docker` binary is present. On a stock macOS workstation — a first-class target for this workspace — Docker is usually absent, so every shell command silently falls through to `super::exec_bash`, which runs `sh -c <cmd>` directly on the host with no confinement (`bin/smdjad/src/main.rs` `exec_bash`).
- The fallthrough is **silent**. When the operator opts in but the backend is missing (no `docker`, image not built), the command still executes unsandboxed with only a `tracing::warn!` — there is no signal in the tool result and no way to choose fail-closed.
- The network policy is **all-or-nothing**. The container runs with `--network none`; there is no way to allow a curated egress set, and the policy does not compose with the existing SSRF allowlist (`is_blocked_ip` in `bin/smdjad/src/main.rs`), which is the established network-boundary precedent.
- `smj sandbox build` (`bin/smj/src/main.rs`) only shells out to `docker build`. There is no command to report whether a usable backend exists, which backend was selected, or why a platform fell back — so operators cannot tell whether confinement is actually active.
- It does not **compose with worktree isolation**. Per-task git worktrees (`WorktreePool`, `bin/smdjad/src/handlers/task.rs`) already give each task a separate working tree, but the sandbox confines to the original workspace mount, not the active worktree, so the two boundaries are unaware of each other.

The goal: when an agent executes a shell tool, the command runs inside a real isolation boundary with a constrained filesystem and a declared network policy, on **both macOS and Linux**, with an explicit and observable contract for what happens when no backend is available.

## What Changes

- **Backend abstraction over the isolation mechanism.** Introduce a `SandboxBackend` trait with the existing Docker implementation plus two OS-native backends — a macOS `sandbox-exec` (Seatbelt) profile and a Linux Landlock + namespace backend — selected by capability detection at startup. `SandboxExecutor` becomes a thin dispatcher over the selected backend.
- **Confined filesystem rooted at the active worktree.** The writable root is the *resolved task workspace* (the worktree path when a task owns one, otherwise the session workspace), reusing the `assert_within_workspace` canonicalisation contract. Writes outside that root are denied by the kernel boundary, not just the path check.
- **Declarative network policy.** A three-mode policy — `none` (default), `allowlist`, `open` — replaces the hard-coded `--network none`. `allowlist` reuses `is_blocked_ip` so the sandbox egress set and the daemon's SSRF set share one source of truth; private/loopback/IMDS ranges stay blocked even in `open`.
- **Explicit fallback contract.** A new `SandboxMode` (`auto` | `required` | `off`) controls behaviour when no backend is available: `required` fails the tool call closed with a diagnostic; `auto` falls back to host execution but stamps the tool result with an unconfined marker and emits a span; `off` skips the sandbox. The silent fallthrough is removed.
- **Operator visibility.** `smj sandbox build` keeps building the Docker image; a new `smj sandbox status` reports the selected backend, its availability, the active network policy, and the fallback mode, so operators can confirm confinement before relying on it.
- **Observability.** Sandbox execution emits an OTel span (`smedja.sandbox.exec`) with backend, network-policy, fallback-mode, and confined-root attributes, and a structured event when a command runs unconfined.

## Capabilities

### New Capabilities

- `exec-sandbox`: `smdjad` confines shell/tool execution inside a per-platform isolation boundary (Docker, macOS Seatbelt, or Linux Landlock) with a filesystem confined to the active worktree, a declarative network policy that shares the SSRF allowlist, an explicit fail-open/fail-closed fallback contract, and operator status reporting.

## Impact

- `bin/smdjad/src/sandbox.rs`: extract a `SandboxBackend` trait; keep Docker as one backend; add `SeatbeltBackend` (macOS) and `LandlockBackend` (Linux); add backend selection, `SandboxMode`, and `NetworkPolicy`.
- `bin/smdjad/src/executor/mod.rs`: route the resolved confined root (worktree-aware) into `exec`; apply the fallback contract; remove the silent host fallthrough; stamp unconfined results.
- `bin/smdjad/src/main.rs`: reuse `is_blocked_ip` for the sandbox `allowlist` policy; emit the sandbox span/event.
- `bin/smdjad/src/handlers/task.rs`: pass the active worktree path as the confined root when a task owns one.
- `bin/smj/src/main.rs`: add `SandboxCmd::Status`; keep `SandboxCmd::Build`.
- `scripts/sandbox/`: profiles for the OS-native backends alongside the existing `Dockerfile`.
- README / docs: the sandbox section becomes accurate — cross-platform, network-policy-aware, with a documented fallback contract.

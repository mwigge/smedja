## 1. Backend abstraction and selection

- [x] 1.1 Add tests for a `SandboxBackend` trait contract: a stub backend reports `available`, exposes its `name`, and its `exec(cmd, confined_root, policy)` returns the stub output (`backend_trait_dispatches_to_selected_impl`)
- [x] 1.2 Add tests for backend selection precedence: Docker-opted-in-and-reachable wins; else the current platform's OS-native backend; else none (`selection_prefers_docker_then_native_then_none`)
- [x] 1.3 Extract the existing Docker logic from `SandboxExecutor` into a `DockerBackend` implementing `SandboxBackend`; make `SandboxExecutor` a dispatcher holding the selected backend
- [x] 1.4 Add `NetworkPolicy { None, Allowlist, Open }` parsed from `SMEDJA_SANDBOX_NETWORK` (default `None`) and `SandboxMode { Auto, Required, Off }` parsed from `SMEDJA_SANDBOX_MODE` (default `Auto`), honouring the legacy `SMEDJA_TOOL_SANDBOX=docker` alias (pins Docker, mode `Auto`)
- [x] 1.5 Run `cargo test -p smdjad` for groups 1.1–1.4; fix until green

## 2. macOS Seatbelt backend

- [x] 2.1 Add a test (gated `#[cfg(target_os = "macos")]`) that a generated Seatbelt profile denies writes outside the confined root and that the profile string is well-formed for `none`/`allowlist`/`open` network policies (`seatbelt_profile_confines_writes_and_encodes_network_policy`)
- [x] 2.2 Implement `SeatbeltBackend`: detect `sandbox-exec`; generate a `.sb` profile granting read-write only under the confined root (+ tmpfs), `.git` read-only, and network rules per policy; run the command via `sandbox-exec -p <profile> sh -c <cmd>`
- [x] 2.3 Add the Seatbelt profile template under `scripts/sandbox/` and wire its generation
- [x] 2.4 Run the macOS-gated tests; fix until green

## 3. Linux Landlock backend

- [x] 3.1 Add a test (gated `#[cfg(target_os = "linux")]`) that the Landlock ruleset grants write access only to the confined root and that a write outside it is denied (`landlock_ruleset_denies_write_outside_root`)
- [x] 3.2 Implement `LandlockBackend`: detect Landlock support (kernel ≥ 5.13); build a ruleset granting read-write under the confined root, read-only `.git`, tmpfs `/tmp`; apply network confinement per policy (no egress for `None`)
- [x] 3.3 Add capability detection that downgrades to no-backend when Landlock is unavailable
- [x] 3.4 Run the linux-gated tests; fix until green

## 4. Confined root resolution (worktree-aware)

- [x] 4.1 Add a test that when a task owns a worktree, `execute_tool` passes the `worktree_path` as the confined root, and otherwise the session workspace (`confined_root_is_worktree_when_task_owns_one`)
- [x] 4.2 Resolve the confined root in `bin/smdjad/src/handlers/task.rs` / the `execute_tool` call path, canonicalising it via the `assert_within_workspace` contract
- [x] 4.3 Pass the resolved confined root into `SandboxExecutor::exec` for every backend
- [x] 4.4 Run `cargo test -p smdjad` for group 4; fix until green

## 5. Network policy sharing the SSRF allowlist

- [x] 5.1 Add a test that under `allowlist`/`open`, a destination rejected by `is_blocked_ip` (loopback, RFC-1918, `169.254.169.254`) is blocked, and under `none` all egress is denied (`network_policy_reuses_is_blocked_ip_floor`)
- [x] 5.2 Make the `allowlist` policy reuse `is_blocked_ip` as its egress predicate so the sandbox and SSRF guard share one source of truth; keep `is_blocked_ip` ranges blocked even under `open`
- [x] 5.3 Map each `NetworkPolicy` to the per-backend mechanism (Docker network args, Seatbelt `network*` rules, Landlock/namespace egress)
- [x] 5.4 Run `cargo test -p smdjad` for group 5; fix until green

## 6. Fallback contract and result stamping

- [x] 6.1 Add tests: `required` with no available backend returns an `error:`-form result and does NOT execute; `auto` with no backend runs on the host and prefixes an unconfined marker; `off` skips the sandbox entirely (`required_fails_closed`, `auto_falls_back_with_marker`, `off_skips_sandbox`)
- [x] 6.2 Implement the fallback in `execute_tool`: remove the silent host fallthrough; apply the mode-driven behaviour
- [x] 6.3 Keep `read_file`/`list_files`/`graph_query` exempt; confirm exemption is unaffected by mode
- [x] 6.4 Run `cargo test -p smdjad` for group 6; fix until green

## 7. Observability

- [x] 7.1 Add a test asserting the sandbox span/event carries `backend`, `network_policy`, `mode`, and `confined_root` attributes, and that an unconfined run emits the `smedja.sandbox.unconfined` event (`sandbox_exec_emits_span_with_backend_attributes`)
- [x] 7.2 Emit the `smedja.sandbox.exec` span and the `smedja.sandbox.unconfined` event via structured logging (no `println!`)
- [x] 7.3 Run `cargo test -p smdjad` for group 7; fix until green

## 8. Operator status command

- [x] 8.1 Add a test that `smj sandbox status` reports the selected backend, its availability, the network policy, and the fallback mode (`sandbox_status_reports_backend_and_policy`)
- [x] 8.2 Add `SandboxCmd::Status` to `bin/smj/src/main.rs` and implement it; keep `SandboxCmd::Build` building the Docker image
- [x] 8.3 Run `cargo test -p smj` for group 8; fix until green

## 9. Verify

- [x] 9.1 Run `cargo test --workspace` — all green
- [x] 9.2 Run `cargo clippy -p smdjad -p smj -- -D warnings` — clean for the touched code
- [x] 9.3 Update the README sandbox section to describe the cross-platform backends, the network policy, and the fallback modes
- [x] 9.4 Run `openspec validate exec-sandbox --strict` — clean

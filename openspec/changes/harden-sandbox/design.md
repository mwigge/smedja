## Context

`exec-sandbox` shipped three `SandboxBackend` implementations selected by capability detection (`sandbox.rs:201` `select_backend`; precedence Docker → OS-native → none). The trait (`sandbox.rs:171`) is:

```
async fn exec(&self, cmd: &str, confined_root: &Path, policy: NetworkPolicy) -> Result<String, String>;
```

Today the backends confine **writes** only:

- **Landlock** (`landlock_backend.rs:62` `apply`) builds an ABI-v1 ruleset: `read` on `PathBeneath("/")` so the shell + shared libs load, and `from_all` (read-write) on the confined root. It takes `_policy` and ignores it (`landlock_backend.rs:113`). Module doc explicitly scopes read + network confinement out (`landlock_backend.rs:15-18`).
- **Seatbelt** (`seatbelt.rs:46` `render_profile`) fills a `.sb` template that does `(deny default)`, blanket `(allow file-read*)` (`seatbelt.sb.template:24`), `(allow file-write*)` under the root + `/tmp`, `(deny file-write*)` under `.git`, and a `@NETWORK_RULE@` line from `network_rule` (`seatbelt.rs:35`) that already encodes `none → (deny network*)`.
- **Docker** (`docker.rs:91` `exec`) bind-mounts only the confined root (`-v root:/workspace:rw`), runs `--read-only` with `--cap-drop ALL` and `--network none|bridge` (`docker.rs:73` `network_arg`). The host home is never mounted, so reads are already structurally confined.

The egress floor `NetworkPolicy::permits_dest` (`sandbox.rs:119`) and `is_blocked_ip` (`main.rs:361`) gate smedja's in-process HTTP clients — loopback, RFC-1918, link-local/IMDS `169.254.169.254`, CGNAT, IPv6 ULA/link-local. They do **not** observe a child process's sockets.

The executor invokes the sandbox at `executor/mod.rs:399-411`: it builds a `SandboxExecutor`, resolves `confined_root_for(workspace)`, and calls `run_confined`, which emits the `smedja.sandbox.exec` span (`sandbox.rs:355`) and applies the `auto|required|off` fallback contract (`sandbox.rs:372-394`).

## Goals / Non-Goals

Goals:
- Confine sandboxed-command **reads** to a system-dir allow-list + the confined root; make host secrets (`~/.ssh`, `~/.aws`, `~/.config`) unreadable.
- Make `NetworkPolicy` actually enforced for the subprocess on every backend: `none` = no egress; `allowlist`/`open` keep the `is_blocked_ip` ranges blocked.
- Keep the change additive over the existing trait + backends and preserve the `auto|required|off` mode contract and fallback semantics.
- Make the system-dir allow-list configurable and document the failure mode.

Non-Goals:
- DNS-name or per-host egress allow-listing for the subprocess (no transparent proxy in this change; `allowlist` for a raw subprocess is `open`-minus-blocked-ranges — see Decision 2).
- Confining reads on platforms with no OS-native backend (the fallback contract already governs the unconfined case).
- Re-implementing `is_blocked_ip` ranges or changing the SSRF floor for smedja's own HTTP clients.
- Persisting or caching network namespaces across executions (each command gets a fresh namespace).

## Decisions

**Decision 1: Read confinement = tighten the allow-list, never deny-list under a broad grant.**
Landlock is additive-grant: rules only *add* access, so you cannot carve a read-only hole beneath a `read` grant on `/`. Therefore `apply` switches from "read `/`" to read+exec rules on a bounded set of system directories — `SMEDJA_SANDBOX_DEFAULT_READ_PATHS` = `/usr /bin /sbin /lib /lib64 /etc /opt /System /Library /private/var/db/dyld` (the macOS dyld cache entries apply to Seatbelt only) — plus read-write on the confined root. Paths that do not exist on the host are skipped (no error). Because the broad `/` grant is gone, `$HOME` and its secret subdirectories are simply never granted, so they are unreadable. Seatbelt cannot rely on additive-only semantics the same way, so its profile gets both an allow list over the same system dirs **and** explicit `(deny file-read*)` rules over secret subpaths (`$HOME/.ssh`, `$HOME/.aws`, `$HOME/.config`, `$HOME/.gnupg`) for defence in depth, replacing the blanket `(allow file-read*)`. Docker needs no read rule change: the host home is never mounted, so reads are structurally confined; the design records that as the Docker read floor.
- Trade-off (honest): a tighter allow-list **can break** a program that reads an unlisted path (e.g. a toolchain under `/Users/<me>/.rustup`, a Homebrew prefix under `/opt/homebrew`, or `$HOME/.cargo`). Mitigation: the set is configurable via `SMEDJA_SANDBOX_READ_PATHS` (colon-separated, *appended* to the defaults), and the failure mode — a command failing with a permission/ENOENT error on a path it expected to read — is documented in the README with the exact env var to widen the list. We deliberately do **not** include `$HOME` in the defaults; an operator who needs it opts in explicitly and accepts the secret-exposure trade-off.
- Alternative considered: keep the broad read grant and rely on the agent not to read secrets. Rejected — that is the status quo gap this change exists to close.

**Decision 2: Network confinement enforced per backend, with an honest subprocess limitation.**
The subprocess cannot be IP-filtered the way smedja's in-process HTTP client is (`permits_dest` runs in smedja's address space, not the child's). So enforcement is structural per backend:
- **Linux**: when `policy == NetworkPolicy::None`, run the command in a fresh network namespace so it has no route to anywhere (`unshare --net sh -c <cmd>`, or the equivalent `unshare(CLONE_NEWNET)` via `pre_exec` alongside the Landlock `pre_exec`). `none` therefore means genuinely no egress. For `allowlist`/`open`, the command keeps the host network; the `is_blocked_ip` ranges remain blocked **only** to smedja's own clients, and for the subprocess we treat `allowlist` as `open`-minus-blocked-ranges enforced at best effort — documented as a known limitation, because without a filtering proxy a raw socket cannot be per-destination filtered. If `unshare -n` is unavailable (no `CAP_NET_ADMIN` / no user namespaces), `none` cannot be honoured: under `SandboxMode::Required` this fails closed; under `Auto` it falls back per the existing contract with the unconfined marker; the `net_confined` telemetry attribute records `false`.
- **macOS Seatbelt**: `none → (deny network*)` is already produced by `network_rule` (`seatbelt.rs:37`); this change only confirms it remains the `none` mapping and is covered by a scenario. `allowlist`/`open → (allow network-outbound)` with the same documented `is_blocked_ip`-floor caveat.
- **Docker**: `none → --network none`, `allowlist`/`open → --network bridge` (already mapped, `docker.rs:73`); kept, with a scenario.
- Alternative considered: a transparent filtering proxy (e.g. an in-process CONNECT proxy the subprocess is pointed at via `HTTP(S)_PROXY`) to make `allowlist` real per-host. Rejected for this change as too large; recorded as the future path to a true subprocess allow-list. The honest statement stands: today `allowlist` for a subprocess == `open`-minus-blocked-ranges.

**Decision 3: Hardened by default whenever the sandbox is on; widen via config, never silently weaken.**
Read + network confinement apply whenever a backend is selected (i.e. `SMEDJA_SANDBOX_MODE != off` and a backend is available). There is no separate "hardening" opt-in flag — that would create a foot-gun where the sandbox looks on but secrets are readable. The existing `auto|required|off` semantics are preserved unchanged: `off` skips the sandbox entirely (and thus the hardening); `required` fails closed when confinement cannot be applied; `auto` falls back to the host with the unconfined marker. To avoid silently breaking workflows, the only knob is `SMEDJA_SANDBOX_READ_PATHS`, which *widens* the read allow-list. Network policy stays governed by the existing `SMEDJA_SANDBOX_NETWORK` (default `none`).
- Trade-off: defaulting `none` network + tightened reads is stricter than today and may surprise existing users. Mitigation: the failure surfaces as a clear command error naming the env var to widen reads, and `SMEDJA_SANDBOX_NETWORK=open` restores broad egress (minus the SSRF floor) for workflows that need it.

**Decision 4: Compose with `exec-sandbox`; extend the trait surface, do not replace it.**
This change keeps the `SandboxBackend` trait, `select_backend`, the fallback contract, and `resolve_confined_root` as-is. It changes only the *internals* of `apply`/`render_profile`/`exec` to enforce reads + network, plus additive telemetry: `SandboxTelemetry` gains `read_confined: bool` and `net_confined: bool`, threaded into the `smedja.sandbox.exec` span (`sandbox.rs:355`). The read-path resolution (`SMEDJA_SANDBOX_READ_PATHS` over the platform defaults) lives in `sandbox.rs` so all backends share one source of truth, mirroring how `NetworkPolicy`/`SandboxMode` are resolved there.

## Risks / Trade-offs

- [Risk] Tightened read allow-list breaks a program reading an unlisted path → Mitigation: `SMEDJA_SANDBOX_READ_PATHS` appends to the defaults; the failure mode and the exact env var are documented; non-existent default paths are skipped, not errored.
- [Risk] `unshare -n` unavailable on a hardened/older host → cannot honour `network=none` → Mitigation: `Required` fails closed, `Auto` falls back per the existing contract with the unconfined marker; `net_confined=false` is recorded in telemetry.
- [Risk] `allowlist` gives a false sense of per-host filtering for subprocesses → Mitigation: documented explicitly as `open`-minus-blocked-ranges; the proxy path is recorded as the future work for a real subprocess allow-list.
- [Risk] Stricter defaults surprise existing `exec-sandbox` users → Mitigation: confinement only applies when the sandbox is already on; `off` is unchanged; `SMEDJA_SANDBOX_NETWORK=open` and `SMEDJA_SANDBOX_READ_PATHS` restore broader access deliberately.
- [Risk] Two `pre_exec` closures on Linux (Landlock + netns) must both be async-signal-safe and ordered → Mitigation: apply the network namespace before the Landlock `restrict_self` in the same forked child; both call only async-signal-safe syscalls, matching the existing Landlock `pre_exec` safety note (`landlock_backend.rs:125-127`).

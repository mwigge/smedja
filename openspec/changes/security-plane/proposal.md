## Why

The Go predecessor (milliways) shipped a heavy security control plane: a startup posture scan over hooks/configs/IOC files, shell-command firewall shims, output scanning, SBOM generation, and a `security` audit subcommand. In practice that plane was tuned so aggressively that it blocked legitimate work — a known regression the project owner has explicitly called out: "tweaked too hard and blocks real work — do NOT go fully there."

smedja today has only three narrow controls and no coherent security surface:

- The cowork approval gate (`bin/smdjad/src/cowork.rs`) — human-in-the-loop, only active in cowork mode.
- A workspace-boundary check (`assert_within_workspace` in `bin/smdjad/src/executor/`).
- An SSRF guard (`is_safe_mcp_url`/`is_blocked_ip` in `bin/smdjad/src/main.rs`) used by the MCP handler.

Critically, a destructive-command classifier already exists in `crates/smedja-ingot/src/guard.rs` (`classify` → `Safe`/`Confirm`/`Blocked`) but is wired into nothing: a workspace grep finds no caller in the daemon. There is no posture scan, no output scanning, no SBOM, and no `smj security` report command. Operators cannot see what risks exist in their workspace, and the one classifier that does exist is dormant.

This change introduces a **proportionate, non-blocking-by-default** security plane: it surfaces findings as advisory audit events and observability signals, generates an SBOM, and adds a `smj security` report command. It deliberately does **not** reproduce milliways' over-strict gates — every enforcement path is opt-in behind an explicit config flag and is off by default.

## What Changes

- **Add a `smedja-security` workspace crate** that owns posture scanning, output scanning, SBOM assembly, and a finding model. It depends on `smedja-ingot` to emit findings as advisory `AuditEvent`s and reuses the existing `smedja_ingot::guard` classifier rather than duplicating a blocklist.
- **Startup posture scan (advisory)**: on daemon start, scan the workspace for risky hooks/configs and known IOC file markers, emitting one advisory audit event per finding with a severity. By default this only warns — it never aborts startup or blocks any action.
- **Output scanning (advisory)**: hook the executor tool-result return path so tool output is scanned for high-signal secret/credential patterns. A match emits an advisory finding and tags the audit event; by default the output is returned unmodified.
- **SBOM generation**: assemble a CycloneDX-style component list from the resolved `Cargo.lock` graph, written on demand via the CLI.
- **`smj security` subcommand**: a new top-level CLI command with `scan` (run a posture scan and print findings), `report` (summarise recorded findings for a session/workspace), and `sbom` (emit the SBOM document).
- **Opt-in enforcement**: a single explicit config flag (e.g. `[security] enforce = true`, default `false`) promotes advisory findings to blocks — a posture finding above a configured severity can refuse startup, an output-scan match can redact, and a `guard`-`Blocked` command can be refused. With the flag absent or `false`, nothing is ever blocked beyond today's behaviour.

Out of scope: network egress firewalling, sandboxed execution (owned by the `exec-sandbox` change), and replacing the cowork gate. The existing SSRF guard and workspace-boundary check stay where they are; this change references them as the current boundary controls but does not move them.

## Capabilities

### New Capabilities

- `security-posture`: a startup workspace posture scan that classifies hooks/configs/IOC markers and the existing command classifier into advisory findings, non-blocking by default, with opt-in enforcement.
- `output-scanning`: advisory scanning of tool-result output for secret/credential patterns on the executor return path, non-blocking by default, with opt-in redaction.
- `sbom`: CycloneDX-style SBOM assembly from the resolved dependency graph, emitted via `smj security sbom`.

## Impact

- `crates/smedja-security/` (new crate): finding model, posture scanner, output scanner, SBOM assembler; added to the root `Cargo.toml` `[workspace] members`.
- `crates/smedja-security/Cargo.toml`: depends on `smedja-ingot` (audit events + `guard`), `serde`/`serde_json`, `walkdir`, `sha2`, `toml`, `regex`, `thiserror`.
- `bin/smdjad/src/main.rs` / startup path: invoke the posture scan once at start; emit advisory findings; read the `[security]` config block.
- `bin/smdjad/src/executor/mod.rs`: call the output scanner on the tool-result return path before the result is recorded; advisory by default.
- `bin/smj/src/main.rs`: add the `Security` subcommand (`scan`/`report`/`sbom`) to the clap `Cmd` enum and its handlers.
- `crates/smedja-ingot/src/audit.rs`: advisory findings are recorded as `AuditEvent`s with a `security_finding` action type — no schema column change required (existing `action_type`/`status`/`error_kind` columns carry the finding).
- README/docs: document the security plane as advisory-by-default with opt-in enforcement.

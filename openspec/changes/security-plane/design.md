## Context

The Go predecessor (milliways) had a full security control plane that the project owner judged over-strict: posture scans, command firewall shims, output scanning, SBOM, and a `security` subcommand were all tuned to hard-block, and they blocked legitimate work. smedja deliberately ships much less today:

- `bin/smdjad/src/cowork.rs` — the cowork approval gate (`CoworkGate::intercept`), human-in-the-loop, only in cowork mode.
- `bin/smdjad/src/executor/` — `assert_within_workspace` (workspace-boundary check) and the tool dispatch in `executor/mod.rs::execute_tool` (where tool results are produced and returned).
- `bin/smdjad/src/main.rs` — `is_safe_mcp_url` / `is_blocked_ip` (SSRF guard), used by `bin/smdjad/src/handlers/mcp.rs`.
- `crates/smedja-ingot/src/guard.rs` — `classify(cmd) -> CommandRisk{Safe,Confirm,Blocked}`, re-exported from `smedja_ingot::lib` as `classify_command`/`command_is_safe`. A workspace grep finds **no caller** in the daemon: it is implemented and tested but dormant.
- `crates/smedja-ingot/src/audit.rs` — `AuditEvent` with `action_type`, `actor`, `status`, `error_kind`, `tool_name`, `traceparent` columns. The natural sink for advisory findings; no new column is needed.

The CLI surface is `bin/smj/src/main.rs`, a clap `Cmd` enum (`Daemon`, `Session`, `Audit`, `Cost`, …); adding a `Security` variant with subcommands matches the existing `Audit { action: AuditCmd }` pattern. The root `Cargo.toml` lists every crate under `crates/` in `[workspace] members`, so a new `smedja-security` crate fits the convention. `sha2`, `walkdir`, `regex`, `toml`, and `serde_json` are already workspace dependencies, so the new crate adds no third-party footprint.

## Goals / Non-Goals

Goals:
- Give operators visibility into workspace security posture, secret leakage in tool output, and the dependency SBOM.
- Reuse the existing `smedja_ingot::guard` classifier and `AuditEvent` sink rather than introducing a second blocklist or schema.
- Keep every control advisory by default; make enforcement a single explicit opt-in.
- Land the scan/scanner/SBOM as a self-contained `smedja-security` crate with thin daemon and CLI wiring.

Non-Goals:
- Network egress firewalling or DNS-level controls.
- Sandboxed / containerised execution (owned by the `exec-sandbox` change).
- Replacing or extending the cowork approval gate.
- Moving the SSRF guard or workspace-boundary check into the new crate (they stay where they are; this change references them as the current boundary).
- Cryptographic SBOM signing or attestation (CycloneDX document only).

## Decisions

### Decision: Non-blocking by default; enforcement opt-in (the milliways lesson)

This is the central design decision and the explicit contrast with milliways.

Every control in this plane is **advisory by default**. A posture-scan finding, an output-scan match, and a `guard`-`Blocked` command classification all produce an advisory `AuditEvent` (and an OTel warning span) and then **let the action proceed unchanged**. No finding aborts daemon startup, refuses a tool call, or redacts output unless the operator has explicitly turned enforcement on.

Enforcement is gated behind a single config flag in the `[security]` block — `enforce` (default `false`) plus an `enforce_min_severity` threshold (default the highest severity, so even when `enforce = true` only the most severe findings block). Resolution order: explicit config value, else the safe default of `enforce = false`. When the block is absent entirely, the plane behaves exactly as the advisory default.

- Rationale: milliways' identical capability set was "tweaked too hard and blocks real work." The capability is valuable; the default posture is what made it harmful. Inverting the default — observe first, enforce only on explicit opt-in — keeps the visibility while removing the failure mode. An operator who wants milliways-style hard gates can set `enforce = true`, but they do so knowingly and per-workspace.
- Rationale: advisory findings flow into the same `AuditEvent` timeline operators already inspect via `smj audit`, so the signal is discoverable without changing anyone's workflow.
- Alternative considered: enforce-by-default with an allowlist/bypass (the milliways model). Rejected — it reproduces the exact regression the owner called out; allowlists drift and the friction lands on every legitimate action.
- Alternative considered: a graduated default (warn for 30 days, then enforce). Rejected — implicit time-based escalation is surprising and still blocks work without an explicit operator decision.
- Consequence: tests MUST assert that, with enforcement off (the default), a finding is recorded **and the action is not blocked**. Each capability spec carries such a scenario.

### Decision: a dedicated `smedja-security` crate, not daemon-internal modules

Posture scanning, output scanning, the finding model, and SBOM assembly live in a new `crates/smedja-security` crate. The daemon and CLI depend on it; the daemon supplies the workspace root and config, the crate returns findings.

- Rationale: matches the workspace convention (one capability area per crate) and keeps the scanning logic unit-testable without a running daemon. It also lets `smj` run a posture scan or emit an SBOM without going through `smdjad`.
- Alternative considered: put the logic in `smedja-ingot` next to `guard`. Rejected — `smedja-ingot` is the persistence layer; scanning and SBOM are not persistence concerns. The new crate depends on `smedja-ingot` for the `AuditEvent` type and the `guard` classifier, keeping the dependency edge one-directional.

### Decision: findings are advisory `AuditEvent`s, no schema change

A finding maps to an `AuditEvent` with `action_type = "security_finding"`, `tool_name` carrying the rule id, `status = "warn"` (advisory) or `"blocked"` (enforced), and `error_kind` carrying the severity. This reuses the existing 23-column schema in `crates/smedja-ingot/src/audit.rs`.

- Rationale: zero migration cost; findings appear in the existing audit timeline and `smj audit query`; the distinction advisory-vs-enforced is captured by `status` without a new table.
- Alternative considered: a dedicated `security_findings` table. Rejected for now — it adds a migration and a second query path for what is conceptually an audit record. Can be revisited if findings need columns the audit schema lacks.

### Decision: reuse `smedja_ingot::guard`, do not add a second blocklist

The command-classification dimension of posture scanning calls `smedja_ingot::classify_command` rather than re-implementing patterns. The posture scan adds only the file-oriented checks (risky hooks/configs, IOC markers) that the classifier does not cover.

- Rationale: a single source of truth for command risk; the classifier is already tested. milliways' duplication of overlapping blocklists was part of what made it brittle.

### Decision: SBOM is CycloneDX-style, assembled from `Cargo.lock`

`smj security sbom` parses the resolved `Cargo.lock` into a CycloneDX-style component list (name, version, purl). It does not invoke `cargo` or network services.

- Rationale: `Cargo.lock` is the authoritative resolved graph and is already present; parsing it is deterministic and offline. CycloneDX is the de-facto interchange format and needs no signing to be useful as a report.
- Alternative considered: shell out to a `cargo sbom` / `cargo cyclonedx` plugin. Rejected — adds an external tool dependency and network/install surface for what is a `Cargo.lock` parse.

## Risks / Trade-offs

- [Risk] Output scanning adds latency to every tool result → Mitigation: a small set of compiled high-signal regexes over the result string; advisory path does no rewriting, only matching; scanning is skippable via a bypass env var consistent with the existing crusher/verbosity bypasses.
- [Risk] False-positive secret matches spam the audit log → Mitigation: high-signal patterns only (e.g. provider key prefixes, PEM headers), severity-tagged; advisory findings are deduplicated per (rule id, session) within a run.
- [Risk] An operator enables `enforce = true` and reproduces the milliways friction → Mitigation: `enforce_min_severity` defaults to the highest severity so even enabling enforcement blocks only the most severe findings; the default remains `enforce = false`; the behaviour is documented as opt-in.
- [Risk] Posture scan slows daemon startup → Mitigation: the scan is bounded to the workspace root, runs once, and is advisory — a scan error logs a warning and never aborts startup.
- [Risk] SBOM drifts from the actual build if `Cargo.lock` is stale → Mitigation: the SBOM documents the resolved lockfile and records the lockfile hash so the document is traceable to a specific resolution.

## 1. Scaffold the smedja-security crate

- [x] 1.1 Create `crates/smedja-security/Cargo.toml` depending on `smedja-ingot`, `serde`, `serde_json`, `walkdir`, `sha2`, `toml`, `regex`, `thiserror`; add `crates/smedja-security` to the root `Cargo.toml` `[workspace] members`
- [x] 1.2 Add the crate `lib.rs` with module stubs (`finding`, `posture`, `output`, `sbom`, `config`) and a crate-level error type via `thiserror`
- [x] 1.3 Run `cargo check -p smedja-security` to confirm the crate compiles and links

## 2. Finding model and config (advisory by default)

- [x] 2.1 Write a failing test asserting a `Finding` maps to an `AuditEvent` with `action_type = "security_finding"`, severity in `error_kind`, and `status = "warn"` when advisory
- [x] 2.2 Implement the `Finding` type (rule id, severity, message) and its `to_audit_event` conversion to make the test pass
- [x] 2.3 Write a failing test for `SecurityConfig` resolution: absent `[security]` block → `enforce = false`; present `enforce = true` → `enforce_min_severity` defaults to the highest severity
- [x] 2.4 Implement `SecurityConfig` parsing from the `[security]` TOML block to make the test pass
- [x] 2.5 Write a failing test asserting `Finding::status_for(config)` returns `"warn"` below threshold or when enforcement is off, and `"blocked"` only at/above threshold with `enforce = true`; implement to pass

## 3. Posture scan

- [x] 3.1 Write a failing test: scanning a fixture workspace with a flagged config/IOC marker yields a `Finding`, and scanning a clean workspace yields none
- [x] 3.2 Implement the file-oriented posture scan (`walkdir` over the workspace root, flagged-path and IOC-marker rules) to make the test pass
- [x] 3.3 Write a failing test asserting command-risk findings call `smedja_ingot::classify_command` (a `Blocked` command yields a highest-severity finding) and that no second blocklist is defined
- [x] 3.4 Implement the command-risk dimension by delegating to `smedja_ingot::classify_command`
- [x] 3.5 Write a failing test asserting a scan I/O error logs and returns the partial findings rather than erroring; implement the non-fatal error path

## 4. Output scanning

- [x] 4.1 Write a failing test: scanning a string containing a high-signal secret pattern yields a match with severity; a clean string yields none
- [x] 4.2 Implement the compiled high-signal secret/credential pattern set and the `scan_output` matcher to pass
- [x] 4.3 Write a failing test asserting `scan_output` returns content unmodified when enforcement is off, and redacts the matched span only when enforcement is on at/above severity; implement the advisory-vs-redact branch
- [x] 4.4 Write a failing test asserting the bypass env var skips scanning entirely; implement the bypass

## 5. SBOM assembly

- [x] 5.1 Write a failing test: parsing a fixture `Cargo.lock` yields one component per package with name/version and records the lockfile content hash
- [x] 5.2 Implement the `Cargo.lock` parse → CycloneDX-style component list with purls and lockfile hash to pass
- [x] 5.3 Write a failing test asserting a missing/unreadable lockfile produces a clear error (not a partial document); implement the error path

## 6. Daemon wiring (posture scan + output scanning)

- [x] 6.1 Add `smedja-security` as a dependency in `bin/smdjad/Cargo.toml`
- [x] 6.2 In the daemon startup path (`bin/smdjad/src/main.rs`), read the `[security]` config and run the posture scan once; emit each `Finding` as an advisory `AuditEvent`; assert in a test that startup completes and no finding aborts it
- [x] 6.3 In `bin/smdjad/src/executor/mod.rs`, call `scan_output` on the tool-result return path before the result is recorded; emit a finding on match; return content unmodified when enforcement is off
- [x] 6.4 Add an executor-level test asserting a secret-bearing tool result records a `security_finding` event AND returns the original content with the default (advisory) config

## 7. CLI: smj security

- [x] 7.1 Add a `Security { action: SecurityCmd }` variant to the `Cmd` enum in `bin/smj/src/main.rs` with `Scan`, `Report`, and `Sbom` subcommands
- [x] 7.2 Implement `cmd_security_scan` (run posture scan, print findings), `cmd_security_report` (summarise recorded `security_finding` audit events), and `cmd_security_sbom` (emit the SBOM document to stdout)
- [x] 7.3 Add a CLI test asserting `smj security sbom` writes a CycloneDX-style document and exits successfully, and `smj security report` runs as a read-only query

## 8. Verify

- [x] 8.1 Run `cargo test -p smedja-security` and `cargo test --workspace` — all green
- [x] 8.2 Confirm the non-blocking default: a workspace test asserts that with no `[security]` config, a high-severity finding is recorded with `status = "warn"` and no action is blocked
- [x] 8.3 Run `cargo clippy -p smedja-security -p smdjad -p smj -- -D warnings` clean for the touched code
- [x] 8.4 Run `openspec validate security-plane --strict` — clean

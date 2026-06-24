//! Daemon-side wiring for the advisory security plane.
//!
//! Reads the `[security]` config from the workspace, runs the startup posture
//! scan once, and records each [`smedja_security::Finding`] as an advisory
//! [`smedja_ingot::AuditEvent`]. The scan is non-blocking by design: a scan
//! error or any finding logs and is recorded, but never aborts startup.

use std::path::Path;

use smedja_ingot::IngotHandle;
use smedja_security::SecurityConfig;

/// Session id used to tag startup posture findings in the audit log.
const STARTUP_SESSION: &str = "smdjad-startup";

/// Resolves the [`SecurityConfig`] for `workspace_root`.
///
/// Reads `<workspace_root>/.smedja/config.toml` when present. A missing file or
/// an unparseable one resolves to the advisory default (`enforce = false`), so
/// the plane never blocks because of config trouble.
#[must_use]
pub fn load_security_config(workspace_root: &Path) -> SecurityConfig {
    let config_path = workspace_root.join(".smedja").join("config.toml");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return SecurityConfig::default();
    };
    match SecurityConfig::from_toml_str(&content) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(error = %e, path = %config_path.display(), "invalid [security] config; using advisory default");
            SecurityConfig::default()
        }
    }
}

/// Runs the startup posture scan over `workspace_root` and records each finding
/// as an advisory audit event.
///
/// Returns the number of findings recorded. This never aborts startup: the scan
/// itself is non-fatal on I/O errors, and findings are recorded as advisory
/// events whose `status` is resolved against `config` (always `"warn"` under the
/// default advisory config).
pub async fn run_startup_posture_scan(
    ingot: &IngotHandle,
    workspace_root: &Path,
    config: &SecurityConfig,
) -> usize {
    let root = workspace_root.to_path_buf();
    // The scan is synchronous filesystem work; keep it off the async executor.
    let findings =
        match tokio::task::spawn_blocking(move || smedja_security::scan_posture(&root)).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "startup posture scan task panicked; continuing");
                return 0;
            }
        };

    let mut recorded = 0;
    for finding in &findings {
        let event = finding.to_audit_event(STARTUP_SESSION, config);
        tracing::warn!(
            rule = %finding.rule_id,
            severity = %finding.severity.as_str(),
            status = %finding.status_for(config),
            message = %finding.message,
            "smedja.security.posture_finding"
        );
        if let Err(e) = ingot.insert_audit_event(event).await {
            tracing::warn!(error = %e, "failed to record posture finding; continuing");
        } else {
            recorded += 1;
        }
    }
    recorded
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_ingot::{Ingot, IngotHandle};

    #[test]
    fn missing_config_resolves_to_advisory_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_security_config(dir.path());
        assert!(!cfg.enforce);
    }

    #[test]
    fn present_enforce_config_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(smedja.join("config.toml"), "[security]\nenforce = true\n").unwrap();
        let cfg = load_security_config(dir.path());
        assert!(cfg.enforce);
    }

    #[tokio::test]
    async fn startup_scan_records_findings_without_aborting() {
        let dir = tempfile::tempdir().unwrap();
        // A flagged path produces a finding.
        std::fs::write(dir.path().join(".npmrc"), "//r/:_authToken=x").unwrap();

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let config = SecurityConfig::default();

        let recorded = run_startup_posture_scan(&ingot, dir.path(), &config).await;
        assert!(recorded >= 1, "at least one finding must be recorded");

        // The finding is advisory: recorded with status "warn".
        let events = ingot.list_audit_events(STARTUP_SESSION).await.unwrap();
        assert!(!events.is_empty());
        let ev = &events[0];
        assert_eq!(ev.action_type, "security_finding");
        assert_eq!(ev.status.as_deref(), Some("warn"));
    }

    #[tokio::test]
    async fn startup_scan_on_clean_workspace_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let config = SecurityConfig::default();
        let recorded = run_startup_posture_scan(&ingot, dir.path(), &config).await;
        assert_eq!(recorded, 0);
    }

    #[tokio::test]
    async fn high_severity_finding_is_warn_under_default_config_and_does_not_block() {
        // Non-blocking default proof at the daemon layer: an IOC marker (High)
        // under the default advisory config is recorded with status "warn" and
        // the scan returns normally (no abort, no block).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("reverse_shell.sh"), "#!/bin/sh\n").unwrap();
        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let config = SecurityConfig::default();

        let recorded = run_startup_posture_scan(&ingot, dir.path(), &config).await;
        assert!(recorded >= 1);

        let events = ingot.list_audit_events(STARTUP_SESSION).await.unwrap();
        let ioc = events
            .iter()
            .find(|e| e.tool_name.as_deref() == Some("ioc-marker"))
            .expect("ioc finding must be recorded");
        assert_eq!(ioc.error_kind.as_deref(), Some("high"));
        assert_eq!(
            ioc.status.as_deref(),
            Some("warn"),
            "high-severity finding must be advisory under the default config"
        );
    }
}

//! The finding model and its mapping onto advisory audit events.
//!
//! A [`Finding`] is advisory by default: [`Finding::to_audit_event`] records it
//! as a [`smedja_ingot::AuditEvent`] with `action_type = "security_finding"`,
//! the severity in `error_kind`, the rule id in `tool_name`, and a `status`
//! resolved by [`Finding::status_for`] against the active [`SecurityConfig`].

use smedja_ingot::AuditEvent;
use smedja_types::Timestamp;
use uuid::Uuid;

use crate::config::SecurityConfig;

/// `action_type` carried by every security finding audit event.
pub const ACTION_TYPE: &str = "security_finding";
/// `status` recorded for an advisory (non-blocking) finding.
pub const STATUS_WARN: &str = "warn";
/// `status` recorded for an enforced (blocking) finding.
pub const STATUS_BLOCKED: &str = "blocked";

/// Severity of a security finding, ordered from least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Informational; no action implied.
    Low,
    /// Worth attention but not urgent.
    Medium,
    /// Significant risk.
    High,
    /// The most severe class; the default enforcement threshold.
    Critical,
}

impl Severity {
    /// The highest severity, used as the default enforcement threshold so that
    /// enabling enforcement blocks only the most severe findings.
    #[must_use]
    pub const fn highest() -> Self {
        Self::Critical
    }

    /// Returns the lowercase string form recorded in `error_kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// A single security finding produced by a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Stable rule identifier (e.g. `"flagged-path"`, `"command-risk"`).
    pub rule_id: String,
    /// Severity of the finding.
    pub severity: Severity,
    /// Human-readable description of what was detected.
    pub message: String,
}

impl Finding {
    /// Constructs a finding from its parts.
    #[must_use]
    pub fn new(rule_id: impl Into<String>, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            rule_id: rule_id.into(),
            severity,
            message: message.into(),
        }
    }

    /// Resolves the finding's audit `status` against `config`.
    ///
    /// Returns [`STATUS_BLOCKED`] only when enforcement is enabled and the
    /// finding's severity is at or above the configured threshold; otherwise
    /// [`STATUS_WARN`]. With the default (advisory) config this is always
    /// [`STATUS_WARN`].
    #[must_use]
    pub fn status_for(&self, config: &SecurityConfig) -> &'static str {
        if config.blocks(self.severity) {
            STATUS_BLOCKED
        } else {
            STATUS_WARN
        }
    }

    /// Converts the finding into an advisory [`AuditEvent`] for `session_id`.
    ///
    /// The event carries `action_type = "security_finding"`, the rule id in
    /// `tool_name`, the severity in `error_kind`, and the `status` resolved by
    /// [`Finding::status_for`]. The `actor` is recorded as `"smedja-security"`.
    #[must_use]
    pub fn to_audit_event(&self, session_id: &str, config: &SecurityConfig) -> AuditEvent {
        AuditEvent {
            id: Uuid::new_v4(),
            ts: Timestamp::now(),
            session_id: session_id.to_owned(),
            action_type: ACTION_TYPE.to_owned(),
            actor: "smedja-security".to_owned(),
            tool_name: Some(self.rule_id.clone()),
            status: Some(self.status_for(config).to_owned()),
            error_kind: Some(self.severity.as_str().to_owned()),
            operation_name: Some(ACTION_TYPE.to_owned()),
            ..AuditEvent::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisory_finding_maps_to_warn_audit_event() {
        let finding = Finding::new("flagged-path", Severity::High, "risky config detected");
        let config = SecurityConfig::default();
        let ev = finding.to_audit_event("session-1", &config);

        assert_eq!(ev.action_type, "security_finding");
        assert_eq!(ev.error_kind.as_deref(), Some("high"));
        assert_eq!(ev.status.as_deref(), Some("warn"));
        assert_eq!(ev.tool_name.as_deref(), Some("flagged-path"));
        assert_eq!(ev.session_id, "session-1");
    }

    #[test]
    fn status_is_warn_when_enforcement_off() {
        let finding = Finding::new("command-risk", Severity::Critical, "blocked command");
        let config = SecurityConfig::default();
        assert_eq!(finding.status_for(&config), "warn");
    }

    #[test]
    fn status_is_warn_below_threshold_even_when_enforcing() {
        let finding = Finding::new("flagged-path", Severity::Low, "low risk");
        let config = SecurityConfig {
            enforce: true,
            enforce_min_severity: Severity::Critical,
        };
        assert_eq!(finding.status_for(&config), "warn");
    }

    #[test]
    fn status_is_blocked_at_or_above_threshold_when_enforcing() {
        let finding = Finding::new("command-risk", Severity::Critical, "blocked command");
        let config = SecurityConfig {
            enforce: true,
            enforce_min_severity: Severity::Critical,
        };
        assert_eq!(finding.status_for(&config), "blocked");
        let ev = finding.to_audit_event("s", &config);
        assert_eq!(ev.status.as_deref(), Some("blocked"));
    }
}

//! Persistence of audit findings as `smedja-ingot` `AuditEvent`s.
//!
//! Each run writes a `turn_start`/`turn_end` marker pair around one
//! `audit_finding` event per finding, so the timeline view sees every run even
//! when the finding set is empty.

use smedja_ingot::{AuditEvent, IngotHandle};
use smedja_rpc::{codes, RpcError};
use smedja_types::Timestamp;
use uuid::Uuid;

use super::findings::AuditFinding;

/// Builds the `turn_start`/`turn_end` marker event for an audit run.
fn marker_event(session_id: &str, action_type: &str) -> AuditEvent {
    AuditEvent {
        id: Uuid::new_v4(),
        ts: Timestamp::now(),
        session_id: session_id.to_owned(),
        action_type: action_type.to_owned(),
        actor: "review".to_owned(),
        tier: Some("deep".to_owned()),
        ..AuditEvent::default()
    }
}

/// Builds the `audit_finding` event for a single finding.
///
/// Column mapping: `tool_name = rule`, `operation_name = severity`,
/// `error_kind = file`, with the rationale carried in the turn record.
fn finding_event(session_id: &str, finding: &AuditFinding) -> AuditEvent {
    AuditEvent {
        id: Uuid::new_v4(),
        ts: Timestamp::now(),
        session_id: session_id.to_owned(),
        action_type: "audit_finding".to_owned(),
        actor: "review".to_owned(),
        tool_name: Some(finding.rule.clone()),
        tier: Some("deep".to_owned()),
        operation_name: Some(finding.severity.as_str().to_owned()),
        error_kind: Some(finding.file.clone()),
        status: Some("ok".to_owned()),
        ..AuditEvent::default()
    }
}

/// Persists `turn_start`/`turn_end` markers around the findings.
///
/// Each finding is written as an `audit_finding` `AuditEvent`. Markers are
/// written even when the finding set is empty so the timeline view sees the run.
///
/// # Errors
///
/// Returns an [`RpcError`] when an ingot write fails.
pub(crate) async fn persist_findings(
    ingot: &IngotHandle,
    session_id: &str,
    findings: &[AuditFinding],
) -> Result<(), RpcError> {
    ingot
        .insert_audit_event(marker_event(session_id, "turn_start"))
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
    for finding in findings {
        ingot
            .insert_audit_event(finding_event(session_id, finding))
            .await
            .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
    }
    ingot
        .insert_audit_event(marker_event(session_id, "turn_end"))
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::findings::Severity;
    use super::*;

    fn ingot() -> IngotHandle {
        IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap())
    }

    fn sample_findings() -> Vec<AuditFinding> {
        vec![
            AuditFinding {
                severity: Severity::Critical,
                file: "src/a.rs".to_owned(),
                line: Some(10),
                rule: "sql-injection".to_owned(),
                rationale: "interpolated SQL".to_owned(),
            },
            AuditFinding {
                severity: Severity::Low,
                file: "src/b.rs".to_owned(),
                line: None,
                rule: "naming".to_owned(),
                rationale: "abbreviated name".to_owned(),
            },
        ]
    }

    #[tokio::test]
    async fn persists_findings_as_audit_events_with_markers() {
        let ig = ingot();
        let sid = Uuid::new_v4().to_string();
        let findings = sample_findings();
        persist_findings(&ig, &sid, &findings).await.unwrap();

        let events = ig.list_audit_events(&sid).await.unwrap();
        let starts = events
            .iter()
            .filter(|e| e.action_type == "turn_start")
            .count();
        let ends = events
            .iter()
            .filter(|e| e.action_type == "turn_end")
            .count();
        assert_eq!(starts, 1, "exactly one turn_start marker");
        assert_eq!(ends, 1, "exactly one turn_end marker");

        let finding_events: Vec<&AuditEvent> = events
            .iter()
            .filter(|e| e.action_type == "audit_finding")
            .collect();
        assert_eq!(finding_events.len(), 2);
        let crit = finding_events
            .iter()
            .find(|e| e.error_kind.as_deref() == Some("src/a.rs"))
            .unwrap();
        assert_eq!(crit.actor, "review");
        assert_eq!(crit.tier.as_deref(), Some("deep"));
        assert_eq!(crit.tool_name.as_deref(), Some("sql-injection"));
        assert_eq!(crit.operation_name.as_deref(), Some("critical"));
    }

    #[tokio::test]
    async fn zero_findings_still_persists_markers() {
        let ig = ingot();
        let sid = Uuid::new_v4().to_string();
        persist_findings(&ig, &sid, &[]).await.unwrap();
        let events = ig.list_audit_events(&sid).await.unwrap();
        assert_eq!(events.len(), 2, "two markers, no findings");
        assert!(events.iter().any(|e| e.action_type == "turn_start"));
        assert!(events.iter().any(|e| e.action_type == "turn_end"));
    }
}

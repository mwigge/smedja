//! Persistence of audit results as ingot audit events (run markers and
//! per-finding records). Moved verbatim from `auditor.rs`.

use super::*;

// ── Persistence ──────────────────────────────────────────────────────────────

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

//! Advisory secret scanning applied to every tool result on the return path.

use smedja_ingot::{IngotHandle, Session};

/// Scans a tool result for secret patterns and records any match as an advisory
/// `security_finding` audit event, returning the content to surface to the
/// caller.
///
/// Advisory by default: with enforcement off (the default config) the original
/// `result` is returned unmodified and findings carry `status = "warn"`. When
/// the `[security]` config enforces at or above a match's severity, the matched
/// span is redacted and the finding carries `status = "blocked"`.
pub(crate) async fn scan_tool_output(
    result: &str,
    tool_name: &str,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
) -> String {
    let config = crate::security::load_security_config(workspace);
    let scan = smedja_security::scan_output(result, &config);
    if scan.is_clean() {
        return scan.content;
    }

    let session_id = session.map_or_else(|| "smdjad".to_owned(), |s| s.id.to_string());
    for finding in &scan.findings {
        tracing::warn!(
            tool = tool_name,
            rule = %finding.rule_id,
            severity = %finding.severity.as_str(),
            status = %finding.status_for(&config),
            "smedja.security.output_finding"
        );
        let mut event = finding.to_audit_event(&session_id, &config);
        event.tool_name = Some(tool_name.to_owned());
        if let Err(e) = ingot.insert_audit_event(event).await {
            tracing::warn!(error = %e, "failed to record output-scan finding; continuing");
        }
    }
    scan.content
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::executor::execute_tool;

    fn test_embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    #[tokio::test]
    async fn secret_bearing_tool_result_records_finding_and_returns_original() {
        use smedja_ingot::{Ingot, IngotHandle, Session};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        // A session ties the finding to a known session id for lookup.
        let session = Session {
            id: uuid::Uuid::new_v4(),
            created_at: smedja_types::Timestamp::from_micros(0),
            updated_at: smedja_types::Timestamp::from_micros(0),
            status: "active".to_owned(),
            task_id: None,
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        };
        let sid = session.id.to_string();

        // Write a file whose content contains a synthetic AWS-style key, then
        // read it back through the executor so the result carries the secret.
        let secret = "AKIAIOSFODNN7EXAMPLE";
        std::fs::write(ws.path().join("creds.txt"), format!("key={secret}")).unwrap();

        let result = execute_tool(
            "read_file",
            r#"{"path":"creds.txt"}"#,
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;

        // Advisory default: the original content is returned unmodified.
        assert!(
            result.contains(secret),
            "advisory default must return the original content; got: {result}"
        );

        // A security_finding event is recorded with status "warn".
        let events = ingot.list_audit_events(&sid).await.unwrap();
        let finding = events
            .iter()
            .find(|e| e.action_type == "security_finding")
            .expect("a security_finding event must be recorded");
        assert_eq!(finding.status.as_deref(), Some("warn"));
        assert_eq!(finding.tool_name.as_deref(), Some("read_file"));
    }

    #[tokio::test]
    async fn clean_tool_result_records_no_finding() {
        use smedja_ingot::{Ingot, IngotHandle, Session};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        let session = Session {
            id: uuid::Uuid::new_v4(),
            created_at: smedja_types::Timestamp::from_micros(0),
            updated_at: smedja_types::Timestamp::from_micros(0),
            status: "active".to_owned(),
            task_id: None,
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        };
        let sid = session.id.to_string();

        std::fs::write(ws.path().join("notes.txt"), "nothing secret here").unwrap();
        let result = execute_tool(
            "read_file",
            r#"{"path":"notes.txt"}"#,
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert_eq!(result, "nothing secret here");

        let events = ingot.list_audit_events(&sid).await.unwrap();
        assert!(
            events.iter().all(|e| e.action_type != "security_finding"),
            "clean output must record no security_finding"
        );
    }
}

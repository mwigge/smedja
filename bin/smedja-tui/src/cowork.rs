use serde_json::json;
use smedja_rpc::client::Client;

/// Reads the daemon's `resolved` flag from a `cowork.*` RPC result.
///
/// Returns `true` only when the response is `Ok` and carries `"resolved": true`.
/// A `resolved: false`, a missing field, or any transport error all yield `false`
/// so the caller keeps the pending item rather than dropping it silently.
pub(crate) fn cowork_resolved(result: &Result<serde_json::Value, smedja_rpc::RpcError>) -> bool {
    match result {
        Ok(v) => v
            .get("resolved")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Decides whether a cowork item should be removed and what transcript line to emit.
///
/// `success` is the confirmation text used when the daemon resolved the decision
/// (`approved: <tool>`, `denied: <tool>`, or `modify sent: <instruction>`). On
/// `resolved: false` the item is retained with an `item not found: <tool>` line;
/// on a transport error it is retained with a `<method> error: <e>` line. Returns
/// `(remove, message)`.
pub(crate) fn apply_cowork_decision(
    result: &Result<serde_json::Value, smedja_rpc::RpcError>,
    method: &str,
    success: &str,
    tool: &str,
) -> (bool, String) {
    match result {
        Ok(_) if cowork_resolved(result) => (true, success.to_owned()),
        Ok(_) => (false, format!("item not found: {tool}")),
        Err(e) => (false, format!("{method} error: {e}")),
    }
}

/// Sends a `cowork.*` decision RPC, injecting `session_id` into `params`.
///
/// Returns the raw RPC result so the caller can both check the `resolved` flag
/// (via [`cowork_resolved`]) and surface the appropriate transcript line. The
/// `session_id` is merged into `params` so call sites pass only the decision
/// fields (`id`, optional `reason`/`instruction`).
pub(crate) async fn resolve_cowork(
    client: &mut Client,
    session_id: &str,
    method: &str,
    mut params: serde_json::Value,
) -> Result<serde_json::Value, smedja_rpc::RpcError> {
    if let Some(obj) = params.as_object_mut() {
        obj.insert("session_id".to_owned(), json!(session_id));
    }
    client.call(method, params).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::cowork_widget;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

    fn cowork_item(id: &str, tool: &str) -> cowork_widget::CoworkItem {
        cowork_widget::CoworkItem {
            id: id.to_owned(),
            tool: tool.to_owned(),
            step_n: 1,
            args_display: String::new(),
            reasoning: String::new(),
        }
    }

    #[test]
    fn cowork_resolved_true_only_when_flag_set() {
        let yes: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": true }));
        assert!(cowork_resolved(&yes), "resolved:true must return true");

        let no: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": false }));
        assert!(!cowork_resolved(&no), "resolved:false must return false");

        let missing: Result<serde_json::Value, smedja_rpc::RpcError> = Ok(json!({ "id": "a" }));
        assert!(
            !cowork_resolved(&missing),
            "missing resolved field must return false"
        );

        let err: Result<serde_json::Value, smedja_rpc::RpcError> =
            Err(smedja_rpc::RpcError::new(-32603, "transport down"));
        assert!(!cowork_resolved(&err), "transport error must return false");
    }

    #[test]
    fn apply_cowork_decision_approve_resolved_removes_and_confirms() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": true }));
        let item = cowork_item("a", "bash");
        let (remove, message) =
            apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
        assert!(remove, "resolved:true must remove the item");
        assert_eq!(message, "approved: bash");
    }

    #[test]
    fn apply_cowork_decision_unresolved_retains_and_reports_not_found() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": false }));
        let item = cowork_item("a", "bash");
        let (remove, message) =
            apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
        assert!(!remove, "resolved:false must retain the item");
        assert_eq!(message, "item not found: bash");
    }

    #[test]
    fn apply_cowork_decision_deny_resolved_removes_and_confirms() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": true }));
        let item = cowork_item("a", "edit_file");
        let (remove, message) =
            apply_cowork_decision(&result, "cowork.deny", "denied: edit_file", &item.tool);
        assert!(remove, "resolved:true must remove the item");
        assert_eq!(message, "denied: edit_file");
    }

    #[test]
    fn apply_cowork_decision_rpc_error_retains_and_reports_error() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Err(smedja_rpc::RpcError::new(-32603, "boom"));
        let item = cowork_item("a", "bash");
        let (remove, message) =
            apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
        assert!(!remove, "rpc error must retain the item");
        assert!(
            message.contains("cowork.approve error"),
            "error message must name the method; got: {message}"
        );
    }

    #[test]
    fn apply_cowork_decision_modify_resolved_echoes_instruction() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": true }));
        let item = cowork_item("a", "bash");
        let (remove, message) = apply_cowork_decision(
            &result,
            "cowork.modify",
            "modify sent: use ls -la instead",
            &item.tool,
        );
        assert!(remove, "resolved:true must remove the item");
        assert_eq!(message, "modify sent: use ls -la instead");
    }

    #[test]
    fn apply_cowork_decision_modify_unresolved_retains_item() {
        let result: Result<serde_json::Value, smedja_rpc::RpcError> =
            Ok(json!({ "id": "a", "resolved": false }));
        let item = cowork_item("a", "bash");
        let (remove, message) = apply_cowork_decision(
            &result,
            "cowork.modify",
            "modify sent: use ls -la instead",
            &item.tool,
        );
        assert!(!remove, "resolved:false must retain the item");
        assert_eq!(message, "item not found: bash");
    }
}

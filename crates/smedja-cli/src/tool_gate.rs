use super::*;

pub(crate) async fn cmd_tool_gate(sock: &std::path::Path) {
    use std::io::Read as _;
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);

    // smedja's session id comes from the env the adapter set, falling back to
    // the hook payload's own session_id.
    let session_id = std::env::var("SMEDJA_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            input
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let tool_name = input
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tool_input = input
        .get("tool_input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    // `updated_input` is only present when the user chose *modify* with a JSON
    // object of replacement arguments; it maps to the PreToolUse hook's
    // `updatedInput` field so claude re-runs the call with the rewritten args.
    let (decision, reason, updated_input) = match Client::connect(sock).await {
        Ok(mut client) => match client
            .call(
                "cowork.gate_tool",
                json!({
                    "session_id": session_id,
                    "tool_name": tool_name,
                    "tool_input": tool_input,
                }),
            )
            .await
        {
            Ok(v) => (
                v.get("decision")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("allow")
                    .to_owned(),
                v.get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                v.get("updated_input").filter(|ui| ui.is_object()).cloned(),
            ),
            Err(_) => (
                "deny".to_owned(),
                "smedja approval interrupted (daemon connection lost) — denied".to_owned(),
                None,
            ),
        },
        Err(_) => (
            "allow".to_owned(),
            "smedja gate unreachable; allowing".to_owned(),
            None,
        ),
    };

    let mut hook_output = json!({
        "hookEventName": "PreToolUse",
        "permissionDecision": decision,
        "permissionDecisionReason": reason,
    });
    if let Some(updated) = updated_input {
        hook_output["updatedInput"] = updated;
    }
    let out = json!({ "hookSpecificOutput": hook_output });
    println!("{out}");
}

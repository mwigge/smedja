//! `smj tool-gate` — internal Claude Code `PreToolUse` approval hook.

use std::path::Path;

use serde_json::json;
use smedja_rpc::client::Client;

/// `smj tool-gate`: Claude Code `PreToolUse` hook. Reads the hook payload from
/// stdin, asks the daemon (`cowork.gate_tool`) whether the tool may run —
/// blocking on the user when the policy says "ask" — and emits the `PreToolUse`
/// permission decision on stdout.
///
/// Fails OPEN (allow) when the daemon is unreachable so a misconfigured gate
/// never bricks the agent; fails CLOSED (deny) if the connection drops
/// mid-decision, because the tool was explicitly pending human review.
pub(crate) async fn run(sock: &Path) {
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

    let (decision, reason) = match Client::connect(sock).await {
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
            ),
            Err(_) => (
                "deny".to_owned(),
                "smedja approval interrupted (daemon connection lost) — denied".to_owned(),
            ),
        },
        Err(_) => (
            "allow".to_owned(),
            "smedja gate unreachable; allowing".to_owned(),
        ),
    };

    let out = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    });
    println!("{out}");
}

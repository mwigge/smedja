//! Response formatters for the slash-command system: render RPC/session
//! payloads (model lists, memory, agents, metrics, approvals) into display
//! strings. Moved verbatim from `slash.rs`.

use serde_json::Value;

pub(crate) fn format_model_list(v: &serde_json::Value) -> String {
    let runners = v.get("runners").and_then(|r| r.as_array());
    let Some(runners) = runners else {
        return "no runners available".to_owned();
    };
    let mut lines = vec!["available models:".to_owned()];
    for r in runners {
        let runner = r.get("runner").and_then(|v| v.as_str()).unwrap_or("?");
        let tier = r.get("tier").and_then(|v| v.as_str()).unwrap_or("?");
        let model = r.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        lines.push(format!("  {runner} ({tier}): {model}"));
    }
    lines.join("\n")
}

/// Renders the `local.models` response: the GPU-annotated local-model inventory.
///
/// Each line shows the model id, its advisory GPU fit, and an active marker.
pub(crate) fn format_local_model_list(v: &serde_json::Value) -> String {
    let Some(models) = v.get("models").and_then(|m| m.as_array()) else {
        return "no local models".to_owned();
    };
    if models.is_empty() {
        return "no local models".to_owned();
    }
    let mut lines = vec!["local models:".to_owned()];
    for m in models {
        let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let fit = m.get("fit").and_then(|v| v.as_str()).unwrap_or("unknown");
        let marker = if m.get("active").and_then(serde_json::Value::as_bool) == Some(true) {
            " *"
        } else {
            ""
        };
        lines.push(format!("  {id} [{fit}]{marker}"));
    }
    lines.join("\n")
}

/// Renders a `session.history` response as a compact, readable memory listing:
/// one entry per stored turn (user prompt + assistant reply previews) plus an
/// audit-trail count. Used by `/memory`.
pub(crate) fn format_memory(history: &Value, context: Option<&Value>, sid: &str) -> String {
    let short: String = sid.chars().take(8).collect();
    let mut lines = vec![format!("\u{25a4} memory · session {short}")];

    // Short-term working set + semantic vault (from session.context).
    if let Some(ctx) = context {
        let used = ctx.get("used_tok").and_then(Value::as_u64).unwrap_or(0);
        let window = ctx.get("window_tok").and_then(Value::as_u64).unwrap_or(0);
        let warm = ctx
            .get("vault_warm_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cold = ctx
            .get("vault_cold_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let pct = used.saturating_mul(100).checked_div(window).unwrap_or(0);
        lines.push(format!(
            "  short-term · context {used}/{window} tok ({pct}%)"
        ));
        lines.push(format!(
            "  vault · {warm} warm + {cold} cold semantic entries"
        ));
    }

    let turns = history.get("turns").and_then(Value::as_array);
    match turns {
        Some(turns) if !turns.is_empty() => {
            lines.push(format!(
                "{} stored turn(s) — long-term memory:",
                turns.len()
            ));
            for t in turns {
                let n = t.get("turn_n").and_then(Value::as_u64).unwrap_or(0);
                let msgs = t.get("messages").and_then(Value::as_array);
                let (mut user, mut asst) = (String::new(), String::new());
                if let Some(msgs) = msgs {
                    for m in msgs {
                        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
                        let msg_text = m.get("content").and_then(Value::as_str).unwrap_or("");
                        let preview: String =
                            msg_text.split_whitespace().collect::<Vec<_>>().join(" ");
                        let preview: String = preview.chars().take(72).collect();
                        if role == "user" && user.is_empty() {
                            user = preview;
                        } else if role == "assistant" && asst.is_empty() {
                            asst = preview;
                        }
                    }
                }
                lines.push(format!("  #{n} \u{25b8} {user}"));
                if !asst.is_empty() {
                    lines.push(format!("      \u{21b3} {asst}"));
                }
            }
        }
        _ => lines.push("  (no stored turns yet — memory fills as turns complete)".to_owned()),
    }

    if let Some(audit) = history.get("audit").and_then(Value::as_array) {
        if !audit.is_empty() {
            lines.push(format!(
                "{} audit event(s) in the tool/turn trail",
                audit.len()
            ));
        }
    }
    lines.push("tip: /memory <session_id> views another session's memory".to_owned());
    lines.join("\n")
}

pub(crate) fn format_agents_table(v: &serde_json::Value) -> String {
    let runners = v.get("runners").and_then(|r| r.as_array());
    let Some(runners) = runners else {
        return "no runners configured".to_owned();
    };
    if runners.is_empty() {
        return "no runners available".to_owned();
    }
    let mut lines = vec![
        format!(" {:<14} {:<8} {}", "runner", "tier", "model"),
        format!(" {}", "─".repeat(60)),
    ];
    for r in runners {
        let runner = r.get("runner").and_then(|v| v.as_str()).unwrap_or("?");
        let tier = r.get("tier").and_then(|v| v.as_str()).unwrap_or("?");
        let model = r.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        lines.push(format!(" {runner:<14} {tier:<8} {model}"));
    }
    lines.join("\n")
}

pub(crate) fn format_metrics(
    usage: &Result<serde_json::Value, smedja_rpc::RpcError>,
    cost: &Result<serde_json::Value, smedja_rpc::RpcError>,
    session_id: &str,
) -> String {
    let (turn_count, total_input, total_output) = match usage {
        Ok(v) => {
            let turns = v.get("turns").and_then(|t| t.as_array());
            turns.map_or((0usize, 0i64, 0i64), |arr| {
                let last = arr.last();
                let total_in = last
                    .and_then(|r| r.get("cumulative_input"))
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let total_out = last
                    .and_then(|r| r.get("cumulative_output"))
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                (arr.len(), total_in, total_out)
            })
        }
        Err(_) => (0, 0, 0),
    };
    let cost_usd = match cost {
        Ok(v) => v
            .get("total_usd")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0),
        Err(_) => 0.0,
    };
    let total_tok = total_input.saturating_add(total_output);
    format!(
        "session: {session_id}\n\
         turns: {turn_count}   tokens: {total_tok}\n\
         cost: ${cost_usd:.4}   input: {total_input}   output: {total_output}"
    )
}

pub(crate) fn format_approvals_list(v: &serde_json::Value) -> String {
    let items = v.as_array();
    let Some(items) = items else {
        return "cowork: unexpected response format".to_owned();
    };
    if items.is_empty() {
        return "cowork: no pending approvals".to_owned();
    }
    let mut lines = vec!["pending approvals:".to_owned()];
    for item in items {
        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let tool = item.get("tool").and_then(|v| v.as_str()).unwrap_or("?");
        let args = item.get("args").and_then(|v| v.as_str()).unwrap_or("");
        lines.push(format!("  [{id}] {tool}: {args}"));
    }
    lines.push("use /approve <id> to approve".to_owned());
    lines.join("\n")
}

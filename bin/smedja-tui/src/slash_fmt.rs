//! Pure formatting/listing helpers used by [`crate::slash::dispatch_slash`].
use std::path::{Path, PathBuf};

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

/// Home directory (`$HOME`, falling back to `.`).
pub(crate) fn home_dir() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("."), PathBuf::from)
}

/// Lists skill names in `dir`: `<name>.md` → `name`, `<name>/SKILL.md` → `name`.
pub(crate) fn list_skill_dir(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                if path.join("SKILL.md").exists() {
                    if let Some(n) = path.file_name().and_then(|s| s.to_str()) {
                        out.push(n.to_owned());
                    }
                }
            } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Some(n) = path.file_stem().and_then(|s| s.to_str()) {
                    out.push(n.to_owned());
                }
            }
        }
    }
    out.sort();
    out
}

/// Copies every `*.md` from `src` into the workspace skills dir `dst`, creating
/// `dst` as needed. Returns a status string.
pub(crate) fn install_skills_dir(src: &Path, dst: &Path) -> String {
    if !src.is_dir() {
        return format!("skills: {} is not a directory", src.display());
    }
    if std::fs::create_dir_all(dst).is_err() {
        return "skills: cannot create .smedja/skills".to_owned();
    }
    let mut n = 0u32;
    if let Ok(rd) = std::fs::read_dir(src) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Some(name) = p.file_name() {
                    if std::fs::copy(&p, dst.join(name)).is_ok() {
                        n += 1;
                    }
                }
            }
        }
    }
    format!(
        "\u{2713} installed {n} skill file(s) into {} — auto-injected next turn",
        dst.display()
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use serde_json::{json, Value};

    #[test]
    fn format_memory_lists_turns_with_previews() {
        let history = json!({
            "turns": [
                { "turn_n": 1, "messages": [
                    {"role": "user", "content": "write a counter"},
                    {"role": "assistant", "content": "here is the code"}
                ]}
            ],
            "audit": [ {"x": 1} ]
        });
        let ctx = json!({ "used_tok": 50, "window_tok": 200, "vault_warm_count": 3, "vault_cold_count": 7 });
        let out = crate::slash_fmt::format_memory(&history, Some(&ctx), "abcd1234ef");
        assert!(out.contains("memory"), "{out}");
        assert!(out.contains("abcd1234"), "{out}"); // short session id
        assert!(out.contains("write a counter"), "{out}");
        assert!(out.contains("here is the code"), "{out}");
        assert!(out.contains("1 audit event"), "{out}");
        assert!(out.contains("/memory <session_id>"), "{out}");
        // Short-term context + vault summary present.
        assert!(out.contains("50/200 tok (25%)"), "{out}");
        assert!(out.contains("3 warm + 7 cold"), "{out}");
    }

    #[test]
    fn skills_listing_and_install_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        // A source dir with two skill .md files + a non-md file.
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("alpha.md"), "skill a").unwrap();
        std::fs::write(src.join("beta.md"), "skill b").unwrap();
        std::fs::write(src.join("notes.txt"), "ignore").unwrap();
        // Directory-form skill.
        let gamma = src.join("gamma");
        std::fs::create_dir_all(&gamma).unwrap();
        std::fs::write(gamma.join("SKILL.md"), "skill g").unwrap();

        let names = crate::slash_fmt::list_skill_dir(&src);
        assert!(names.contains(&"alpha".to_owned()), "{names:?}");
        assert!(names.contains(&"gamma".to_owned()), "{names:?}"); // dir/SKILL.md
        assert!(!names.iter().any(|n| n == "notes"), "{names:?}"); // .txt ignored

        let dst = tmp.path().join(".smedja").join("skills");
        let msg = crate::slash_fmt::install_skills_dir(&src, &dst);
        assert!(msg.contains("installed 2 skill file"), "{msg}"); // alpha.md + beta.md
        assert!(dst.join("alpha.md").exists());
    }

    #[test]
    fn format_memory_handles_empty_history() {
        let out = crate::slash_fmt::format_memory(&json!({ "turns": [] }), None, "sess0001");
        assert!(out.contains("no stored turns"), "{out}");
    }

    #[test]
    fn format_agents_table_renders_header_and_rows() {
        let v = serde_json::json!({
            "runners": [
                { "runner": "claude-cli", "tier": "fast", "model": "claude-haiku-4-5-20251001" },
                { "runner": "claude-cli", "tier": "deep", "model": "claude-sonnet-4-6" },
            ]
        });
        let out = format_agents_table(&v);
        assert!(out.contains("runner"), "header must include 'runner'");
        assert!(out.contains("claude-cli"), "table must list runner name");
        assert!(out.contains("fast"), "table must list tier");
        assert!(
            out.contains("claude-haiku-4-5-20251001"),
            "table must list model"
        );
    }

    #[test]
    fn format_agents_table_empty_runners_returns_message() {
        let v = serde_json::json!({ "runners": [] });
        let out = format_agents_table(&v);
        assert!(out.contains("no runners"), "empty pool must say no runners");
    }

    #[test]
    fn format_metrics_aggregates_token_and_cost_data() {
        let usage = Ok(serde_json::json!({
            "session_id": "sess-1",
            "turns": [
                { "turn_n": 1, "input_tok": 100, "output_tok": 50, "cumulative_input": 100, "cumulative_output": 50 }
            ]
        }));
        let cost = Ok(serde_json::json!({
            "session_id": "sess-1",
            "total_usd": 0.0025,
            "breakdown": []
        }));
        let out = format_metrics(&usage, &cost, "sess-1");
        assert!(out.contains("sess-1"), "metrics must include session id");
        assert!(out.contains("turns: 1"), "metrics must include turn count");
        assert!(out.contains("0.0025"), "metrics must include cost");
    }

    #[test]
    fn format_metrics_handles_rpc_errors_gracefully() {
        let usage: Result<serde_json::Value, smedja_rpc::RpcError> =
            Err(smedja_rpc::RpcError::new(-32600, "unavailable"));
        let cost: Result<serde_json::Value, smedja_rpc::RpcError> =
            Err(smedja_rpc::RpcError::new(-32600, "unavailable"));
        let out = format_metrics(&usage, &cost, "sess-err");
        assert!(
            out.contains("sess-err"),
            "metrics must still show session id on error"
        );
    }

    #[test]
    fn format_approvals_list_shows_items() {
        let v = serde_json::json!([
            { "id": "item-1", "tool": "Bash", "args": "git push origin main", "step_n": 1 }
        ]);
        let out = format_approvals_list(&v);
        assert!(out.contains("item-1"), "must include id");
        assert!(out.contains("Bash"), "must include tool name");
        assert!(out.contains("git push"), "must include args");
        assert!(out.contains("/approve"), "must include usage hint");
    }

    #[test]
    fn format_approvals_list_empty_shows_no_pending_message() {
        let v = serde_json::json!([]);
        let out = format_approvals_list(&v);
        assert!(
            out.contains("no pending"),
            "empty list must say no pending approvals"
        );
    }

    #[test]
    fn format_model_list_renders_all_entries() {
        let v = serde_json::json!({
            "runners": [
                { "runner": "claude-cli", "tier": "fast", "model": "claude-haiku-4-5-20251001" }
            ]
        });
        let out = format_model_list(&v);
        assert!(out.contains("claude-cli"), "must include runner name");
        assert!(out.contains("fast"), "must include tier");
        assert!(
            out.contains("claude-haiku-4-5-20251001"),
            "must include model"
        );
    }

    #[test]
    fn format_local_model_list_renders_fit_and_active() {
        let v = serde_json::json!({
            "active_model_id": "qwen3-14b",
            "models": [
                { "id": "qwen3-14b", "fit": "fits", "active": true },
                { "id": "huge-70b", "fit": "exceeds", "active": false }
            ]
        });
        let out = format_local_model_list(&v);
        assert!(out.contains("qwen3-14b") && out.contains("[fits]"));
        assert!(out.contains('*'), "active model must be marked");
        assert!(out.contains("huge-70b") && out.contains("[exceeds]"));
    }
}

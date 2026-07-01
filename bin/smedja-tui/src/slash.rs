//! Slash-command dispatch and its exclusive formatting helpers.
//!
//! [`dispatch_slash`] is the central handler for all `/cmd` inputs in the TUI.
//! It returns `Ok(true)` when input was consumed as a slash command, `Ok(false)`
//! when the input does not start with `/` (caller should send it as a normal
//! turn instead).
//!
//! The pure-formatting helpers in this module are only called from within
//! `dispatch_slash`; they live here to keep `main.rs` focused on wiring.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};
use smedja_rpc::client::Client;

use crate::{
    detect_project_types, fetch_latest_version, format_gov_list, format_openspec_list,
    format_openspec_status, format_resume_rows, format_token_count, gov_create, gov_transition,
    is_newer, parse_resume_args, parse_review_scope, push_system_message, render_findings_summary,
    resume_blocked_by_pending_turn, resume_into_view, resume_plan, run_openspec, run_upgrade,
    scan_gov_artifacts, slugify, submit, AppState, OutputType, HELP_TEXT, VERSION,
};

/// Sets the session tier and returns a status string.
pub(crate) fn apply_tier(args: &str, state: &mut AppState) -> String {
    match args {
        "fast" | "deep" | "local" => {
            state.tier = Some(args.to_owned());
            format!("tier set to {args}")
        }
        "" => "usage: /tier fast|deep|local".to_owned(),
        other => format!("unknown tier: {other}"),
    }
}

/// Sets the agent mode on `state` and returns a status string.
pub(crate) fn apply_agent(args: &str, state: &mut AppState) -> String {
    match args {
        "impl" | "review" | "test" | "sre" | "explain" => {
            state.mode = Some(args.to_owned());
            if args == "sre" {
                state.tier = Some("deep".to_owned());
            }
            format!("agent mode set to {args}")
        }
        "" => "usage: /agent impl|review|test|sre|explain".to_owned(),
        other => format!("unknown agent mode: {other}"),
    }
}

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
fn home_dir() -> PathBuf {
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

#[allow(clippy::too_many_lines)] // flat slash-command dispatch table; splitting is out of scope here
pub(crate) async fn dispatch_slash(
    input: &str,
    state: &mut AppState,
    client: &mut Client,
) -> Result<bool> {
    let trimmed = input.trim();
    let Some(command_line) = trimmed.strip_prefix('/') else {
        return Ok(false);
    };
    let mut parts = command_line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or_default();
    let args = parts.next().unwrap_or_default().trim();

    match cmd {
        "tier" => {
            let text = apply_tier(args, state);
            push_system_message(state, text);
            // Make the tier meaningful: ask the daemon to resolve (runner, tier)
            // → model and pin it, so turns actually run on the chosen tier's
            // model (and it persists across restarts).
            if matches!(args, "fast" | "deep" | "local") {
                match client
                    .call(
                        "session.set_tier",
                        json!({ "session_id": state.session_id, "tier": args }),
                    )
                    .await
                {
                    Ok(v) => {
                        if let Some(m) = v.get("model").and_then(Value::as_str) {
                            state.model = Some(m.to_owned());
                            push_system_message(state, format!("model → {m}"));
                        }
                    }
                    Err(e) => push_system_message(state, format!("session.set_tier error: {e}")),
                }
            }
            Ok(true)
        }
        // `/memory` lists this session's stored memory (the long-term turn
        // history); `/memory <session_id>` views ANOTHER session's memory — e.g.
        // a new runner (codex) inspecting work a prior runner (claude) left
        // behind. This is the cross-client memory hand-off surface.
        "memory" => {
            let sid = if args.is_empty() {
                state.session_id.clone()
            } else {
                args.to_owned()
            };
            let ctx = client
                .call("session.context", json!({ "session_id": sid }))
                .await
                .ok();
            let text = match client
                .call("session.history", json!({ "session_id": sid }))
                .await
            {
                Ok(v) => format_memory(&v, ctx.as_ref(), &sid),
                Err(e) => format!("memory: session.history error: {e}"),
            };
            push_system_message(state, text);
            Ok(true)
        }
        // `/index [path]` builds the code graph for the workspace (defaults to the
        // TUI's working directory, NOT the daemon's). The graph is auto-injected
        // into the agent's context once built.
        "index" => {
            // Require an explicit path: defaulting to the TUI's cwd silently
            // indexed the wrong (often huge, e.g. $HOME) tree and hung.
            if args.trim().is_empty() {
                push_system_message(
                    state,
                    "/index error: <path to repo missing> — usage: /index <path>".to_owned(),
                );
                return Ok(true);
            }
            let workspace = args.to_owned();
            push_system_message(state, format!("indexing code graph: {workspace}\u{2026}"));
            let text = match client
                .call("graph.index", json!({ "workspace": workspace }))
                .await
            {
                Ok(v) => {
                    let n = v.get("indexed").and_then(Value::as_u64).unwrap_or(0);
                    let ws = v
                        .get("workspace")
                        .and_then(Value::as_str)
                        .unwrap_or(&workspace);
                    state.graph_symbols = usize::try_from(n).ok();
                    // Remember this as the workspace whose graph status the
                    // right-bar tracks (survives across the rest of the session).
                    state.graph_workspace = Some(ws.to_owned());
                    format!(
                        "\u{25c6} code graph: {n} symbols indexed ({ws}) — auto-injected into agent context"
                    )
                }
                Err(e) => format!("graph.index error: {e}"),
            };
            push_system_message(state, text);
            Ok(true)
        }
        // `/skills` lists available skills (global ~/.claude/skills + workspace
        // .smedja/skills); `/skills add <dir>` copies *.md from a directory into
        // the workspace skills folder. Skills are auto-injected into agent context.
        // `/tools` lists recent tool calls with fuller args than the inline card
        // — the always-works backup to right-clicking a card for the overlay.
        "tools" => {
            if state.tool_details.is_empty() {
                push_system_message(state, "no tool calls this session yet");
                return Ok(true);
            }
            let mut lines = vec![format!(
                "\u{2692} {} tool call(s) — right-click a card for full args",
                state.tool_details.len()
            )];
            let start = state.tool_details.len().saturating_sub(12);
            for (i, (_, name, full)) in state.tool_details.iter().enumerate().skip(start) {
                let preview: String = full
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(160)
                    .collect();
                lines.push(format!("  [{i}] {name} \u{00b7} {preview}"));
            }
            push_system_message(state, lines.join("\n"));
            Ok(true)
        }
        "skills" | "skill" => {
            let global_dir = home_dir().join(".claude").join("skills");
            let ws_dir = std::env::current_dir()
                .unwrap_or_default()
                .join(".smedja")
                .join("skills");
            if let Some(add_path) = args.strip_prefix("add ") {
                let text = install_skills_dir(Path::new(add_path.trim()), &ws_dir);
                push_system_message(state, text);
                return Ok(true);
            }
            let global = list_skill_dir(&global_dir);
            let ws = list_skill_dir(&ws_dir);
            let fmt = |v: &[String]| {
                if v.is_empty() {
                    "(none)".to_owned()
                } else {
                    v.join(", ")
                }
            };
            let text = [
                "\u{2692} skills (auto-injected into agent context)".to_owned(),
                format!("  global  ~/.claude/skills : {}", fmt(&global)),
                format!("  work .smedja/skills      : {}", fmt(&ws)),
                "add: /skills add <dir>  (copies *.md into .smedja/skills)".to_owned(),
            ]
            .join("\n");
            push_system_message(state, text);
            Ok(true)
        }
        "agent" => {
            if args.is_empty() {
                let result = client.call("runner.list", json!({})).await;
                let text = match result {
                    Ok(v) => format_agents_table(&v),
                    Err(e) => format!("runner.list error: {e}"),
                };
                push_system_message(state, text);
            } else {
                let text = apply_agent(args, state);
                if matches!(args, "impl" | "review" | "test" | "sre" | "explain") {
                    let session_id = state.session_id.clone();
                    let _ = client
                        .call(
                            "session.set_mode",
                            json!({
                                "session_id": session_id,
                                "mode": args,
                            }),
                        )
                        .await;
                }
                push_system_message(state, text);
            }
            Ok(true)
        }
        "health" => {
            let start = std::time::Instant::now();
            let session_id = state.session_id.clone();
            let health_result = client
                .call("session.get", json!({ "id": session_id }))
                .await;
            let latency_ms = start.elapsed().as_millis();
            let text = match health_result {
                Ok(_) => {
                    format!("health: socket=ok session={session_id} latency={latency_ms}ms")
                }
                Err(e) => format!("health: error — {e}"),
            };
            push_system_message(state, text);
            Ok(true)
        }
        "gov" => {
            let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let artifacts = scan_gov_artifacts(&workspace);
            match args {
                "" | "list" => {
                    push_system_message(state, format_gov_list(&artifacts));
                }
                id_or_show if id_or_show.starts_with("show ") => {
                    let id = id_or_show.trim_start_matches("show ").trim();
                    if let Some(a) = artifacts.iter().find(|a| a.id.eq_ignore_ascii_case(id)) {
                        push_system_message(
                            state,
                            format!(
                                "id: {}\nkind: {}\nstatus: {}\ntitle: {}",
                                a.id, a.kind, a.status, a.title
                            ),
                        );
                    } else {
                        push_system_message(state, format!("gov show: artifact '{id}' not found"));
                    }
                }
                create_args if create_args.starts_with("create ") => {
                    let rest = create_args.trim_start_matches("create ").trim();
                    let msg = gov_create(&workspace, rest);
                    push_system_message(state, msg);
                }
                transition_args if transition_args.starts_with("transition ") => {
                    let rest = transition_args.trim_start_matches("transition ").trim();
                    // Validate status before delegating to gov_transition.
                    #[allow(clippy::items_after_statements)]
                    const VALID_WI_STATUSES: &[&str] =
                        &["planned", "in_progress", "done", "cancelled"];
                    #[allow(clippy::items_after_statements)]
                    const VALID_DOC_STATUSES: &[&str] =
                        &["draft", "accepted", "rejected", "superseded"];
                    let status_arg = rest.split_once(' ').map_or("", |x| x.1).trim();
                    let id_prefix = rest.split('-').next().unwrap_or("");
                    let valid_statuses = if id_prefix.eq_ignore_ascii_case("WI") {
                        VALID_WI_STATUSES
                    } else {
                        VALID_DOC_STATUSES
                    };
                    if !status_arg.is_empty() && !valid_statuses.contains(&status_arg) {
                        push_system_message(
                            state,
                            format!(
                                "Invalid status '{}'. Valid: {}",
                                status_arg,
                                valid_statuses.join(" | ")
                            ),
                        );
                    } else {
                        let msg = gov_transition(&workspace, rest);
                        push_system_message(state, msg);
                    }
                }
                _ => {
                    push_system_message(
                        state,
                        "gov: unknown subcommand — try: /gov list | /gov show <id> | /gov create work-item <title> | /gov transition <id> <status>",
                    );
                }
            }
            Ok(true)
        }
        "help" => {
            push_system_message(state, HELP_TEXT);
            Ok(true)
        }
        "loop" => {
            match args {
                "status" | "" => {
                    match client
                        .call("loop.list_by_status", json!({"statuses": ["planning","slicing","verifying","reviewing","fixed"]}))
                        .await
                    {
                        Ok(Value::Object(ref resp)) => {
                            let loops = resp.get("loops")
                                .and_then(serde_json::Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            if loops.is_empty() {
                                push_system_message(state, "loop: no active loops");
                            } else {
                                let mut lines = vec!["active loops:".to_owned()];
                                for l in &loops {
                                    let id = l["id"].as_str().unwrap_or("?");
                                    let status = l["status"].as_str().unwrap_or("?");
                                    let goal = l["goal"].as_str().unwrap_or("");
                                    lines.push(format!("  {id} [{status}] {goal}"));
                                }
                                push_system_message(state, lines.join("\n"));
                            }
                        }
                        Err(e) => push_system_message(state, format!("loop.status error: {e}")),
                        _ => push_system_message(state, "loop: unexpected response format"),
                    }
                }
                "list" => {
                    match client
                        .call("loop.list", json!({"session_id": state.session_id}))
                        .await
                    {
                        Ok(Value::Object(ref resp)) => {
                            let loops = resp
                                .get("loops")
                                .and_then(serde_json::Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            if loops.is_empty() {
                                push_system_message(state, "loop: no loops for this session");
                            } else {
                                let mut lines = vec!["loops:".to_owned()];
                                for l in &loops {
                                    let id = l["id"].as_str().unwrap_or("?");
                                    let status = l["status"].as_str().unwrap_or("?");
                                    let goal = l["goal"].as_str().unwrap_or("");
                                    lines.push(format!("  {id} [{status}] {goal}"));
                                }
                                push_system_message(state, lines.join("\n"));
                            }
                        }
                        Err(e) => push_system_message(state, format!("loop.list error: {e}")),
                        _ => push_system_message(state, "loop: unexpected response format"),
                    }
                }
                goal if goal.starts_with("create ") => {
                    let goal_text = goal.trim_start_matches("create ").trim();
                    if goal_text.is_empty() {
                        push_system_message(
                            state,
                            "loop create: provide a goal — /loop create <goal text>",
                        );
                    } else {
                        match client
                            .call(
                                "loop.create",
                                json!({
                                    "session_id": state.session_id,
                                    "goal": goal_text,
                                }),
                            )
                            .await
                        {
                            Ok(ref resp) => {
                                let id = resp["id"].as_str().unwrap_or("?");
                                push_system_message(state, format!("loop created: {id}"));
                            }
                            Err(e) => {
                                push_system_message(state, format!("loop.create error: {e}"));
                            }
                        }
                    }
                }
                "cancel" => {
                    // Cancel the most recent active loop for this session.
                    match client
                        .call("loop.list_by_status", json!({"statuses": ["planning","slicing","verifying","reviewing","fixed"]}))
                        .await
                    {
                        Ok(Value::Object(ref resp)) => {
                            let loops = resp.get("loops")
                                .and_then(serde_json::Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            if let Some(first) = loops.first() {
                                let id = first["id"].as_str().unwrap_or("").to_owned();
                                match client.call("loop.cancel", json!({"id": id})).await {
                                    Ok(_) => {
                                        push_system_message(state, format!("loop cancelled: {id}"));
                                    }
                                    Err(e) => {
                                        push_system_message(
                                            state,
                                            format!("loop.cancel error: {e}"),
                                        );
                                    }
                                }
                            } else {
                                push_system_message(state, "loop cancel: no active loop to cancel");
                            }
                        }
                        Err(e) => push_system_message(state, format!("loop.list error: {e}")),
                        _ => push_system_message(state, "loop: unexpected response"),
                    }
                }
                _ => {
                    push_system_message(
                        state,
                        "loop: unknown subcommand — try: /loop status | list | create <goal> | cancel",
                    );
                }
            }
            Ok(true)
        }
        "quit" | "exit" => {
            state.quit = true;
            Ok(true)
        }
        "clear" => {
            state.display_start_idx = state.messages.len();
            state.main_panel.clear_display();
            Ok(true)
        }
        "spec" => {
            let Some(ref bin) = state.openspec_bin else {
                push_system_message(
                    state,
                    "openspec not found — install it and restart smedja-tui",
                );
                return Ok(true);
            };
            let bin = bin.clone();
            let (sub, rest) = args.split_once(' ').unwrap_or((args, ""));
            let text = match sub {
                "" | "list" => match run_openspec(&bin, &["list", "--json"]).await {
                    Ok(json) => format_openspec_list(&json),
                    Err(e) => e,
                },
                "status" => {
                    let extra: Vec<&str> = if rest.is_empty() {
                        vec!["status", "--json"]
                    } else {
                        vec!["status", "--change", rest, "--json"]
                    };
                    match run_openspec(&bin, &extra).await {
                        Ok(json) => format_openspec_status(&json),
                        Err(e) => e,
                    }
                }
                "archive" if !rest.is_empty() => {
                    match run_openspec(&bin, &["archive", rest, "--yes"]).await {
                        Ok(_) => format!("archived: {rest}"),
                        Err(e) => e,
                    }
                }
                _ => "usage: /spec [list|status [name]|archive <name>]".to_owned(),
            };
            push_system_message(state, text);
            Ok(true)
        }
        "model" => {
            let session_id = state.session_id.clone();
            let is_local = state.runner == "local";
            if args.is_empty() || args == "reset" {
                // For the local runner, list the GPU-annotated inventory via
                // local.models; for hosted runners keep the runner.list view.
                let text = if is_local {
                    match client.call("local.models", json!({})).await {
                        Ok(v) => format_local_model_list(&v),
                        Err(e) => format!("local.models error: {e}"),
                    }
                } else {
                    match client.call("runner.list", json!({})).await {
                        Ok(v) => format_model_list(&v),
                        Err(e) => format!("runner.list error: {e}"),
                    }
                };
                push_system_message(state, text);
            } else if is_local {
                // Local runner: a model name hot-swaps the active local model via
                // local.swap (not the relabel-only session.set_model).
                let model = args.to_owned();
                let result = client.call("local.swap", json!({ "model": model })).await;
                match result {
                    Ok(v) => {
                        let latency = v["swap_latency_ms"].as_u64().unwrap_or(0);
                        state.model = Some(model.clone());
                        push_system_message(
                            state,
                            format!("local model swapped to {model} ({latency} ms)"),
                        );
                    }
                    Err(e) => push_system_message(state, format!("local.swap error: {e}")),
                }
            } else {
                let model = args.to_owned();
                let result = client
                    .call(
                        "session.set_model",
                        json!({ "session_id": session_id, "model": model }),
                    )
                    .await;
                match result {
                    Ok(_) => {
                        state.model = Some(model.clone());
                        push_system_message(state, format!("model set to {model}"));
                    }
                    Err(e) => push_system_message(state, format!("session.set_model error: {e}")),
                }
            }
            Ok(true)
        }
        "metrics" => {
            let session_id = state.session_id.clone();
            let usage_result = client
                .call("session.token_usage", json!({ "session_id": session_id }))
                .await;
            let cost_result = client
                .call("session.cost", json!({ "session_id": &state.session_id }))
                .await;
            let text = format_metrics(&usage_result, &cost_result, &state.session_id);
            push_system_message(state, text);
            Ok(true)
        }
        "approve" => {
            if args.is_empty() {
                let session_id = state.session_id.clone();
                let result = client
                    .call("cowork.pending", json!({ "session_id": session_id }))
                    .await;
                let text = match result {
                    Ok(v) => format_approvals_list(&v),
                    Err(e) => format!("cowork.pending error: {e}"),
                };
                push_system_message(state, text);
                return Ok(true);
            }
            let id = args.to_owned();
            let session_id = state.session_id.clone();
            let result = client
                .call(
                    "cowork.approve",
                    json!({ "session_id": session_id, "id": id }),
                )
                .await;
            match result {
                Ok(v) => {
                    let resolved = v
                        .get("resolved")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let text = if resolved {
                        format!("approved: {id}")
                    } else {
                        format!("item not found: {id}")
                    };
                    push_system_message(state, text);
                }
                Err(e) => push_system_message(state, format!("cowork.approve error: {e}")),
            }
            Ok(true)
        }
        "quality" => {
            trigger_quality_review(state, client).await;
            Ok(true)
        }
        "value" => {
            show_value_report(state);
            Ok(true)
        }
        "review" => {
            let mut params = parse_review_scope(args);

            // Empty working-tree diff (everything committed) no longer hard-refuses:
            // fall back to a repository path scope instead.
            let explicit_diff = params
                .get("diff")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let is_diff_scope = !explicit_diff
                && params.get("path").is_none()
                && params.get("branch").is_none()
                && params.get("pr").is_none();
            if is_diff_scope {
                let empty_diff = std::process::Command::new("git")
                    .args(["diff", "HEAD"])
                    .output()
                    .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim().is_empty());
                if empty_diff {
                    params = json!({ "path": "." });
                    push_system_message(
                        state,
                        "working tree clean; auditing the repository path scope",
                    );
                }
            }

            // The audit runs under the read-only Review role; set review mode.
            let session_id = state.session_id.clone();
            let _ = client
                .call(
                    "session.set_mode",
                    json!({ "session_id": session_id, "mode": "review" }),
                )
                .await;

            match client.call("audit.run", params).await {
                Ok(resp) => {
                    let counts = resp.get("counts").cloned().unwrap_or_else(|| json!({}));
                    let report_path = resp.get("report_path").and_then(serde_json::Value::as_str);
                    push_system_message(state, render_findings_summary(&counts, report_path));
                }
                Err(e) => push_system_message(state, format!("audit.run error: {e}")),
            }
            Ok(true)
        }
        "drawio" => {
            if args.is_empty() {
                push_system_message(state, "usage: /drawio <topic>");
                return Ok(true);
            }
            let slug = slugify(args);
            state.pending_output_type = Some(OutputType::DrawIo { slug });
            let message = format!(
                "Generate a draw.io diagram (mxGraph XML format) for: {args}\n\n\
                 Output ONLY the complete XML, enclosed in a ```xml code block. \
                 Use valid mxGraph XML that draw.io can open directly."
            );
            submit(&message, state, client).await?;
            Ok(true)
        }
        "pptx" => {
            if args.is_empty() {
                push_system_message(state, "usage: /pptx <topic>");
                return Ok(true);
            }
            let slug = slugify(args);
            state.pending_output_type = Some(OutputType::Pptx { slug });
            let message = format!(
                "Generate a python-pptx script to create a presentation about: {args}\n\n\
                 Output ONLY the complete Python script, enclosed in a ```python code block. \
                 The script must save the file as '{args_slug}.pptx' in the current directory.",
                args_slug = slugify(args)
            );
            submit(&message, state, client).await?;
            Ok(true)
        }
        "briefing" => {
            let session_id = state.session_id.clone();
            let result = client
                .call("session.compact", json!({ "session_id": session_id }))
                .await;
            match result {
                Ok(v) => {
                    let summary = v
                        .get("summary")
                        .and_then(|s| s.as_str())
                        .unwrap_or("(no summary)")
                        .to_owned();
                    push_system_message(state, format!("briefing:\n{summary}"));
                }
                Err(e) => push_system_message(state, format!("session.compact error: {e}")),
            }
            Ok(true)
        }
        "lsp" => {
            let snap = &state.lsp_snapshot;
            if snap.servers.is_empty() {
                push_system_message(state, "lsp: no language servers running (install rust-analyzer, gopls, pyright, or typescript-language-server)");
            } else {
                let mut lines = vec!["lsp servers:".to_owned()];
                for srv in &snap.servers {
                    let state_str = match &srv.state {
                        smedja_lsp::ServerState::Starting => "starting".to_owned(),
                        smedja_lsp::ServerState::Ready => "ready".to_owned(),
                        smedja_lsp::ServerState::Degraded(r) => format!("degraded ({r})"),
                    };
                    lines.push(format!("  {} — {}", srv.name, state_str));
                }
                let errs = snap.error_count();
                let warns = snap.warning_count();
                lines.push(format!("diagnostics: {errs} error(s), {warns} warning(s)"));
                for diag in snap.diagnostics.iter().take(10) {
                    let label = diag.severity.label();
                    let file = diag.file.display();
                    let code = diag.code.as_deref().unwrap_or("");
                    lines.push(format!(
                        "  {label} {file}:{} {code} — {}",
                        diag.line,
                        &diag.message[..diag.message.len().min(60)]
                    ));
                }
                push_system_message(state, lines.join("\n"));
            }
            Ok(true)
        }
        "test" => {
            let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let detected = detect_project_types(&workspace);
            let has_cargo = detected.contains(&"Cargo.toml");
            let has_npm = detected.contains(&"package.json");
            let has_go = detected.contains(&"go.mod");
            let has_py = detected.contains(&"pyproject.toml");
            if detected.len() > 1 {
                push_system_message(
                    state,
                    format!(
                        "note: multiple manifests ({}) — using {} (pass /test cargo|npm|go|py to override)",
                        detected.join(", "),
                        detected[0]
                    ),
                );
            }
            let (cmd, cmd_args): (&str, &[&str]) = match args {
                "cargo" => ("cargo", &["test", "--", "--test-output=immediate"]),
                "npm" => ("npm", &["test"]),
                "go" => ("go", &["test", "./..."]),
                "py" | "pytest" => ("python", &["-m", "pytest"]),
                _ => {
                    if has_cargo {
                        ("cargo", &["test", "--", "--test-output=immediate"])
                    } else if has_npm {
                        ("npm", &["test"])
                    } else if has_go {
                        ("go", &["test", "./..."])
                    } else if has_py {
                        ("python", &["-m", "pytest"])
                    } else {
                        ("cargo", &["test", "--", "--test-output=immediate"])
                    }
                }
            };
            push_system_message(
                state,
                format!("running {cmd} {}\u{2026}", cmd_args.join(" ")),
            );
            let text = match tokio::process::Command::new(cmd)
                .args(cmd_args)
                .current_dir(&workspace)
                .output()
                .await
            {
                Err(e) => format!("{cmd} failed to start: {e}"),
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let combined = format!("{stdout}{stderr}");
                    let passed = combined.matches("test result: ok").count()
                        + combined.matches("PASSED").count()
                        + combined.matches(" passed").count();
                    let failed =
                        combined.matches("FAILED").count() + combined.matches(" failed").count();
                    let mut summary = format!("test: {passed} passed, {failed} failed");
                    // Show last 20 lines of output for context.
                    let tail: Vec<&str> = combined.lines().rev().take(20).collect();
                    let tail_text: Vec<&str> = tail.into_iter().rev().collect();
                    if !tail_text.is_empty() {
                        summary.push('\n');
                        summary.push_str(&tail_text.join("\n"));
                    }
                    summary
                }
            };
            push_system_message(state, text);
            Ok(true)
        }
        "quota" => {
            let used = state.obs_snapshot.daily_tokens_used;
            let limit = state.obs_snapshot.daily_tokens_limit;
            let text = match (used, limit) {
                (Some(u), Some(l)) if l > 0 => {
                    #[allow(clippy::cast_precision_loss)]
                    let pct = (u as f64 / l as f64 * 100.0).min(100.0);
                    format!(
                        "quota: {}/{} tokens used ({:.1}%)",
                        format_token_count(u),
                        format_token_count(l),
                        pct
                    )
                }
                (Some(u), _) => format!(
                    "quota: {} tokens used (no daily limit configured — set SMEDJA_DAILY_TOKEN_LIMIT)",
                    format_token_count(u)
                ),
                (None, _) => "quota: no usage data yet — opens after first turn completes".into(),
            };
            push_system_message(state, text);
            Ok(true)
        }
        "login" => {
            let guidance = if args.is_empty() {
                // Scan for installed CLIs so the user sees what's found vs missing.
                let claude_found = std::process::Command::new("which")
                    .arg("claude")
                    .output()
                    .is_ok_and(|o| o.status.success());
                let codex_found = std::process::Command::new("which")
                    .arg("codex")
                    .output()
                    .is_ok_and(|o| o.status.success());
                let llmctl_found = std::process::Command::new("which")
                    .arg("llmctl")
                    .output()
                    .is_ok_and(|o| o.status.success());

                let mut lines = vec![
                    "available runners:".to_owned(),
                    format!(
                        "  claude   [{}]  — Claude.ai subscription (OAuth, no API key needed)",
                        if claude_found {
                            "installed"
                        } else {
                            "not found"
                        }
                    ),
                    format!(
                        "  codex    [{}]  — OpenAI Codex CLI",
                        if codex_found {
                            "installed"
                        } else {
                            "not found"
                        }
                    ),
                    format!(
                        "  local    [{}]  — local model via rs-llmctl",
                        if llmctl_found {
                            "installed"
                        } else {
                            "not found"
                        }
                    ),
                    "  copilot              — GitHub Copilot".to_owned(),
                    "  minimax              — Minimax (set MINIMAX_API_KEY)".to_owned(),
                    "  berget               — Berget AI (set BERGET_API_KEY)".to_owned(),
                ];
                if !claude_found {
                    lines.push(String::new());
                    lines.push("to install claude CLI: https://claude.ai/download".to_owned());
                    lines.push("then run: claude login".to_owned());
                }
                lines.join("\n")
            } else {
                match args {
                    "claude" => {
                        let found = std::process::Command::new("which")
                            .arg("claude")
                            .output()
                            .is_ok_and(|o| o.status.success());
                        if found {
                            "claude CLI is installed — uses your Claude.ai subscription (OAuth).\n\
                             if not authenticated yet, run: claude login"
                                .to_owned()
                        } else {
                            "claude CLI not found.\n\
                             install: https://claude.ai/download\n\
                             then run: claude login\n\
                             no API key required — uses your Claude.ai subscription."
                                .to_owned()
                        }
                    }
                    "codex" => {
                        let found = std::process::Command::new("which")
                            .arg("codex")
                            .output()
                            .is_ok_and(|o| o.status.success());
                        if found {
                            "codex CLI is installed.\n\
                             set OPENAI_API_KEY in your shell profile to authenticate."
                                .to_owned()
                        } else {
                            "codex CLI not found.\n\
                             install: npm install -g @openai/codex\n\
                             then set OPENAI_API_KEY in your shell profile."
                                .to_owned()
                        }
                    }
                    "local" => "local runner uses rs-llmctl and a llama-swap proxy.\n\
                                install rs-llmctl, then restart smdjad."
                        .to_owned(),
                    "copilot" => "copilot runner uses GitHub Copilot.\n\
                                  authenticate via the copilot CLI or VS Code extension."
                        .to_owned(),
                    "minimax" => {
                        state.secret_var = Some("MINIMAX_API_KEY".to_owned());
                        "paste your Minimax API key then Enter — input is hidden · Esc to cancel"
                            .to_owned()
                    }
                    "berget" => {
                        state.secret_var = Some("BERGET_API_KEY".to_owned());
                        "paste your Berget API key then Enter — input is hidden · Esc to cancel"
                            .to_owned()
                    }
                    other => format!(
                        "unknown runner: {other}\nvalid: claude, codex, local, copilot, minimax, berget"
                    ),
                }
            };
            push_system_message(state, guidance);
            Ok(true)
        }
        "switch" => {
            if args.is_empty() {
                let result = client.call("runner.list", json!({})).await;
                match result {
                    Ok(v) => {
                        let runners: Vec<String> = v
                            .get("runners")
                            .and_then(|r| r.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|r| {
                                        r.get("runner").and_then(|n| n.as_str()).map(str::to_owned)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if runners.is_empty() {
                            push_system_message(state, "no runners available from runner.list");
                        } else {
                            state.slash_completions = runners;
                            state.slash_cursor = 0;
                            state.slash_popup_visible = true;
                            state.runner_picker_mode = true;
                            state.input.clear();
                            state.input_cursor = 0;
                        }
                    }
                    Err(e) => {
                        push_system_message(
                            state,
                            format!(
                                "usage: /switch [runner]  — omit for interactive picker\n\
                                 runners: claude, codex, local, copilot, minimax, berget\n\
                                 (runner.list error: {e})"
                            ),
                        );
                    }
                }
                return Ok(true);
            }
            let session_id = state.session_id.clone();
            let result = client
                .call(
                    "session.set_runner",
                    json!({ "session_id": session_id, "runner": args }),
                )
                .await;
            match result {
                Ok(v) => {
                    let canonical = v
                        .get("runner")
                        .and_then(|r| r.as_str())
                        .unwrap_or(args)
                        .to_owned();
                    state.runner.clone_from(&canonical);
                    // Update the displayed model to the new runner's default.
                    if let Ok(list) = client.call("runner.list", json!({})).await {
                        if let Some(runners) = list.get("runners").and_then(|r| r.as_array()) {
                            if let Some(m) = runners
                                .iter()
                                .find(|r| {
                                    r.get("runner").and_then(|n| n.as_str()) == Some(&canonical)
                                })
                                .and_then(|r| r.get("model").and_then(|m| m.as_str()))
                            {
                                state.model = Some(m.to_owned());
                            }
                        }
                    }
                    push_system_message(state, format!("runner switched to {canonical}"));
                    // Show the existing session memory the new runner picks up.
                    if let Ok(hist) = client
                        .call("session.history", json!({ "session_id": session_id }))
                        .await
                    {
                        push_system_message(state, format_memory(&hist, None, &session_id));
                    }
                }
                Err(e) => push_system_message(state, format!("session.set_runner error: {e}")),
            }
            Ok(true)
        }
        "takeover" => {
            if args.is_empty() {
                push_system_message(
                    state,
                    "usage: /takeover <runner>  — fork this session onto a new runner",
                );
                return Ok(true);
            }
            let session_id = state.session_id.clone();
            let result = client
                .call(
                    "session.takeover",
                    json!({ "session_id": session_id, "runner": args }),
                )
                .await;
            match result {
                Ok(v) => {
                    let new_session_id = v
                        .get("new_session_id")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let runner = v
                        .get("runner")
                        .and_then(|r| r.as_str())
                        .unwrap_or(args)
                        .to_owned();
                    state.session_id.clone_from(&new_session_id);
                    state.runner.clone_from(&runner);
                    push_system_message(
                        state,
                        format!(
                            "handed off to {runner} — new session: {}",
                            &new_session_id[..8.min(new_session_id.len())]
                        ),
                    );
                    // Surface the memory the new runner inherits so the hand-off
                    // is transparent (e.g. codex seeing claude's prior work).
                    if let Ok(hist) = client
                        .call("session.history", json!({ "session_id": new_session_id }))
                        .await
                    {
                        push_system_message(state, format_memory(&hist, None, &new_session_id));
                    }
                }
                Err(e) => push_system_message(state, format!("session.takeover error: {e}")),
            }
            Ok(true)
        }
        "resume" => {
            if resume_blocked_by_pending_turn(state) {
                return Ok(true);
            }
            match parse_resume_args(args) {
                None => {
                    // No id: open the interactive picker from session.list.
                    match client.call("session.list", json!({})).await {
                        Ok(list) => {
                            let rows = format_resume_rows(&list);
                            let ids: Vec<String> = list
                                .as_array()
                                .map(|items| {
                                    items
                                        .iter()
                                        .map(|s| {
                                            s.get("id")
                                                .and_then(serde_json::Value::as_str)
                                                .unwrap_or("")
                                                .to_owned()
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            if rows.is_empty() {
                                push_system_message(state, "no sessions available to resume");
                            } else {
                                state.slash_completions = rows;
                                state.session_picker_ids = ids;
                                state.slash_cursor = 0;
                                state.slash_popup_visible = true;
                                state.session_picker_mode = true;
                                state.input.clear();
                                state.input_cursor = 0;
                            }
                        }
                        Err(e) => push_system_message(state, format!("session.list error: {e}")),
                    }
                }
                Some((id, turn)) => {
                    // Direct resume: swap session, clear the live display, replay.
                    state.session_id = id;
                    state.display_start_idx = state.messages.len();
                    state.main_panel.clear_display();
                    resume_into_view(state, client, resume_plan(turn)).await;
                }
            }
            Ok(true)
        }
        "version" => {
            push_system_message(state, format!("smedja v{VERSION}"));
            match fetch_latest_version().await {
                Some(ref tag) if is_newer(tag, VERSION) => {
                    push_system_message(
                        state,
                        format!("new version {tag} available — run /upgrade to install"),
                    );
                }
                Some(_) => {
                    push_system_message(state, "you are up to date");
                }
                None => {
                    push_system_message(state, "could not reach GitHub to check for updates");
                }
            }
            Ok(true)
        }
        "upgrade" => {
            if state.upgrade_rx.is_some() {
                push_system_message(state, "upgrade already in progress");
                return Ok(true);
            }
            let current = VERSION.to_owned();
            push_system_message(
                state,
                format!("checking for updates (current: v{current})\u{2026}"),
            );
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            state.upgrade_rx = Some(rx);
            tokio::spawn(async move {
                let msg = match fetch_latest_version().await {
                    None => "upgrade failed: could not reach GitHub releases".into(),
                    Some(latest) if !is_newer(&latest, &current) => {
                        format!("already at {latest}, nothing to upgrade")
                    }
                    Some(latest) => run_upgrade(&latest).await,
                };
                let _ = tx.send(msg);
            });
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Fires a Tier-2 LLM quality review via the `quality.review` RPC.
///
/// Shows `[quality review in progress…]` in the quality panel while awaiting.
/// On completion or error the panel is updated via the normal `QualitySnapshot`
/// bellows event.
pub(crate) async fn trigger_quality_review(state: &mut AppState, client: &mut Client) {
    if state.quality_review_in_progress {
        push_system_message(state, "quality review already in progress");
        return;
    }
    state.quality_review_in_progress = true;
    state.panels.quality = true;
    push_system_message(state, "[quality review in progress\u{2026}]");

    let session_id = state.session_id.clone();
    match client
        .call("quality.review", json!({ "session_id": session_id }))
        .await
    {
        Ok(_) => {
            // The review runs async in smdjad; result arrives as QualitySnapshot event.
        }
        Err(e) => {
            push_system_message(state, format!("quality.review error: {e}"));
            state.quality_review_in_progress = false;
        }
    }
}

/// Prints a Markdown ROI report for the active openspec change to the main panel.
pub(crate) fn show_value_report(state: &mut AppState) {
    let snap = &state.value_snapshot;
    let change = snap.change_name.as_deref().unwrap_or("(no active change)");
    #[allow(clippy::cast_precision_loss)]
    let cost_dollars = snap.cost_usd_micros as f64 / 1_000_000.0;
    let report = format!(
        "## value report\n\n| field | value |\n|---|---|\n| change | {change} |\n| tokens | {} |\n| cost | ${cost_dollars:.4} |\n| quality avg | {}/100 |\n| roi estimate | {} (estimate) |",
        snap.token_cost, snap.quality_avg, snap.estimated_value
    );
    push_system_message(state, report);
    state.panels.value = true;
}

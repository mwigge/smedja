use super::*;
use serde_json::json;

pub(crate) async fn dispatch_audit(action: AuditCmd, sock: &std::path::Path) -> Result<()> {
    match action {
        AuditCmd::Run {
            path,
            branch,
            pr,
            diff,
            report,
            format,
        } => {
            let workspace = std::env::current_dir()
                .map(|p| p.display().to_string())
                .ok();
            let params = build_audit_params(
                path.as_deref(),
                branch.as_deref(),
                pr.as_deref(),
                diff,
                report.as_deref(),
                &format,
                workspace.as_deref(),
            );
            let mut client = Client::connect(sock)
                .await
                .with_context(|| format!("smdjad not running ({})", sock.display()))?;
            // `audit.run` blocks for the full read-only LLM exploration loop
            // (minutes), so it needs the long timeout — the 30s default would
            // kill it mid-loop (JSON-RPC -32001).
            let resp = client
                .call_with_timeout(
                    "audit.run",
                    params,
                    smedja_rpc::client::LONG_REQUEST_TIMEOUT,
                )
                .await
                .context("audit.run failed")?;
            if format == "json" {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else if let Some(report_path) = resp.get("report_path").and_then(|v| v.as_str()) {
                println!("Report written to {report_path}");
            } else if let Some(body) = resp.get("report").and_then(|v| v.as_str()) {
                println!("{body}");
            } else {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        }
        AuditCmd::Query {
            session,
            since,
            action,
        } => {
            let mut client = Client::connect(sock)
                .await
                .with_context(|| format!("smdjad not running ({})", sock.display()))?;
            let mut params = serde_json::json!({});
            if let Some(sid) = session {
                params["session_id"] = serde_json::Value::String(sid);
            }
            if let Some(s) = since {
                params["since"] = serde_json::Value::String(s);
            }
            if let Some(a) = action {
                params["action_type"] = serde_json::Value::String(a);
            }
            let resp = client
                .call("audit.list", params)
                .await
                .context("audit.list failed")?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
        AuditCmd::PromptDiff { change } => {
            let db_path = default_ingot_path();
            let ingot = Ingot::open(&db_path)
                .with_context(|| format!("failed to open ingot at {}", db_path.display()))?;
            let hashes = ingot
                .list_prompt_hashes(&change)
                .context("list_prompt_hashes failed")?;
            if hashes.is_empty() {
                println!("No prompt hashes recorded for change '{change}'");
            } else {
                println!("{:<20} {:<64} ts", "role", "hash");
                println!("{}", "-".repeat(90));
                for h in &hashes {
                    println!("{:<20} {:<64} {}", h.role, h.hash, h.ts);
                }
            }
        }
        AuditCmd::Who { session } => {
            let mut client = Client::connect(sock)
                .await
                .with_context(|| format!("smdjad not running ({})", sock.display()))?;
            let resp = client
                .call("audit.list", json!({ "session_id": session }))
                .await
                .context("audit.list failed")?;
            // Group by role_id -> action_type -> count.
            let mut counts: std::collections::HashMap<
                String,
                std::collections::HashMap<String, u64>,
            > = std::collections::HashMap::new();
            if let Some(events) = resp["events"].as_array() {
                for ev in events {
                    let role_id = ev["role_id"].as_str().unwrap_or("(no role)").to_owned();
                    let action = ev["action_type"].as_str().unwrap_or("?").to_owned();
                    *counts
                        .entry(role_id)
                        .or_default()
                        .entry(action)
                        .or_insert(0) += 1;
                }
            }
            println!("{:<36} {:<20} count", "role_id", "action_type");
            println!("{}", "-".repeat(64));
            let mut rows: Vec<(String, String, u64)> = counts
                .into_iter()
                .flat_map(|(role, actions)| {
                    actions
                        .into_iter()
                        .map(move |(action, count)| (role.clone(), action, count))
                })
                .collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            for (role_id, action, count) in &rows {
                println!("{role_id:<36} {action:<20} {count}");
            }
        }
        AuditCmd::Export {
            change,
            format,
            include_prompts,
        } => {
            let db_path = default_ingot_path();
            let ingot = Ingot::open(&db_path)
                .with_context(|| format!("failed to open ingot at {}", db_path.display()))?;
            let events = ingot
                .list_all_audit_events()
                .context("list_all_audit_events failed")?;
            // Build optional prompt hash lookup: role -> hash.
            let prompt_hashes: std::collections::HashMap<String, String> = if include_prompts {
                ingot
                    .list_prompt_hashes(&change)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| (r.role, r.hash))
                    .collect()
            } else {
                std::collections::HashMap::new()
            };

            match format.as_str() {
                "csv" => {
                    if include_prompts {
                        println!(
                            "role_id,session_id,tool_name,args_hash,result_tokens,traceparent,ts,prompt_hash"
                        );
                    } else {
                        println!(
                            "role_id,session_id,tool_name,args_hash,result_tokens,traceparent,ts"
                        );
                    }
                    for ev in &events {
                        let role_id = ev.role_id.as_deref().unwrap_or("");
                        let tool = ev.tool_name.as_deref().unwrap_or("");
                        let tp = ev.traceparent.as_deref().unwrap_or("");
                        if include_prompts {
                            let ph = prompt_hashes.get(role_id).map_or("", String::as_str);
                            println!(
                                "{role_id},{},{tool},,{},{tp},{},{}",
                                ev.session_id,
                                ev.output_tok,
                                ev.ts.as_micros(),
                                ph
                            );
                        } else {
                            println!(
                                "{role_id},{},{tool},,{},{tp},{}",
                                ev.session_id,
                                ev.output_tok,
                                ev.ts.as_micros()
                            );
                        }
                    }
                }
                _ => {
                    // Default: jsonl
                    for ev in &events {
                        let role_id = ev.role_id.as_deref().unwrap_or("");
                        let mut obj = serde_json::json!({
                            "role_id": role_id,
                            "session_id": ev.session_id,
                            "tool_name": ev.tool_name,
                            "args_hash": "",
                            "result_tokens": ev.output_tok,
                            "traceparent": ev.traceparent,
                            "ts": ev.ts,
                        });
                        if include_prompts {
                            let ph = prompt_hashes.get(role_id).map_or("", String::as_str);
                            obj["prompt_hash"] = serde_json::Value::String(ph.to_owned());
                        }
                        println!("{}", serde_json::to_string(&obj)?);
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn build_audit_params(
    path: Option<&str>,
    branch: Option<&str>,
    pr: Option<&str>,
    diff: bool,
    report: Option<&str>,
    format: &str,
    workspace: Option<&str>,
) -> serde_json::Value {
    let mut params = json!({ "format": format });
    if let Some(pr) = pr {
        params["pr"] = json!(pr);
    } else if let Some(base) = branch {
        params["branch"] = json!(base);
    } else if diff {
        params["diff"] = json!(true);
    } else if let Some(path) = path {
        params["path"] = json!(path);
    }
    if let Some(report) = report {
        params["report"] = json!(report);
    }
    if let Some(ws) = workspace {
        params["workspace"] = json!(ws);
    }
    params
}

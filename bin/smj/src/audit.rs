//! `smj audit` — repo/PR/branch audit runs and audit-log queries.

use std::path::Path;

use anyhow::{Context as _, Result};
use clap::Subcommand;
use serde_json::json;
use smedja_ingot::Ingot;

use crate::util::{connect, default_ingot_path};

#[derive(Subcommand)]
pub(crate) enum AuditCmd {
    /// Run a read-only repo/PR/branch audit and write a markdown report
    Run {
        /// Path or whole-repo scope (omit for the working-tree diff)
        path: Option<String>,
        /// Audit a branch range against this base (`<base>...HEAD`)
        #[arg(long)]
        branch: Option<String>,
        /// Audit a pull-request reference resolved to a branch range
        #[arg(long)]
        pr: Option<String>,
        /// Force the working-tree diff scope (`git diff HEAD`)
        #[arg(long)]
        diff: bool,
        /// Write the markdown report to this path instead of stdout
        #[arg(long)]
        report: Option<String>,
        /// Output format: `md` (default) or `json`
        #[arg(long, default_value = "md")]
        format: String,
    },
    /// Query audit log events
    Query {
        /// Filter by session ID
        #[arg(long)]
        session: Option<String>,
        /// Filter by relative duration (e.g. "1h", "24h")
        #[arg(long)]
        since: Option<String>,
        /// Filter by action type
        #[arg(long)]
        action: Option<String>,
    },
    /// Show prompt hash records for a change
    PromptDiff {
        /// Name of the `OpenSpec` change to inspect
        #[arg(long)]
        change: String,
    },
    /// Show which role produced which action type for a session
    Who {
        /// Session ID to inspect
        #[arg(long)]
        session: String,
    },
    /// Export audit events for a change to stdout
    Export {
        /// Name of the `OpenSpec` change to export
        #[arg(long)]
        change: String,
        /// Output format: `jsonl` or `csv`
        #[arg(long, default_value = "jsonl")]
        format: String,
        /// Include prompt hashes as an additional column
        #[arg(long)]
        include_prompts: bool,
    },
}

/// Dispatches a `smj audit` subcommand.
pub(crate) async fn run(sock: &Path, action: AuditCmd) -> Result<()> {
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
            let mut client = connect(sock).await?;
            let resp = client
                .call("audit.run", params)
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
            let mut client = connect(sock).await?;
            let mut params = json!({});
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
            let mut client = connect(sock).await?;
            let resp = client
                .call("audit.list", json!({ "session_id": session }))
                .await
                .context("audit.list failed")?;
            // Group by role_id → action_type → count.
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
            // Build optional prompt hash lookup: role → hash.
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
                        let mut obj = json!({
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

/// Builds the `audit.run` RPC params from the parsed `smj audit run` flags.
///
/// Scope precedence mirrors the daemon's `resolve_scope`: `--pr` → `--branch` →
/// `--diff`/no-path → a positional `<path>`.
fn build_audit_params(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Cmd};
    use clap::Parser as _;

    #[test]
    fn audit_run_parses_path_branch_pr_diff_report_format() {
        let cli = Cli::try_parse_from([
            "smj",
            "audit",
            "run",
            "src/lib.rs",
            "--report",
            "out.md",
            "--format",
            "json",
        ])
        .expect("audit run with a path must parse");
        let Cmd::Audit {
            action:
                AuditCmd::Run {
                    path,
                    branch,
                    pr,
                    diff,
                    report,
                    format,
                },
        } = cli.command
        else {
            panic!("expected an audit run command");
        };
        assert_eq!(path.as_deref(), Some("src/lib.rs"));
        assert_eq!(branch, None);
        assert_eq!(pr, None);
        assert!(!diff);
        assert_eq!(report.as_deref(), Some("out.md"));
        assert_eq!(format, "json");
    }

    #[test]
    fn audit_run_parses_branch_and_pr_flags() {
        let branch = Cli::try_parse_from(["smj", "audit", "run", "--branch", "main"]).unwrap();
        assert!(matches!(
            branch.command,
            Cmd::Audit {
                action: AuditCmd::Run { branch: Some(b), .. }
            } if b == "main"
        ));
        let pr = Cli::try_parse_from(["smj", "audit", "run", "--pr", "42"]).unwrap();
        assert!(matches!(
            pr.command,
            Cmd::Audit {
                action: AuditCmd::Run { pr: Some(p), .. }
            } if p == "42"
        ));
    }

    #[test]
    fn build_audit_params_respects_scope_precedence() {
        // pr wins over branch/path
        let params = build_audit_params(
            Some("src"),
            Some("main"),
            Some("7"),
            false,
            None,
            "md",
            None,
        );
        assert_eq!(params["pr"], "7");
        assert!(params.get("branch").is_none());
        assert!(params.get("path").is_none());

        // path scope with report + workspace
        let params = build_audit_params(
            Some("src"),
            None,
            None,
            false,
            Some("r.md"),
            "json",
            Some("/ws"),
        );
        assert_eq!(params["path"], "src");
        assert_eq!(params["report"], "r.md");
        assert_eq!(params["format"], "json");
        assert_eq!(params["workspace"], "/ws");

        // diff flag overrides a path
        let params = build_audit_params(Some("src"), None, None, true, None, "md", None);
        assert_eq!(params["diff"], true);
        assert!(params.get("path").is_none());
    }
}

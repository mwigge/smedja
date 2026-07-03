//! `smj session` — session lifecycle, export, compaction, and headless prompts.

use std::path::Path;

use anyhow::{Context as _, Result};
use clap::Subcommand;
use serde_json::json;
use smedja_rpc::client::Client;

use crate::util::connect_or_exit;

#[derive(Subcommand)]
pub(crate) enum SessionCmd {
    /// Start a new session
    Start {
        /// Enable cowork mode (human approval for each tool call)
        #[arg(long)]
        cowork: bool,
        /// Create a task linked to this session
        #[arg(long)]
        task: Option<String>,
        /// Maximum number of agentic turns (must be ≥ 1)
        #[arg(long, value_parser = parse_max_turns)]
        max_turns: Option<usize>,
        /// Maximum budget in USD for this session
        #[arg(long)]
        max_budget_usd: Option<f64>,
        /// Tool permission level: default | accept-edits | bypass-permissions | plan
        #[arg(long, value_parser = parse_permission_mode)]
        permission_mode: Option<PermissionMode>,
    },
    List,
    Show {
        id: String,
    },
    Fork {
        id: String,
        #[arg(long)]
        turn: Option<u32>,
    },
    Rollback {
        id: String,
        turn: u32,
    },
    /// List stored blocks for a session
    Blocks {
        id: String,
    },
    /// List checkpoints for a session
    Checkpoint {
        id: String,
    },
    /// Export session cost lineage or messages
    Export {
        /// Session ID to export
        id: String,
        /// Output format: json (default) or md
        #[arg(long, default_value = "json")]
        format: String,
    },
    /// Compact session conversation history
    Compact {
        /// Session ID to compact
        id: String,
    },
    /// Show per-turn token usage for a session
    Tokens {
        /// Session ID to query
        id: String,
    },
    /// Submit a headless prompt to an existing session and print the turn result
    Prompt {
        /// The message to submit
        #[arg(long)]
        message: String,
        /// Target session ID (defaults to the most recent session)
        #[arg(long)]
        session: Option<String>,
        /// Emit stream events as JSON lines instead of plain text
        #[arg(long)]
        json: bool,
    },
}

/// Validates that `--max-turns` is ≥ 1.
pub(crate) fn parse_max_turns(s: &str) -> Result<usize, String> {
    let n: usize = s
        .parse()
        .map_err(|_| format!("{s:?} is not a valid integer"))?;
    if n == 0 {
        Err("--max-turns must be ≥ 1".to_owned())
    } else {
        Ok(n)
    }
}

/// Tool-gate permission level for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermissionMode {
    /// Standard tool-approval flow (ask before each sensitive tool).
    Default,
    /// Auto-approve file-edit tools; ask for others.
    AcceptEdits,
    /// Skip all tool approval — all tool calls are auto-approved.
    BypassPermissions,
    /// Read-only planning mode; tools that write files are blocked.
    Plan,
}

impl PermissionMode {
    /// Returns the wire string sent to smdjad in the `session.create` payload.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept-edits",
            Self::BypassPermissions => "bypass-permissions",
            Self::Plan => "plan",
        }
    }
}

/// Parses the `--permission-mode` flag value into a [`PermissionMode`].
pub(crate) fn parse_permission_mode(s: &str) -> Result<PermissionMode, String> {
    match s {
        "default" => Ok(PermissionMode::Default),
        "accept-edits" => Ok(PermissionMode::AcceptEdits),
        "bypass-permissions" => Ok(PermissionMode::BypassPermissions),
        "plan" => Ok(PermissionMode::Plan),
        other => Err(format!(
            "unknown permission mode {other:?}; valid values: default, accept-edits, bypass-permissions, plan"
        )),
    }
}

/// Dispatches a `smj session` subcommand.
pub(crate) async fn run(sock: &Path, action: SessionCmd) -> Result<()> {
    if let SessionCmd::Blocks { id } = action {
        eprintln!("smj session blocks: 'session.blocks' RPC not yet implemented");
        eprintln!("  session: {id}");
        std::process::exit(1);
    }
    let mut client = connect_or_exit(sock).await;
    match action {
        SessionCmd::Start {
            cowork,
            task,
            max_turns,
            max_budget_usd,
            permission_mode,
        } => {
            let mut payload = json!({
                "cowork_mode": cowork,
                "task_description": task,
            });
            if let Some(n) = max_turns {
                payload["max_turns"] = json!(n);
            }
            if let Some(b) = max_budget_usd {
                payload["max_budget_usd"] = json!(b);
            }
            if let Some(m) = permission_mode {
                payload["permission_mode"] = json!(m.as_str());
            }
            let resp = client
                .call("session.create", payload)
                .await
                .context("session.create failed")?;
            let session_id = resp["id"].as_str().unwrap_or("?");
            println!("Session: {session_id}");
            if let Some(task_id) = resp["task_id"].as_str() {
                println!("Task created: {task_id}");
            }
        }
        SessionCmd::List => cmd_session_list(&mut client).await?,
        SessionCmd::Show { id } => cmd_session_show(&mut client, &id).await?,
        SessionCmd::Rollback { id, turn } => {
            cmd_session_rollback(&mut client, &id, turn).await?;
        }
        SessionCmd::Fork { id, .. } => {
            let resp = client
                .call("session.fork", json!({ "session_id": id }))
                .await
                .context("session.fork failed")?;
            println!(
                "Forked: {} → {}",
                id,
                resp["session_id"].as_str().unwrap_or("?")
            );
        }
        SessionCmd::Checkpoint { id } => {
            let resp = client
                .call("session.checkpoint.list", json!({ "session_id": id }))
                .await
                .context("session.checkpoint.list failed")?;
            if let Some(arr) = resp.as_array() {
                for cp in arr {
                    println!(
                        "turn={} ts={} messages={}",
                        cp["turn_n"].as_i64().unwrap_or(-1),
                        cp["created_at"].as_f64().unwrap_or(0.0),
                        cp["message_count"].as_u64().unwrap_or(0),
                    );
                }
            }
        }
        SessionCmd::Blocks { .. } => unreachable!(),
        SessionCmd::Export { id, format } => {
            if format == "md" {
                let resp = client
                    .call(
                        "session.messages",
                        json!({ "session_id": id, "limit": 1000 }),
                    )
                    .await
                    .context("session.messages failed")?;
                let messages = resp["messages"].as_array().cloned().unwrap_or_default();
                println!("# Session {id}\n");
                for msg in &messages {
                    let role = msg["role"].as_str().unwrap_or("unknown");
                    let content = msg["content"].as_str().unwrap_or("");
                    match role {
                        "user" => println!("**User:**\n\n{content}\n"),
                        "assistant" => println!("**Assistant:**\n\n{content}\n"),
                        _ => println!("**{role}:**\n\n{content}\n"),
                    }
                }
            } else {
                let resp = client
                    .call("session.export", json!({ "id": id }))
                    .await
                    .context("session.export failed")?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        }
        SessionCmd::Compact { id } => {
            let resp = client
                .call("session.compact", json!({ "session_id": id }))
                .await
                .context("session.compact failed")?;
            let summary = resp["summary"].as_str().unwrap_or("(no summary)");
            println!("Compaction summary for {id}:\n{summary}");
        }
        SessionCmd::Tokens { id } => {
            let resp = client
                .call("session.token_usage", json!({ "session_id": id }))
                .await
                .context("session.token_usage failed")?;
            if let Some(arr) = resp.as_array() {
                println!(
                    "{:<8} {:<12} {:<12} {:<16} {:<16}",
                    "turn_n", "input_tok", "output_tok", "cum_input", "cum_output"
                );
                println!("{}", "-".repeat(68));
                for snap in arr {
                    println!(
                        "{:<8} {:<12} {:<12} {:<16} {:<16}",
                        snap["turn_n"].as_i64().unwrap_or(-1),
                        snap["input_tok"].as_i64().unwrap_or(0),
                        snap["output_tok"].as_i64().unwrap_or(0),
                        snap["cumulative_input"].as_i64().unwrap_or(0),
                        snap["cumulative_output"].as_i64().unwrap_or(0),
                    );
                }
            }
        }
        SessionCmd::Prompt {
            message,
            session,
            json: json_output,
        } => {
            let mut payload = json!({ "content": message });
            if let Some(sid) = session {
                payload["session_id"] = json!(sid);
            }
            let resp = client
                .call("turn.submit", payload)
                .await
                .context("turn.submit failed")?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else if let Some(task_id) = resp["task_id"].as_str() {
                println!("{task_id}");
            } else {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        }
    }
    Ok(())
}

async fn cmd_session_list(client: &mut Client) -> Result<()> {
    let resp = client
        .call("session.list", serde_json::Value::Null)
        .await
        .context("session.list failed")?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

async fn cmd_session_show(client: &mut Client, id: &str) -> Result<()> {
    let resp = client
        .call("session.get", json!({"id": id}))
        .await
        .context("session.get failed")?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    let cowork = resp["cowork_mode"].as_bool().unwrap_or(false);
    println!("Cowork mode: {}", if cowork { "yes" } else { "no" });
    if let Some(task_id) = resp["task_id"].as_str() {
        println!("Active task: {task_id}");
    }
    Ok(())
}

async fn cmd_session_rollback(client: &mut Client, session_id: &str, turn: u32) -> Result<()> {
    let resp = client
        .call(
            "session.rollback",
            json!({"session_id": session_id, "turn_n": turn}),
        )
        .await
        .context("session.rollback failed")?;
    println!(
        "Rolled back to turn {}: {}",
        turn,
        serde_json::to_string_pretty(&resp)?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser as _;

    // -------------------------------------------------------------------------
    // M9 — Permission mode flag
    // -------------------------------------------------------------------------

    #[test]
    fn permission_mode_default_accepted() {
        let mode = parse_permission_mode("default").unwrap();
        assert_eq!(mode, PermissionMode::Default);
        assert_eq!(mode.as_str(), "default");
    }

    #[test]
    fn permission_mode_bypass_accepted() {
        let mode = parse_permission_mode("bypass-permissions").unwrap();
        assert_eq!(mode, PermissionMode::BypassPermissions);
        assert_eq!(mode.as_str(), "bypass-permissions");
    }

    #[test]
    fn permission_mode_invalid_rejected() {
        let result = parse_permission_mode("nonsense");
        assert!(result.is_err(), "unknown mode must return an error");
        assert!(
            result.unwrap_err().contains("unknown permission mode"),
            "error must describe the problem"
        );
    }

    // -------------------------------------------------------------------------
    // M8 — Session guards and headless flags
    // -------------------------------------------------------------------------

    #[test]
    fn max_turns_zero_is_rejected() {
        let result = parse_max_turns("0");
        assert!(result.is_err(), "max-turns 0 must be rejected");
        assert!(result.unwrap_err().contains("≥ 1"));
    }

    #[test]
    fn max_turns_positive_is_accepted() {
        let result = parse_max_turns("5");
        assert_eq!(result.unwrap(), 5);
    }

    #[test]
    fn session_prompt_json_flag_accepted() {
        // Verify the SessionCmd::Prompt variant parses with --json by checking the
        // parse_max_turns helper and the variant shape — we do not run a live RPC.
        assert!(matches!(parse_max_turns("10"), Ok(10)));
        // Confirm the CLI still parses a session prompt with --json.
        let cli = Cli::try_parse_from(["smj", "session", "prompt", "--message", "hi", "--json"])
            .expect("session prompt --json must parse");
        assert!(matches!(
            cli.command,
            crate::cli::Cmd::Session {
                action: SessionCmd::Prompt { json: true, .. }
            }
        ));
    }
}

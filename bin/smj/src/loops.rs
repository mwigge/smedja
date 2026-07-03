//! `smj loop` — loop engine control (run/status/cancel/retire/list).

use std::path::Path;

use anyhow::{Context as _, Result};
use clap::Subcommand;
use serde_json::json;
use smedja_ingot::Ingot;

use crate::util::{connect, default_ingot_path};

#[derive(Subcommand)]
pub(crate) enum LoopCmd {
    /// Run a loop against an `OpenSpec` change
    Run {
        /// Name of the `OpenSpec` change to drive
        #[arg(long)]
        change: String,
        /// Maximum number of task slices to process
        #[arg(long, default_value = "10")]
        max_slices: u32,
        /// Stream loop progress events to stdout (default true; use --no-follow to detach).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        follow: bool,
    },
    /// Show loop status for a change
    Status {
        /// Name of the `OpenSpec` change to query
        #[arg(long)]
        change: String,
    },
    /// Cancel a running loop for a change
    Cancel {
        /// Name of the `OpenSpec` change to cancel
        #[arg(long)]
        change: String,
    },
    /// Retire a completed or failed loop
    Retire {
        /// Name of the `OpenSpec` change whose loop to retire
        #[arg(long)]
        change: String,
    },
    /// List loops, optionally filtered by status
    List {
        /// Filter by loop status (e.g. `complete`, `failed`, `retired`)
        #[arg(long)]
        status: Option<String>,
    },
}

/// Dispatches a `smj loop` subcommand.
pub(crate) async fn run(sock: &Path, action: LoopCmd) -> Result<()> {
    let mut client = connect(sock).await?;
    match action {
        LoopCmd::Run {
            change,
            max_slices,
            follow,
        } => {
            let resp = client
                .call(
                    "loop.create",
                    json!({ "change_name": change, "max_slices": max_slices }),
                )
                .await
                .context("loop.create failed")?;
            let loop_id = resp["loop_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("loop.create returned no loop_id"))?
                .to_owned();
            client
                .call("loop.run", json!({ "loop_id": loop_id }))
                .await
                .context("loop.run failed")?;
            println!("Loop {loop_id} running");
            if follow {
                let stream_sock = {
                    let mut p = sock.as_os_str().to_owned();
                    p.push(".stream");
                    std::path::PathBuf::from(p)
                };
                follow_loop(&stream_sock, &loop_id)
                    .await
                    .unwrap_or_else(|e| {
                        eprintln!("warning: could not follow loop stream: {e}");
                    });
            }
        }
        LoopCmd::Status { change } => {
            let resp = client
                .call("loop.list", json!({ "change_name": change }))
                .await
                .context("loop.list failed")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&resp).unwrap_or_default()
            );
        }
        LoopCmd::Cancel { change } => {
            let resp = client
                .call("loop.list", json!({ "change_name": change }))
                .await
                .context("loop.list failed")?;
            let active = resp["loops"]
                .as_array()
                .and_then(|arr| {
                    arr.iter().find(|r| {
                        let status = r["status"].as_str().unwrap_or("");
                        status != "cancelled" && status != "done"
                    })
                })
                .and_then(|r| r["id"].as_str())
                .map(str::to_owned);
            match active {
                Some(loop_id) => {
                    client
                        .call("loop.cancel", json!({ "loop_id": loop_id }))
                        .await
                        .context("loop.cancel failed")?;
                    println!("Loop {loop_id} cancelled");
                }
                None => {
                    println!("No active loop for change {change}");
                }
            }
        }
        LoopCmd::Retire { change } => {
            // Find the most recent complete or failed loop for this change.
            let resp = client
                .call("loop.list", json!({ "change_name": change }))
                .await
                .context("loop.list failed")?;
            let loop_id = resp["loops"]
                .as_array()
                .and_then(|arr| {
                    arr.iter().find(|r| {
                        let status = r["status"].as_str().unwrap_or("");
                        status == "complete" || status == "failed"
                    })
                })
                .and_then(|r| r["id"].as_str())
                .map(str::to_owned)
                .ok_or_else(|| {
                    anyhow::anyhow!("No complete or failed loop found for change '{change}'")
                })?;
            client
                .call("loop.retire", json!({ "loop_id": loop_id }))
                .await
                .context("loop.retire failed")?;
            // Export audit events for the change to a JSONL file.
            let db_path = default_ingot_path();
            let out_path = format!("{change}.loop-audit.jsonl");
            if let Ok(ingot) = Ingot::open(&db_path) {
                if let Ok(events) = ingot.list_all_audit_events() {
                    if let Ok(mut f) = std::fs::File::create(&out_path) {
                        use std::io::Write as _;
                        for ev in &events {
                            if let Ok(line) = serde_json::to_string(ev) {
                                let _ = writeln!(f, "{line}");
                            }
                        }
                    }
                }
            }
            println!("Loop {loop_id} retired");
            println!("Audit export: {out_path}");
        }
        LoopCmd::List { status } => {
            let mut params = json!({});
            if let Some(s) = status {
                params["status"] = serde_json::Value::String(s);
            }
            let resp = client
                .call("loop.list_by_status", params)
                .await
                .context("loop.list_by_status failed")?;
            if let Some(loops) = resp["loops"].as_array() {
                println!(
                    "{:<36} {:<20} {:<12} {:<16} updated_at",
                    "id", "change_name", "status", "created_at"
                );
                println!("{}", "-".repeat(100));
                for rec in loops {
                    println!(
                        "{:<36} {:<20} {:<12} {:<16} {}",
                        rec["id"].as_str().unwrap_or("?"),
                        rec["change_name"].as_str().unwrap_or("?"),
                        rec["status"].as_str().unwrap_or("?"),
                        rec["created_at"].as_f64().unwrap_or(0.0),
                        rec["updated_at"].as_f64().unwrap_or(0.0),
                    );
                }
            }
        }
    }
    Ok(())
}

/// Connects to the smdjad stream socket and prints NDJSON events for `loop_id`
/// until a terminal state (`done`, `error`, `policy_tampered`) is received or
/// the connection is closed.
async fn follow_loop(stream_sock: &Path, loop_id: &str) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(stream_sock)
        .await
        .context("cannot connect to stream socket")?;
    let req = format!("{{\"task_id\":\"{loop_id}\"}}\n");
    stream.write_all(req.as_bytes()).await?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        println!("{trimmed}");
        if trimmed.contains(r#""type":"done""#)
            || trimmed.contains(r#""type":"error""#)
            || trimmed.contains(r#""type":"policy_tampered""#)
        {
            break;
        }
    }
    Ok(())
}

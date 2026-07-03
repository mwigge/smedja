//! `smj cost` — per-session and all-sessions cost ledger.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::json;

use crate::util::connect;

/// Dispatches the `smj cost` command.
pub(crate) async fn run(sock: &Path, session: Option<String>, json: bool) -> Result<()> {
    let mut client = connect(sock).await?;
    if let Some(session_id) = session {
        let resp = client
            .call("session.cost", json!({"session_id": session_id}))
            .await
            .context("session.cost failed")?;
        if json {
            println!("{}", serde_json::to_string_pretty(&resp)?);
        } else {
            let usd = resp["total_usd"].as_f64().unwrap_or(0.0);
            println!("SESSION  {session_id}  TOTAL  ${usd:.6}");
            if let Some(rows) = resp["breakdown"].as_array() {
                if !rows.is_empty() {
                    println!();
                    println!(
                        "{:<32}  {:<12}  {:>5}  {:>8}  {:>8}  {:>10}",
                        "MODEL", "RUNNER", "TURNS", "INPUT", "OUTPUT", "COST"
                    );
                    println!("{}", "-".repeat(82));
                    for row in rows {
                        let model = row["model"].as_str().unwrap_or("-");
                        let runner = row["runner"].as_str().unwrap_or("-");
                        let turns = row["turns"].as_i64().unwrap_or(0);
                        let input = row["input_tok"].as_i64().unwrap_or(0);
                        let output = row["output_tok"].as_i64().unwrap_or(0);
                        let cost = row["cost_usd"].as_f64().unwrap_or(0.0);
                        println!(
                            "{model:<32}  {runner:<12}  {turns:>5}  {input:>8}  {output:>8}  ${cost:.6}",
                        );
                    }
                }
            }
        }
    } else {
        // All-sessions summary
        let sessions_resp = client
            .call("session.list", json!({}))
            .await
            .context("session.list failed")?;
        let sessions = sessions_resp["sessions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if json {
            let mut all_costs: Vec<serde_json::Value> = Vec::new();
            for sess in &sessions {
                let sid = sess["id"].as_str().unwrap_or_default();
                if let Ok(cost_resp) = client
                    .call("session.cost", json!({"session_id": sid}))
                    .await
                {
                    all_costs.push(cost_resp);
                }
            }
            println!("{}", serde_json::to_string_pretty(&all_costs)?);
        } else {
            println!(
                "{:<32}  {:<12}  {:>5}  {:>8}  {:>8}  {:>10}",
                "model", "runner", "turns", "in_tok", "out_tok", "cost_usd"
            );
            println!("{}", "-".repeat(80));
            let mut total_cost = 0.0f64;
            for sess in &sessions {
                let sid = sess["id"].as_str().unwrap_or_default();
                if let Ok(cost_resp) = client
                    .call("session.cost", json!({"session_id": sid}))
                    .await
                {
                    if let Some(rows) = cost_resp["breakdown"].as_array() {
                        for row in rows {
                            let model = row["model"].as_str().unwrap_or("-");
                            let runner = row["runner"].as_str().unwrap_or("-");
                            let turns = row["turns"].as_i64().unwrap_or(0);
                            let input = row["input_tok"].as_i64().unwrap_or(0);
                            let output = row["output_tok"].as_i64().unwrap_or(0);
                            let cost = row["cost_usd"].as_f64().unwrap_or(0.0);
                            total_cost += cost;
                            println!(
                                "{model:<32}  {runner:<12}  {turns:>5}  {input:>8}  {output:>8}  ${cost:.6}",
                            );
                        }
                    }
                }
            }
            println!("{}", "-".repeat(80));
            println!("{:<56}  ${total_cost:.6}", "TOTAL");
        }
    }
    Ok(())
}

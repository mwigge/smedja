//! `smj timeline` — conversation timeline inspection (local Agent Timeline view).

use anyhow::{Context as _, Result};
use clap::Subcommand;
use smedja_ingot::Ingot;

use crate::util::default_ingot_path;

#[derive(Subcommand)]
pub(crate) enum TimelineCmd {
    /// List recent conversations with rollup statistics
    Conversations {
        /// Only show conversations from the last N seconds (e.g. 3600 for last hour)
        #[arg(long)]
        since: Option<u64>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show ordered timeline events for a conversation
    Show {
        /// Conversation ID
        conversation_id: String,
        /// Only show failure events
        #[arg(long)]
        failures_only: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Open a conversation in a configured backend (`Honeycomb`, `SigNoz`, etc.)
    Open {
        /// Conversation ID, trace ID, or span ID
        id: String,
    },
}

/// Dispatches a `smj timeline` subcommand.
pub(crate) fn run(action: TimelineCmd) -> Result<()> {
    let db_path = default_ingot_path();
    let ingot = Ingot::open(&db_path)
        .with_context(|| format!("failed to open ingot at {}", db_path.display()))?;
    match action {
        TimelineCmd::Conversations { since, json } => {
            let rollups = ingot.recent_conversations(50)?;
            let rollups: Vec<_> = if let Some(since_secs) = since {
                let cutoff = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .saturating_sub(since_secs)
                    .try_into()
                    .unwrap_or(i64::MAX);
                rollups
                    .into_iter()
                    .filter(|r| r.started_at >= cutoff)
                    .collect()
            } else {
                rollups
            };
            if json {
                let arr: Vec<serde_json::Value> = rollups
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "conversation_id": r.conversation_id,
                            "started_at": r.started_at,
                            "last_seen_at": r.last_seen_at,
                            "agent_count": r.agent_count,
                            "llm_call_count": r.llm_call_count,
                            "tool_call_count": r.tool_call_count,
                            "failure_count": r.failure_count,
                            "input_token_total": r.input_token_total,
                            "output_token_total": r.output_token_total,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else if rollups.is_empty() {
                println!("No conversations found.");
            } else {
                println!(
                    "{:<40} {:>8} {:>8} {:>6} {:>6}",
                    "CONVERSATION", "LLM", "TOOLS", "FAIL", "TOKENS"
                );
                for r in &rollups {
                    println!(
                        "{:<40} {:>8} {:>8} {:>6} {:>6}",
                        &r.conversation_id[..r.conversation_id.len().min(40)],
                        r.llm_call_count,
                        r.tool_call_count,
                        r.failure_count,
                        r.input_token_total + r.output_token_total,
                    );
                }
            }
        }
        TimelineCmd::Show {
            conversation_id,
            failures_only,
            json,
        } => {
            let events = if failures_only {
                ingot.failed_events(&conversation_id)?
            } else {
                ingot.conversation_timeline(&conversation_id)?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&events)?);
            } else if events.is_empty() {
                println!("No events for conversation {conversation_id}");
            } else {
                for ev in &events {
                    println!(
                        "{:.0} {:12} {:8} {:<30} trace:{} span:{}",
                        ev.ts.as_secs_f64(),
                        ev.action_type,
                        ev.status.as_deref().unwrap_or("-"),
                        ev.tool_name.as_deref().unwrap_or(ev.actor.as_str()),
                        ev.trace_id.as_deref().unwrap_or("-"),
                        ev.span_id.as_deref().unwrap_or("-"),
                    );
                }
            }
        }
        TimelineCmd::Open { id } => {
            let template = std::env::var("SMEDJA_TIMELINE_URL").unwrap_or_default();
            if template.is_empty() {
                println!("Set SMEDJA_TIMELINE_URL to open traces in a backend.");
                println!("Example (Honeycomb): SMEDJA_TIMELINE_URL=https://ui.honeycomb.io/your-team/environments/prod/trace?trace_id={{id}}");
                println!("ID: {id}");
            } else {
                let url = template.replace("{id}", &id);
                println!("{url}");
            }
        }
    }
    Ok(())
}

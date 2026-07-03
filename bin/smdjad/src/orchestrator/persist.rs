//! Post-loop persistence for a completed turn: cost/savings ledgers, the
//! conversation checkpoint, the per-turn token snapshot, and the auto-summarise
//! compaction step. Extracted from `TurnOrchestrator::run` as methods so they
//! keep direct access to the orchestrator's private fields.

use std::sync::Arc;

use smedja_adapter::types::{Message as AdapterMessage, Role as AdapterRole};
use smedja_bellows::event::CorrelationCtx;
use smedja_ingot::{Checkpoint, CostEntry, TokenSnapshot, TokensSavedEntry};
use smedja_memory::WorkingMemory;
use smedja_types::{Microdollars, Timestamp};
use tracing::warn;
use uuid::Uuid;

use super::TurnOrchestrator;

impl TurnOrchestrator {
    /// Records the turn's cost entry, source-tagged token savings, conversation
    /// checkpoint, and cumulative token snapshot. All writes are advisory: a
    /// ledger error is logged and swallowed and never breaks the turn.
    #[allow(clippy::too_many_arguments)] // settled per-turn totals threaded from run
    pub(super) async fn record_turn_metrics(
        &self,
        session_id: &str,
        turn_id: &str,
        runner: &str,
        model: String,
        turn_n: i64,
        total_input_tokens: u32,
        total_output_tokens: u32,
        total_cache_read_tokens: u32,
        total_cold_omitted_tokens: usize,
        mem: &WorkingMemory,
    ) {
        // 7. Record cost entry.
        {
            let cost_usd =
                self.price_table
                    .compute_cost(&model, total_input_tokens, total_output_tokens);
            let entry = CostEntry {
                id: Uuid::new_v4(),
                session_id: session_id.to_owned(),
                turn_n,
                runner: runner.to_owned(),
                model,
                input_tok: i64::from(total_input_tokens),
                output_tok: i64::from(total_output_tokens),
                cost_usd: Microdollars::from_usd_f64(cost_usd),
                created_at: Timestamp::now(),
                change_name: self.active_change.clone(),
            };
            if let Err(e) = self.ingot.insert_cost(entry).await {
                warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to record cost entry");
            }
        }

        // 7b. Record savings, source-tagged, on the tokens-saved ledger (never
        // the billed cost_ledger). Cache reads are provider-reported "input not
        // re-paid" (source=cache); cold-context omission is the dropped-token
        // estimate (source=cold-context). Both are advisory: a ledger error is
        // logged and swallowed and must never break the turn. Zero-valued
        // savings write no row.
        {
            if total_cache_read_tokens > 0 {
                let entry = TokensSavedEntry {
                    id: Uuid::new_v4(),
                    session_id: session_id.to_owned(),
                    turn_n,
                    command: "cache_read".to_owned(),
                    tokens_saved: i64::from(total_cache_read_tokens),
                    source: "cache".to_owned(),
                    created_at: Timestamp::now(),
                };
                if let Err(e) = self.ingot.insert_tokens_saved(entry).await {
                    warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to record cache savings");
                }
            }
            if total_cold_omitted_tokens > 0 {
                let entry = TokensSavedEntry {
                    id: Uuid::new_v4(),
                    session_id: session_id.to_owned(),
                    turn_n,
                    command: "cold_context".to_owned(),
                    tokens_saved: i64::try_from(total_cold_omitted_tokens).unwrap_or(i64::MAX),
                    source: "cold-context".to_owned(),
                    created_at: Timestamp::now(),
                };
                if let Err(e) = self.ingot.insert_tokens_saved(entry).await {
                    warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to record cold-context savings");
                }
            }
        }

        // 8. Save checkpoint.
        {
            let messages_json_value: Vec<serde_json::Value> = mem
                .messages()
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "role": match m.role {
                            AdapterRole::User => "user",
                            AdapterRole::Assistant => "assistant",
                            AdapterRole::System => "system",
                            AdapterRole::Tool => "tool",
                        },
                        "content": m.content,
                    })
                })
                .collect();
            let cp = Checkpoint {
                id: Uuid::new_v4(),
                session_id: session_id.to_owned(),
                turn_n,
                messages_json: serde_json::to_string(&messages_json_value)
                    .unwrap_or_else(|_| "[]".to_owned()),
                created_at: Timestamp::now(),
                compaction_id: None,
            };
            if let Err(e) = self.ingot.save_checkpoint(cp).await {
                warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to save checkpoint");
            }
        }

        // 9. Save per-turn token snapshot.
        {
            let input_tok = i64::from(total_input_tokens);
            let output_tok = i64::from(total_output_tokens);
            let (prior_in, prior_out) = self
                .ingot
                .session_token_snapshots(session_id)
                .await
                .map_or((0, 0), |snaps| {
                    snaps
                        .last()
                        .map_or((0i64, 0i64), |s| (s.cumulative_input, s.cumulative_output))
                });
            let snap = TokenSnapshot {
                id: Uuid::new_v4(),
                session_id: session_id.to_owned(),
                turn_n,
                input_tok,
                output_tok,
                cumulative_input: prior_in + input_tok,
                cumulative_output: prior_out + output_tok,
                created_at: Timestamp::now(),
            };
            if let Err(e) = self.ingot.save_token_snapshot(snap).await {
                warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to save token snapshot");
            }
        }
    }

    /// Auto-summarises the conversation when context pressure exceeds the
    /// configured threshold, storing the summary in the `compact` vault namespace
    /// and publishing a `HistoryReplaced` event. All failures are logged and
    /// swallowed so the turn always completes.
    pub(super) async fn maybe_auto_summarise(
        &self,
        mem: &WorkingMemory,
        session_id: &str,
        turn_id: &str,
        turn_n: i64,
        total_input_tokens: u32,
        context_window: usize,
    ) {
        let cpt = super::budget::compact_threshold_from_env(
            std::env::var("SMEDJA_COMPACT_THRESHOLD").ok().as_deref(),
        );
        if super::budget::context_pressure_exceeds_threshold(
            total_input_tokens,
            context_window,
            cpt,
        ) {
            let history: Vec<(String, String)> = mem
                .messages()
                .iter()
                .map(|m| {
                    let role = match m.role {
                        AdapterRole::User => "user",
                        AdapterRole::Assistant => "assistant",
                        _ => "system",
                    };
                    (role.to_owned(), m.content.clone())
                })
                .collect();
            let prompt = super::budget::build_summariser_prompt(&history);
            let pool_entry = self
                .pool
                .get(smedja_assayer::Runner::Claude, smedja_assayer::Tier::Fast)
                .or_else(|| {
                    self.pool
                        .get(smedja_assayer::Runner::Codex, smedja_assayer::Tier::Fast)
                })
                .or_else(|| self.pool.get_default());
            if let Some(entry) = pool_entry {
                let sum_opts = smedja_adapter::CallOptions {
                    model: std::env::var("SMEDJA_SUMMARISER_MODEL")
                        .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_owned()),
                    max_tokens: Some(512),
                    temperature: Some(0.3),
                    system: Some("You are a summarisation assistant.".to_owned()),
                    tools: None,
                    provider_session_id: None,
                    smedja_session_id: None,
                    permission_mode: None,
                    stable_prefix_len: None,
                    cache_strategy: smedja_adapter::CacheStrategy::None,
                    workspace: None,
                };
                let stream = entry
                    .provider
                    .stream_chat(&[AdapterMessage::user(prompt)], &sum_opts);
                let sum_dispatcher = smedja_bellows::Dispatcher::new(1);
                match crate::common::drain_stream(
                    stream,
                    &sum_dispatcher,
                    None,
                    &CorrelationCtx::default(),
                )
                .await
                {
                    Ok((summary, _, _, _, _)) if !summary.is_empty() => {
                        let summary_tokens = summary.split_whitespace().count();
                        let embedding = self.embedder.embed_query(&summary).await;
                        let model_id = self.embedder.model_id().to_owned();
                        let dim = self.embedder.dim();
                        let compact_sid = session_id.to_owned();
                        let vault = Arc::clone(&self.vault);
                        tokio::task::spawn_blocking(move || {
                            use smedja_vault::VaultEntry;
                            let entry = VaultEntry {
                                id: format!("compact:{compact_sid}:{turn_n}"),
                                embedding,
                                payload: serde_json::json!({
                                    "session_id": compact_sid,
                                    "turn_n": turn_n,
                                }),
                                namespace: "compact".to_owned(),
                                content: summary,
                                source_file: None,
                                added_by: Some("auto-summarise".to_owned()),
                                chunk_index: None,
                                parent_id: None,
                                created_at: 0.0,
                                embedder_model_id: model_id,
                                dim,
                            };
                            let mut guard = vault.blocking_lock();
                            if let Err(e) = guard.upsert(&entry) {
                                tracing::warn!(error = %e, "auto-summarise: vault upsert failed");
                            }
                        });
                        self.dispatcher
                            .publish(smedja_bellows::TurnEvent::HistoryReplaced {
                                session_id: session_id.to_owned(),
                                turn_id: turn_id.to_owned(),
                                summary_tokens,
                            });
                        tracing::info!(
                            session_id = %session_id,
                            turn_n,
                            threshold = cpt,
                            summary_tokens,
                            "auto-summarise: context compacted"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "auto-summarise: summariser call failed; continuing");
                    }
                }
            }
        }
    }
}

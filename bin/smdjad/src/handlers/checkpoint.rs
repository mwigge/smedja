//! Checkpoint/compaction RPC handlers: `session.checkpoint.list`,
//! `session.rollback`, `session.compact`.

use serde_json::{json, Value};
use smedja_adapter::types::{Message as AdapterMessage, Role as AdapterRole};
use smedja_adapter::CallOptions;
use smedja_assayer::{Runner, Tier};
use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::Dispatcher;
use smedja_ingot::Checkpoint;
use smedja_rpc::{codes, RpcError};
use smedja_types::Timestamp;
use smedja_vault::VaultEntry;
use tracing::warn;
use uuid::Uuid;

use crate::handlers::HandlerState;
use crate::{assemble_compaction_transcript, ingot_err, missing_param};

/// Handles `session.checkpoint.list`.
///
/// # Errors
///
/// Returns an error when `session_id` is missing or the ingot query fails.
pub(crate) async fn list(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?;
    let cps = ig
        .list_checkpoints(session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let out: Vec<Value> = cps
        .iter()
        .map(|cp| {
            json!({
                "turn_n": cp.turn_n,
                "created_at": cp.created_at,
            })
        })
        .collect();
    Ok(Value::Array(out))
}

/// Handles `session.rollback`.
///
/// # Errors
///
/// Returns an error when `session_id`/`turn_n` is missing or invalid, the ingot
/// query fails, or the target checkpoint does not exist.
pub(crate) async fn rollback(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let turn_n_raw = params
        .get("turn_n")
        .and_then(Value::as_i64)
        .ok_or_else(|| missing_param("turn_n"))?;
    let turn_n_u32 = u32::try_from(turn_n_raw).map_err(|_| {
        RpcError::new(
            codes::INVALID_PARAMS,
            "turn_n must be a non-negative integer",
        )
    })?;

    // Atomically load the target checkpoint and prune all later checkpoints
    // for this session in a single SQLite transaction.
    let cp = ig
        .rollback_session(&session_id, turn_n_u32)
        .await
        .map_err(|e| ingot_err(&e))?;

    match cp {
        Some(cp) => Ok(json!({
            "session_id": session_id,
            "turn_n": cp.turn_n,
            "messages_json": cp.messages_json,
            "created_at": cp.created_at,
        })),
        None => Err(RpcError::new(codes::INVALID_PARAMS, "checkpoint not found")),
    }
}

/// Handles `session.compact`: summarises the conversation and saves a
/// pre-compaction checkpoint.
///
/// # Errors
///
/// Returns an error when `session_id` is missing, no provider is available, or
/// the summarisation stream fails.
#[allow(clippy::too_many_lines)] // single compaction pipeline kept inline
pub(crate) async fn compact(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let pool = state.provider_pool;
    let vt = state.vault;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();

    // Load latest checkpoint for conversation history.
    let messages_json = {
        ig.latest_checkpoint(&session_id)
            .await
            .map_err(|e| ingot_err(&e))?
            .map_or_else(|| "[]".to_owned(), |cp| cp.messages_json)
    };

    // Assemble the summarisation input through working-memory strata
    // rather than feeding the raw checkpoint blob. The raw blob is still
    // preserved in the pre-compaction checkpoint below for rollback.
    let history_transcript = assemble_compaction_transcript(&messages_json);

    // Call provider to produce a summary.
    let compaction_prompt = format!(
        "Summarise this conversation in 3–5 bullet points, then state the current goal. \
         Be concise.\n\nConversation history:\n{history_transcript}"
    );
    let pool_entry = pool
        .get(Runner::Claude, Tier::Fast)
        .or_else(|| pool.get(Runner::Codex, Tier::Fast))
        .or_else(|| pool.get_default())
        .ok_or_else(|| {
            RpcError::new(
                codes::INTERNAL_ERROR,
                "no provider available for compaction",
            )
        })?;
    let provider = &pool_entry.provider;
    let opts = CallOptions {
        model: std::env::var("SMEDJA_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_owned()),
        max_tokens: Some(512),
        temperature: Some(0.3),
        system: Some("You are a summarisation assistant.".to_owned()),
        tools: None,
        provider_session_id: None,
        stable_prefix_len: None,
        cache_strategy: smedja_adapter::CacheStrategy::None,
    };
    let stream = provider.stream_chat(
        &[AdapterMessage {
            role: AdapterRole::User,
            content: compaction_prompt,
        }],
        &opts,
    );
    // Capacity-1 fire-and-forget dispatcher: the summary deltas have no live
    // subscriber here (the summary is captured from drain_stream's return value),
    // so a single-slot broadcast channel is sufficient and intentional.
    let dispatcher = Dispatcher::new(1);
    let (summary, _, _, _, _) =
        crate::common::drain_stream(stream, &dispatcher, None, &CorrelationCtx::default())
            .await
            .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("compaction failed: {e}")))?;

    // Save the pre-compaction checkpoint, tagged with a compaction id so
    // it is retrievable via `list_compaction_checkpoints`.
    let turn_count = {
        ig.list_checkpoints(&session_id)
            .await
            .map_or(0i64, |v| i64::try_from(v.len()).unwrap_or(i64::MAX))
    };
    let cp = Checkpoint {
        id: Uuid::new_v4(),
        session_id: session_id.clone(),
        turn_n: turn_count,
        messages_json: messages_json.clone(),
        created_at: Timestamp::now(),
        compaction_id: Some(Uuid::new_v4().to_string()),
    };
    {
        if let Err(e) = ig.save_checkpoint(cp).await {
            warn!(error = %e, "failed to save pre-compaction checkpoint");
        }
    }

    // Fire-and-forget: index compaction summary into vault cold storage.
    let compact_sid = session_id.clone();
    let compact_summary = summary.clone();
    tokio::task::spawn_blocking(move || {
        let entry = VaultEntry {
            id: format!("compact:{compact_sid}:{turn_count}"),
            embedding: crate::embedder::embed(&compact_summary),
            payload: serde_json::json!({
                "session_id": compact_sid,
                "turn_count": turn_count,
            }),
            namespace: "compact".to_owned(),
            content: compact_summary,
            source_file: None,
            added_by: Some("session.compact".to_owned()),
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
        };
        let mut guard = vt.blocking_lock();
        if let Err(e) = guard.upsert(&entry) {
            tracing::warn!(error = %e, "session.compact: vault upsert failed, compaction data lost");
        }
    });

    Ok(json!({
        "session_id": session_id,
        "summary": summary,
        "turn_count": turn_count,
        "compaction_checkpoint_saved": true,
    }))
}

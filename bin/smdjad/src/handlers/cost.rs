//! Cost RPC handlers: `session.cost`, `cost.active_change`.

use serde_json::{json, Value};
use smedja_ingot::CostRow;
use smedja_rpc::RpcError;

use crate::handlers::HandlerState;
use crate::{ingot_err, missing_param};

/// Handles `session.cost`: returns the total spend and per-model breakdown.
///
/// # Errors
///
/// Returns an error when `session_id` is missing or an ingot query fails.
pub(crate) async fn cost(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?;
    let total_usd = ig
        .session_cost(session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let rows: Vec<CostRow> = ig
        .session_cost_entries(session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let breakdown: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "model": r.model,
                "runner": r.runner,
                "turns": r.turns,
                "input_tok": r.input_tok,
                "output_tok": r.output_tok,
                "cost_usd": r.cost_usd.as_usd_f64(),
            })
        })
        .collect();
    Ok(json!({
        "session_id": session_id,
        "total_usd": total_usd.as_usd_f64(),
        "breakdown": breakdown,
    }))
}

/// Handles `cost.active_change`: returns the active openspec change name and
/// cumulative token count attributed to it.
///
/// Returns `{ change_name: null, token_cost: 0 }` when no active change is
/// detected at startup or no audit events carry the change name yet.
///
/// # Errors
///
/// Returns an error when the ingot query fails.
pub(crate) async fn active_change(state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    let Some(ref change) = state.active_change else {
        return Ok(json!({ "change_name": serde_json::Value::Null, "token_cost": 0u64 }));
    };
    let token_cost = state
        .ingot
        .cost_for_change(change)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "change_name": change.as_ref(), "token_cost": token_cost }))
}

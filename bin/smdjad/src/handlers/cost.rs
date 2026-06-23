//! Cost RPC handlers: `session.cost`.

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

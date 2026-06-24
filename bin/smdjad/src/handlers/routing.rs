//! Routing RPC handler: `agent.routing`.
//!
//! Resolves a `(role, complexity)` pair to a concrete routing destination via
//! the daemon's shared [`smedja_assayer::Assayer`], returning the chosen
//! runner, tier, model, complexity, and a human-readable rationale.

use serde_json::{json, Value};
use smedja_assayer::{AgentRole, Complexity};
use smedja_rpc::{codes, RpcError};

use crate::handlers::HandlerState;

/// Parses an agent-role label into an [`AgentRole`].
fn parse_role(raw: &str) -> Option<AgentRole> {
    match raw.to_ascii_lowercase().as_str() {
        "impl" | "implement" | "implementer" => Some(AgentRole::Impl),
        "test" | "tester" => Some(AgentRole::Test),
        "review" | "reviewer" => Some(AgentRole::Review),
        "sre" => Some(AgentRole::Sre),
        "orchestrator" | "orchestrate" => Some(AgentRole::Orchestrator),
        _ => None,
    }
}

/// Parses a complexity label into a [`Complexity`], defaulting to
/// [`Complexity::Coding`] when absent.
fn parse_complexity(raw: Option<&str>) -> Option<Complexity> {
    match raw {
        None => Some(Complexity::Coding),
        Some(s) => match s.to_ascii_lowercase().as_str() {
            "simple" => Some(Complexity::Simple),
            "coding" | "code" | "moderate" => Some(Complexity::Coding),
            "complex" => Some(Complexity::Complex),
            _ => None,
        },
    }
}

/// Handles `agent.routing`.
///
/// Params: `{ role: string, complexity?: string }`.
/// Response: `{ runner, tier, model, complexity, rationale }`.
///
/// # Errors
///
/// Returns [`codes::INVALID_PARAMS`] when `role` is missing or unknown, or when
/// `complexity` is present but unrecognised.
#[allow(clippy::unused_async)] // uniform handler signature: all handlers are async fns
pub(crate) async fn routing(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let role_raw = params
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(codes::INVALID_PARAMS, "missing required param: role"))?;
    let role = parse_role(role_raw)
        .ok_or_else(|| RpcError::new(codes::INVALID_PARAMS, format!("unknown role: {role_raw}")))?;

    let complexity_raw = params.get("complexity").and_then(Value::as_str);
    let complexity = parse_complexity(complexity_raw).ok_or_else(|| {
        RpcError::new(
            codes::INVALID_PARAMS,
            format!("unknown complexity: {}", complexity_raw.unwrap_or("")),
        )
    })?;

    let decision = state.assayer.route_decision(role, complexity);
    Ok(json!({
        "runner": decision.runner(),
        "tier": decision.tier(),
        "model": decision.model(),
        "complexity": decision.complexity(),
        "rationale": decision.rationale(),
    }))
}

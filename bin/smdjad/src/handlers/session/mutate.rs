//! Session mutation handlers: model / runner / tier / title / mode setters and
//! the runner/tier name parsers. Moved verbatim from `session.rs`.

use super::*;

/// Handles `session.set_model`.
///
/// # Errors
///
/// Returns an error when `session_id`/`model` is missing or the ingot write fails.
pub(crate) async fn set_model(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let model = params["model"]
        .as_str()
        .ok_or_else(|| missing_param("model"))?
        .to_owned();
    ig.update_session_model_override(&session_id, &model)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "session_id": session_id, "model": model }))
}

/// Parses a runner name (tolerating the `-cli` suffix, e.g. `claude-cli`) into
/// a [`Runner`]. Returns `None` for unknown runners.
pub(crate) fn parse_runner_name(s: &str) -> Option<smedja_assayer::Runner> {
    use smedja_assayer::Runner;
    match s.trim().to_ascii_lowercase().split('-').next()? {
        "claude" => Some(Runner::Claude),
        "codex" => Some(Runner::Codex),
        "kimi" | "moonshot" => Some(Runner::Kimi),
        "gemini" | "google" => Some(Runner::Gemini),
        "local" => Some(Runner::Local),
        "copilot" => Some(Runner::Copilot),
        "minimax" => Some(Runner::Minimax),
        "berget" => Some(Runner::Berget),
        _ => None,
    }
}

/// Parses a tier name into a [`Tier`]. Returns `None` for unknown tiers.
pub(crate) fn parse_tier_name(s: &str) -> Option<smedja_assayer::Tier> {
    use smedja_assayer::Tier;
    match s.trim().to_ascii_lowercase().as_str() {
        "fast" => Some(Tier::Fast),
        "local" => Some(Tier::Local),
        "deep" => Some(Tier::Deep),
        _ => None,
    }
}

/// Handles `session.set_tier`: makes `/tier` meaningful by resolving the
/// session's current runner + the requested tier to a concrete model (via the
/// provider pool) and pinning it as the session's `model_override`. So
/// `/tier deep` actually runs on the runner's deep model (and persists across
/// restarts via the model-override inheritance in `create`).
///
/// # Errors
///
/// Returns an error when `session_id`/`tier` is missing, the tier is unknown,
/// or no model is configured for the (runner, tier) pair.
pub(crate) async fn set_tier(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let pool = state.provider_pool;
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let tier_str = params["tier"]
        .as_str()
        .ok_or_else(|| missing_param("tier"))?
        .to_owned();
    let tier = parse_tier_name(&tier_str)
        .ok_or_else(|| RpcError::new(codes::INVALID_PARAMS, format!("unknown tier: {tier_str}")))?;

    // Resolve the session's effective runner (override, else the startup default).
    let runner_str = ig
        .get_session(&session_id)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.runner_override)
        .unwrap_or_else(|| state.startup_runner.to_string());
    let runner = parse_runner_name(&runner_str).ok_or_else(|| {
        RpcError::new(
            codes::INVALID_PARAMS,
            format!("unknown runner: {runner_str}"),
        )
    })?;

    // (runner, tier) → model, falling back through the eligible ring.
    let model = pool
        .get(runner, tier)
        .or_else(|| pool.eligible_ring(runner, tier).into_iter().next())
        .map(|e| e.default_model.clone())
        .ok_or_else(|| {
            RpcError::new(
                codes::INVALID_PARAMS,
                format!("no model configured for {runner_str} @ {tier_str}"),
            )
        })?;

    ig.update_session_model_override(&session_id, &model)
        .await
        .map_err(|e| ingot_err(&e))?;

    Ok(json!({
        "session_id": session_id,
        "tier": tier_str,
        "runner": runner_str,
        "model": model,
    }))
}

/// Handles `session.set_runner`.
///
/// # Errors
///
/// Returns an error when `session_id`/`runner` is missing, the runner is unknown,
/// or the ingot write fails.
pub(crate) async fn set_runner(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let runner_str = params["runner"]
        .as_str()
        .ok_or_else(|| missing_param("runner"))?
        .to_owned();
    // Validate and normalise to the canonical key stored in the DB.
    let canonical = crate::common::parse_runner_str(&runner_str)
        .map(crate::common::runner_session_key)
        .ok_or_else(|| {
            RpcError::new(
                codes::INVALID_PARAMS,
                format!("unknown runner: {runner_str}; valid: claude, codex, kimi, gemini, local, copilot, minimax, berget"),
            )
        })?;
    ig.update_session_runner_override(&session_id, canonical)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "session_id": session_id, "runner": canonical }))
}

/// Handles `session.set_title`: overwrites the session's human-readable title.
///
/// # Errors
///
/// Returns an error when `session_id`/`title` is missing or the ingot write fails.
pub(crate) async fn set_title(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let title = params["title"]
        .as_str()
        .ok_or_else(|| missing_param("title"))?
        .to_owned();
    ig.update_session_title(&session_id, &title)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "session_id": session_id, "title": title }))
}

/// Handles `session.set_mode`.
///
/// # Errors
///
/// Returns an error when `session_id`/`mode` is missing, the session is a
/// read-only review session, or the ingot write fails.
pub(crate) async fn set_mode(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let mode = params
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("mode"))?
        .to_owned();
    // Prevent escalation out of read-only review sessions.
    let existing_session = ig
        .get_session(&session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    if let Some(existing_session) = existing_session {
        if existing_session.mode.as_deref() == Some("review") {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                "review sessions are read-only",
            ));
        }
    }
    ig.update_session_mode(&session_id, &mode)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "session_id": session_id, "mode": mode }))
}

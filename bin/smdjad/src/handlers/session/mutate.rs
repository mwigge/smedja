//! Session mutation handlers: model / runner / tier / title / mode setters and
//! the runner/tier name parsers. Moved verbatim from `session.rs`.

use super::*;

/// Handles `session.set_model`.
///
/// # Errors
///
/// Returns an error when `session_id`/`model` is missing or the ingot write fails.
pub(crate) async fn set_model(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot.clone();
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let model = params["model"]
        .as_str()
        .ok_or_else(|| missing_param("model"))?
        .to_owned();
    let runner = ig
        .get_session(&session_id)
        .await
        .map_err(|e| ingot_err(&e))?
        .and_then(|s| s.runner_override)
        .unwrap_or_else(|| {
            crate::provider_pool::pool_snapshot(&state.provider_pool)
                .default_runner_name()
                .to_owned()
        });
    let catalog = query::runner_models(state, json!({"runner": runner})).await?;
    if catalog["verified"].as_bool() == Some(true)
        && !catalog["models"]
            .as_array()
            .is_some_and(|models| models.iter().any(|item| item.as_str() == Some(&model)))
    {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            format!(
                "model {model} is unavailable for {runner}; use /model to list available models"
            ),
        ));
    }
    ig.update_session_model_override(&session_id, &model)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "session_id": session_id, "model": model }))
}

/// Parses a runner name (tolerating the `-cli` suffix, e.g. `claude-cli`) into
/// a [`Runner`]. Returns `None` for unknown runners.
pub(crate) fn parse_runner_name(s: &str) -> Option<smedja_assayer::Runner> {
    use smedja_assayer::Runner;
    let lower = s.trim().to_ascii_lowercase();
    if lower == "kimi-code" {
        return Some(Runner::KimiCode);
    }
    match lower.split('-').next()? {
        "claude" | "anthropic" => Some(Runner::Claude),
        "codex" | "openai" => Some(Runner::Codex),
        "kimi" | "moonshot" => Some(Runner::Kimi),
        "gemini" | "google" => Some(Runner::Gemini),
        "local" => Some(Runner::Local),
        "copilot" => Some(Runner::Copilot),
        "minimax" => Some(Runner::Minimax),
        "berget" => Some(Runner::Berget),
        "mistral" => Some(Runner::Mistral),
        "deepseek" => Some(Runner::Deepseek),
        "ollama" | "ollama-cloud" => Some(Runner::OllamaCloud),
        "openrouter" => Some(Runner::Openrouter),
        "xai" | "grok" => Some(Runner::Xai),
        "groq" => Some(Runner::Groq),
        "cerebras" => Some(Runner::Cerebras),
        "custom" => Some(Runner::Custom),
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
    let catalog_state = state.clone();
    let ig = state.ingot;
    let pool = crate::provider_pool::pool_snapshot(&state.provider_pool);
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
        .unwrap_or_else(|| pool.default_runner_name().to_owned());
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

    if pool.get(runner, tier).is_some() {
        let catalog = query::runner_models(catalog_state, json!({"runner": runner_str})).await?;
        if catalog["verified"].as_bool() == Some(true)
            && !catalog["models"]
                .as_array()
                .is_some_and(|models| models.iter().any(|item| item.as_str() == Some(&model)))
        {
            return Err(RpcError::new(codes::INVALID_PARAMS,
                format!("{model} is unavailable for {runner_str}; choose an available model with /model")));
        }
    }

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
/// Switching runners also clears any pinned `model_override`: a model pinned
/// for the old runner (e.g. `gpt-5.5` from codex) is not a valid model id on
/// the new runner, and `run_turn` gives `model_override` precedence over the
/// new runner's own default — so a stale pin would send a foreign model id to
/// a CLI that rejects it (kimi: "issue with the selected model"). With the pin
/// cleared, turns fall back to the new runner's per-tier default model.
///
/// # Errors
///
/// Returns an error when `session_id`/`runner` is missing, the runner is unknown,
/// or the ingot write fails.
pub(crate) async fn set_runner(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let runner_str = params["runner"]
        .as_str()
        .ok_or_else(|| missing_param("runner"))?
        .to_owned();
    let mut runner = crate::common::parse_runner_str(&runner_str).ok_or_else(|| {
        RpcError::new(
            codes::INVALID_PARAMS,
            format!("unknown runner: {runner_str}"),
        )
    })?;
    if runner == smedja_assayer::Runner::KimiCode
        && crate::provider_pool::pool_snapshot(&state.provider_pool)
            .models_for_runner(runner)
            .is_empty()
        && crate::provider_pool::pool_snapshot(&state.provider_pool)
            .get(smedja_assayer::Runner::Kimi, smedja_assayer::Tier::Fast)
            .is_some_and(|entry| entry.runner_name == "kimi-cli")
    {
        runner = smedja_assayer::Runner::Kimi;
    }
    if crate::provider_pool::pool_snapshot(&state.provider_pool)
        .models_for_runner(runner)
        .is_empty()
    {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            format!("{runner_str} is not ready; use /connect to set it up"),
        ));
    }
    let selected_model = if runner == smedja_assayer::Runner::Kimi {
        let catalog = query::runner_models(state.clone(), json!({"runner": runner_str})).await?;
        if catalog["verified"].as_bool() == Some(true) {
            let models = catalog["models"].as_array().ok_or_else(|| {
                RpcError::new(codes::INVALID_PARAMS, "Moonshot returned no model catalog")
            })?;
            let selected = choose_moonshot_model(models).ok_or_else(|| {
                RpcError::new(
                    codes::INVALID_PARAMS,
                    "Moonshot account has no available models",
                )
            })?;
            Some(selected.to_owned())
        } else {
            None
        }
    } else {
        None
    };
    let mut result = set_runner_with(
        &state.ingot,
        &session_id,
        crate::common::runner_session_key(runner),
    )
    .await?;
    if let Some(model) = selected_model {
        state
            .ingot
            .update_session_model_override(&session_id, &model)
            .await
            .map_err(|e| ingot_err(&e))?;
        result["model"] = json!(model);
    }
    Ok(result)
}

fn choose_moonshot_model(models: &[Value]) -> Option<&str> {
    models
        .iter()
        .filter_map(Value::as_str)
        .find(|model| *model == "kimi-k3")
        .or_else(|| models.iter().filter_map(Value::as_str).next())
}

/// Core of `session.set_runner`, factored out so tests can exercise it
/// without constructing a full [`HandlerState`].
pub(crate) async fn set_runner_with(
    ig: &smedja_ingot::IngotHandle,
    session_id: &str,
    runner_str: &str,
) -> Result<Value, RpcError> {
    // Validate and normalise to the canonical key stored in the DB.
    let canonical = crate::common::parse_runner_str(runner_str)
        .map(crate::common::runner_session_key)
        .ok_or_else(|| {
            RpcError::new(
                codes::INVALID_PARAMS,
                format!("unknown runner: {runner_str}; valid: claude, codex, kimi, gemini, local, copilot, minimax, berget, mistral, deepseek, ollama-cloud, openrouter, xai, groq, cerebras"),
            )
        })?;
    ig.update_session_runner_override(session_id, canonical)
        .await
        .map_err(|e| ingot_err(&e))?;
    ig.clear_session_model_override(session_id)
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

#[cfg(test)]
mod moonshot_model_tests {
    use super::*;

    #[test]
    fn prefers_k3_only_when_account_catalog_has_it() {
        let models = vec![json!("kimi-k2.7"), json!("kimi-k3")];
        assert_eq!(choose_moonshot_model(&models), Some("kimi-k3"));
        let models = vec![json!("kimi-k2.7")];
        assert_eq!(choose_moonshot_model(&models), Some("kimi-k2.7"));
        assert_eq!(choose_moonshot_model(&[]), None);
    }
}

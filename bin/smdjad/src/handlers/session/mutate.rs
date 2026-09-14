//! Session mutation handlers: model / runner / tier / effort / title / mode
//! setters and the tier name parser. Moved verbatim from `session.rs`.

use super::*;
use smedja_assayer::Tier;
use smedja_types::Effort;

/// Returns the models the pool knows for `runner_str`: the local model
/// inventory for [`Runner::Local`](smedja_assayer::Runner::Local), else the
/// runner's configured per-tier default models. An empty list means the runner
/// has no known models (unparseable, not in the pool, or self-selecting), in
/// which case `session.set_model` accepts any value.
pub(crate) fn known_models_for_runner(
    pool: &crate::provider_pool::ProviderPool,
    runner_str: &str,
) -> Vec<String> {
    let Some(runner) = crate::common::parse_runner_str(runner_str) else {
        return Vec::new();
    };
    if runner == smedja_assayer::Runner::Local {
        return pool.local_control().map_or_else(Vec::new, |local| {
            local.inventory().iter().map(|m| m.id.clone()).collect()
        });
    }
    pool.models_for_runner(runner)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// The `session.set_model` acceptance policy, factored pure so tests need no
/// [`HandlerState`]: `Some(error)` when the pin must be rejected — a
/// [`Runner::Local`](smedja_assayer::Runner::Local) model missing from the
/// local inventory, which IS authoritative — `None` when accepted. CLI/API
/// runners are advisory-only: providers add models faster than the pool's
/// static config tracks them.
pub(crate) fn set_model_rejection(
    runner_str: &str,
    model: &str,
    known: &[String],
) -> Option<String> {
    if known.contains(&model.to_owned()) {
        return None;
    }
    let is_local =
        crate::common::parse_runner_str(runner_str) == Some(smedja_assayer::Runner::Local);
    if is_local && !known.is_empty() {
        return Some(format!(
            "unknown model for {runner_str}: {model}; valid: {}",
            known.join(", ")
        ));
    }
    None
}

/// Handles `session.set_model`.
///
/// The model is validated before pinning: empty/whitespace is rejected. For
/// [`Runner::Local`](smedja_assayer::Runner::Local) — where the pool's model
/// inventory is authoritative — a name outside the inventory is rejected with
/// the valid choices. For CLI/API runners the known-models list is only
/// advisory: providers add models faster than the pool's static config tracks
/// them, so an unrecognised id is accepted (and logged) rather than rejected.
///
/// # Errors
///
/// Returns an error when `session_id`/`model` is missing, the model is empty,
/// a local-runner model is not in the local inventory, or the ingot write
/// fails.
pub(crate) async fn set_model(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let model = params["model"]
        .as_str()
        .ok_or_else(|| missing_param("model"))?
        .trim()
        .to_owned();
    if model.is_empty() {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            "model must not be empty",
        ));
    }

    // Resolve the session's effective runner (override, else the CURRENT pool
    // default — provider.rescan may have replaced the pool since startup).
    let pool = state.provider_pool.snapshot();
    let runner_str = ig
        .get_session(&session_id)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.runner_override)
        .unwrap_or_else(|| pool.default_runner_name().to_string());
    let known = known_models_for_runner(&pool, &runner_str);
    if let Some(msg) = set_model_rejection(&runner_str, &model, &known) {
        return Err(RpcError::new(codes::INVALID_PARAMS, msg));
    }
    if !known.is_empty() && !known.contains(&model) {
        tracing::info!(
            runner = %runner_str,
            model = %model,
            "session.set_model: model not in the pool's known list; accepting (advisory)"
        );
    }

    ig.update_session_model_override(&session_id, &model)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "session_id": session_id, "model": model }))
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
    let pool = state.provider_pool.snapshot();
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

    // Resolve the session's effective runner (override, else the CURRENT pool
    // default — provider.rescan may have replaced the pool since startup).
    let runner_str = ig
        .get_session(&session_id)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.runner_override)
        .unwrap_or_else(|| pool.default_runner_name().to_string());
    let runner = crate::common::parse_runner_str(&runner_str).ok_or_else(|| {
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

/// Parses an effort level name into an [`Effort`]. Returns `None` for unknown
/// levels. Accepts surrounding whitespace and any case, like
/// [`parse_tier_name`].
pub(crate) fn parse_effort_name(s: &str) -> Option<Effort> {
    s.parse().ok()
}

/// Per-session reasoning-effort overrides, keyed by session id.
///
/// In-memory on purpose: the ingot session schema is owned by `smedja-ingot`,
/// and unlike a model pin (a backend-specific id that goes stale across
/// runner switches) an effort level is a portable hint — a daemon restart
/// simply falls back to the provider default. Same construction pattern as
/// `executor::output_filter`'s store.
fn effort_overrides() -> &'static std::sync::Mutex<std::collections::HashMap<String, Effort>> {
    static STORE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Effort>>> =
        std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Returns the session's pinned effort override, if any (`None` = provider
/// default). Read by `session.get` and by the turn orchestrator when it
/// derives `CallOptions`.
pub(crate) fn session_effort(session_id: &str) -> Option<Effort> {
    effort_overrides()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(session_id)
        .copied()
}

/// Core of `session.set_effort`, factored out so tests can exercise it without
/// constructing a full [`HandlerState`]. `"default"` clears the pin.
pub(crate) fn set_effort_with(session_id: &str, effort_str: &str) -> Result<Value, RpcError> {
    if effort_str.trim().eq_ignore_ascii_case("default") {
        effort_overrides()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        return Ok(json!({ "session_id": session_id, "effort": Value::Null }));
    }
    let effort = parse_effort_name(effort_str).ok_or_else(|| {
        RpcError::new(
            codes::INVALID_PARAMS,
            format!("unknown effort: {effort_str}; valid: low, medium, high, default"),
        )
    })?;
    let mut store = effort_overrides()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    insert_capped(&mut store, session_id, effort)?;
    Ok(json!({ "session_id": session_id, "effort": effort.as_str() }))
}

/// Upper bound on pinned effort overrides. The map is unbounded per session id
/// otherwise, so a client minting fresh ids could grow it without limit.
const MAX_EFFORT_OVERRIDES: usize = 1024;

/// Inserts one override, refusing new entries past [`MAX_EFFORT_OVERRIDES`]
/// (re-pinning an existing session always succeeds). Pure so tests exercise
/// the bound without touching the process-global store.
pub(crate) fn insert_capped(
    map: &mut std::collections::HashMap<String, Effort>,
    session_id: &str,
    effort: Effort,
) -> Result<(), RpcError> {
    if !map.contains_key(session_id) && map.len() >= MAX_EFFORT_OVERRIDES {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            format!("too many effort overrides (max {MAX_EFFORT_OVERRIDES})"),
        ));
    }
    map.insert(session_id.to_owned(), effort);
    Ok(())
}

/// Drops the effort override for a deleted session.
pub(crate) fn clear_effort_override(session_id: &str) {
    effort_overrides()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_id);
}

/// Drops every override whose session id is not in `valid` — the daily
/// maintenance sweep keeps the map from accumulating pins for pruned sessions.
pub(crate) fn prune_effort_overrides(valid: &std::collections::HashSet<String>) {
    effort_overrides()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|id, _| valid.contains(id));
}

/// Handles `session.set_effort`: pins a per-session reasoning-effort level
/// (`low`/`medium`/`high`), threaded into `CallOptions` on the next turn. Each
/// adapter maps the level to its own mechanism (codex
/// `-c model_reasoning_effort=…`, claude `MAX_THINKING_TOKENS`, ACP
/// `session/set_config_option`, `OpenAI`-compatible `reasoning_effort`);
/// backends without a knob ignore it. `"default"` clears the pin.
///
/// The pin is kept across `/switch` runner changes: the level names are
/// portable across backends (like tier names), unlike a pinned model id.
///
/// # Errors
///
/// Returns an error when `session_id`/`effort` is missing, the session does
/// not exist, or the level is unknown.
pub(crate) async fn set_effort(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let effort = params["effort"]
        .as_str()
        .ok_or_else(|| missing_param("effort"))?
        .to_owned();
    // Reject pins for sessions that do not exist — the override map is
    // in-memory and unvalidated ids would linger until the daily sweep.
    ensure_session_exists(&state.ingot, &session_id).await?;
    set_effort_with(&session_id, &effort)
}

/// Errors when `session_id` is not a session the ingot knows about.
pub(crate) async fn ensure_session_exists(
    ingot: &smedja_ingot::IngotHandle,
    session_id: &str,
) -> Result<(), RpcError> {
    let exists = ingot
        .get_session(session_id)
        .await
        .map_err(|e| crate::ingot_err(&e))?
        .is_some();
    if exists {
        return Ok(());
    }
    Err(RpcError::new(
        codes::INVALID_PARAMS,
        format!("unknown session: {session_id}"),
    ))
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
    let mut resp = set_runner_with(&state.ingot, &session_id, &runner_str).await?;

    // Surface whether the switch cleared a model pin (set_runner_with reports
    // it) — and the default model the session now falls back to — so the
    // client can tell the user instead of silently unpinning.
    let pin_cleared = resp["model_pin_cleared"].as_bool().unwrap_or(false);
    let pool = state.provider_pool.snapshot();
    let default_model = resp["runner"]
        .as_str()
        .and_then(crate::common::parse_runner_str)
        .and_then(|r| {
            pool.get_exact(r, Tier::Fast)
                .or_else(|| pool.get_exact(r, Tier::Deep))
                .or_else(|| pool.get_exact(r, Tier::Local))
        })
        .map(|e| e.default_model.clone())
        .filter(|m| !m.is_empty());
    match (pin_cleared, default_model) {
        (true, Some(m)) => {
            resp["note"] = json!(format!(
                "model pin cleared; turns now use the default model {m}"
            ));
            resp["default_model"] = json!(m);
        }
        (true, None) => {
            resp["note"] = json!(
                "model pin cleared; runner is not in the provider pool, so turns fall back to the pool default"
            );
        }
        (false, Some(m)) => {
            resp["note"] = json!(format!("turns use the default model {m}"));
            resp["default_model"] = json!(m);
        }
        (false, None) => {}
    }
    Ok(resp)
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
                format!(
                    "unknown runner: {runner_str}; valid: {}",
                    crate::common::valid_runner_names()
                ),
            )
        })?;
    // Report whether a model pin actually existed so clients don't claim one
    // was cleared when the session never had one.
    let had_pin = ig
        .get_session(session_id)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.model_override)
        .is_some();
    ig.update_session_runner_override(session_id, canonical)
        .await
        .map_err(|e| ingot_err(&e))?;
    ig.clear_session_model_override(session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "session_id": session_id, "runner": canonical, "model_pin_cleared": had_pin }))
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

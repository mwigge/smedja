//! Session query handlers: list, search, get, token usage, runner list,
//! context, and history. Moved verbatim from `session.rs`.

use super::*;

/// Handles `session.list`.
///
/// # Errors
///
/// Returns an error when the ingot query fails.
pub(crate) async fn list(state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    list_with(&state.ingot).await
}

/// Core of `session.list`, parameterised on the ingot handle so it is testable
/// without constructing a full [`HandlerState`].
pub(crate) async fn list_with(ig: &smedja_ingot::IngotHandle) -> Result<Value, RpcError> {
    let sessions = ig.list_sessions().await.map_err(|e| ingot_err(&e))?;
    let start = sessions.len().saturating_sub(10);
    let out: Vec<Value> = sessions[start..]
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "title": s.title,
                "mode": s.mode,
                "runner": s.runner_override,
                "created_at": s.created_at,
                "updated_at": s.updated_at,
            })
        })
        .collect();
    Ok(Value::Array(out))
}

/// Handles `session.search`: returns sessions whose title or workspace_root matches `query`.
///
/// # Errors
///
/// Returns an error when `query` is missing or the ingot read fails.
pub(crate) async fn search(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let query = params["query"]
        .as_str()
        .ok_or_else(|| missing_param("query"))?
        .to_owned();
    search_with(&state.ingot, &query).await
}

pub(crate) async fn search_with(
    ig: &smedja_ingot::IngotHandle,
    query: &str,
) -> Result<Value, RpcError> {
    let sessions = ig.search_sessions(query).await.map_err(|e| ingot_err(&e))?;
    let out: Vec<Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "title": s.title,
                "mode": s.mode,
                "workspace_root": s.workspace_root,
                "created_at": s.created_at,
                "updated_at": s.updated_at,
            })
        })
        .collect();
    Ok(Value::Array(out))
}

/// Handles `session.get`.
///
/// # Errors
///
/// Returns an error when `id` is missing or the session does not exist.
pub(crate) async fn get(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("id"))?;

    let session = ig
        .get_session(id)
        .await
        .map_err(|e| ingot_err(&e))?
        .ok_or_else(|| RpcError::new(codes::INTERNAL_ERROR, format!("session not found: {id}")))?;

    Ok(json!({
        "id": session.id,
        "title": session.title,
        "mode": session.mode,
        "runner": session.runner_override,
        "created_at": session.created_at,
        "updated_at": session.updated_at,
        "status": session.status,
        "task_id": session.task_id,
        "cowork_mode": session.cowork_mode,
        "active_change": state.active_change.as_deref(),
    }))
}

/// Handles `session.token_usage`.
///
/// # Errors
///
/// Returns an error when `session_id` is missing or the ingot query fails.
pub(crate) async fn token_usage(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?;
    let snaps = ig
        .session_token_snapshots(session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let rows: Vec<Value> = snaps
        .iter()
        .map(|s| {
            json!({
                "turn_n": s.turn_n,
                "input_tok": s.input_tok,
                "output_tok": s.output_tok,
                "cumulative_input": s.cumulative_input,
                "cumulative_output": s.cumulative_output,
            })
        })
        .collect();
    Ok(json!({ "session_id": session_id, "turns": rows }))
}

/// Handles `runner.list`.
///
/// # Errors
///
/// Infallible in practice; the signature matches the handler contract.
#[allow(clippy::unused_async)] // uniform handler signature: all handlers are async fns
pub(crate) async fn runner_list(state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    let pool = crate::provider_pool::pool_snapshot(&state.provider_pool);
    let runners: Vec<Value> = pool
        .list_all_entries()
        .into_iter()
        .map(|(runner, tier, model)| json!({ "runner": runner, "tier": tier, "model": model }))
        .collect();
    Ok(json!({ "runners": runners }))
}

/// Lists models from the authenticated provider when it exposes a standard
/// catalog. CLI-backed runners fall back to their configured tier models.
pub(crate) async fn runner_models(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let runner = params["runner"]
        .as_str()
        .ok_or_else(|| missing_param("runner"))?;
    let canonical = mutate::parse_runner_name(runner)
        .ok_or_else(|| RpcError::new(codes::INVALID_PARAMS, format!("unknown runner: {runner}")))?;
    let configured: Vec<String> = crate::provider_pool::pool_snapshot(&state.provider_pool)
        .models_for_runner(canonical)
        .into_iter()
        .map(str::to_owned)
        .collect();
    if configured.is_empty() {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            format!("runner {runner} is not ready"),
        ));
    }
    let remote = match canonical {
        smedja_assayer::Runner::Claude => Some((
            "https://api.anthropic.com/v1/models".to_owned(),
            "ANTHROPIC_API_KEY",
            true,
        )),
        smedja_assayer::Runner::Codex => Some((
            "https://api.openai.com/v1/models".to_owned(),
            "OPENAI_API_KEY",
            false,
        )),
        smedja_assayer::Runner::Kimi => Some((
            format!(
                "{}/v1/models",
                std::env::var("MOONSHOT_BASE_URL")
                    .unwrap_or_else(|_| "https://api.moonshot.ai".to_owned())
                    .trim_end_matches('/')
                    .trim_end_matches("/v1")
            ),
            "MOONSHOT_API_KEY",
            false,
        )),
        smedja_assayer::Runner::Mistral => Some((
            "https://api.mistral.ai/v1/models".to_owned(),
            "MISTRAL_API_KEY",
            false,
        )),
        smedja_assayer::Runner::Deepseek => Some((
            "https://api.deepseek.com/models".to_owned(),
            "DEEPSEEK_API_KEY",
            false,
        )),
        smedja_assayer::Runner::OllamaCloud => Some((
            "https://ollama.com/v1/models".to_owned(),
            "OLLAMA_API_KEY",
            false,
        )),
        smedja_assayer::Runner::Openrouter => Some((
            "https://openrouter.ai/api/v1/models".to_owned(),
            "OPENROUTER_API_KEY",
            false,
        )),
        smedja_assayer::Runner::Xai => Some((
            "https://api.x.ai/v1/models".to_owned(),
            "XAI_API_KEY",
            false,
        )),
        smedja_assayer::Runner::Groq => Some((
            "https://api.groq.com/openai/v1/models".to_owned(),
            "GROQ_API_KEY",
            false,
        )),
        smedja_assayer::Runner::Cerebras => Some((
            "https://api.cerebras.ai/v1/models".to_owned(),
            "CEREBRAS_API_KEY",
            false,
        )),
        smedja_assayer::Runner::Custom => std::env::var("SMEDJA_COMPAT_BASE_URL").ok().map(|url| {
            (
                format!(
                    "{}/v1/models",
                    url.trim_end_matches('/').trim_end_matches("/v1")
                ),
                "SMEDJA_COMPAT_API_KEY",
                false,
            )
        }),
        _ => None,
    };
    if let Some((url, env_var, anthropic)) = remote {
        if let Some(key) = saved_provider_keys()?
            .get(env_var)
            .cloned()
            .or_else(|| std::env::var(env_var).ok())
        {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
            let request = if anthropic {
                client
                    .get(url)
                    .header("x-api-key", key)
                    .header("anthropic-version", "2023-06-01")
            } else {
                client.get(url).bearer_auth(key)
            };
            let response = request.send().await.map_err(|e| {
                RpcError::new(
                    codes::INTERNAL_ERROR,
                    format!("model catalog unavailable: {e}"),
                )
            })?;
            if !response.status().is_success() {
                return Err(RpcError::new(
                    codes::INVALID_PARAMS,
                    format!("provider model catalog returned HTTP {}", response.status()),
                ));
            }
            let value: Value = response.json().await.map_err(|e| {
                RpcError::new(codes::INTERNAL_ERROR, format!("invalid model catalog: {e}"))
            })?;
            let models: Vec<String> = value["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                .collect();
            let authenticated_catalog = !matches!(
                canonical,
                smedja_assayer::Runner::OllamaCloud | smedja_assayer::Runner::Openrouter
            );
            return Ok(
                json!({"runner": runner, "models": models, "verified": authenticated_catalog}),
            );
        }
    }
    Ok(json!({"runner": runner, "models": configured, "verified": false}))
}

/// Rebuilds provider instances from the saved keys and atomically switches
/// future requests to the new pool. Running turns retain their old snapshot.
pub(crate) async fn reload_providers(
    state: HandlerState,
    _params: Value,
) -> Result<Value, RpcError> {
    let keys = saved_provider_keys()?;
    let replacement =
        std::sync::Arc::new(crate::provider_pool::build_provider_pool_with_keys(&keys).await);
    let runners: Vec<Value> = replacement
        .list_all_entries()
        .into_iter()
        .map(|(runner, tier, model)| json!({"runner": runner, "tier": tier, "model": model}))
        .collect();
    *state
        .provider_pool
        .write()
        .map_err(|_| RpcError::new(codes::INTERNAL_ERROR, "provider pool lock poisoned"))? =
        replacement;
    Ok(json!({"runners": runners}))
}

fn saved_provider_keys() -> Result<std::collections::HashMap<String, String>, RpcError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| RpcError::new(codes::INTERNAL_ERROR, "HOME is not set"))?;
    let path = std::path::PathBuf::from(home).join(".config/smedja/secrets.env");
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(RpcError::new(
                codes::INTERNAL_ERROR,
                format!("cannot read saved credentials: {e}"),
            ));
        }
    };
    let keys = contents
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(name, value)| name.ends_with("_API_KEY") && !value.is_empty())
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    Ok(keys)
}

/// Checks a newly entered API key before it is saved. The key is used only for
/// this outbound request and is never included in a response or error string.
pub(crate) async fn check_provider_key(
    _state: HandlerState,
    params: Value,
) -> Result<Value, RpcError> {
    let provider = params["provider"]
        .as_str()
        .ok_or_else(|| missing_param("provider"))?;
    let key = params["key"].as_str().ok_or_else(|| missing_param("key"))?;
    if key.trim().is_empty() {
        return Err(RpcError::new(codes::INVALID_PARAMS, "empty API key"));
    }
    let moonshot_url = format!(
        "{}/v1/models",
        std::env::var("MOONSHOT_BASE_URL")
            .unwrap_or_else(|_| "https://api.moonshot.ai".to_owned())
            .trim_end_matches('/')
            .trim_end_matches("/v1")
    );
    let (url, header) = match provider {
        "ANTHROPIC_API_KEY" => ("https://api.anthropic.com/v1/models", true),
        "OPENAI_API_KEY" => ("https://api.openai.com/v1/models", false),
        "MOONSHOT_API_KEY" => (moonshot_url.as_str(), false),
        "MISTRAL_API_KEY" => ("https://api.mistral.ai/v1/models", false),
        "DEEPSEEK_API_KEY" => ("https://api.deepseek.com/models", false),
        "OPENROUTER_API_KEY" => ("https://openrouter.ai/api/v1/key", false),
        "XAI_API_KEY" => ("https://api.x.ai/v1/models", false),
        "GROQ_API_KEY" => ("https://api.groq.com/openai/v1/models", false),
        "CEREBRAS_API_KEY" => ("https://api.cerebras.ai/v1/models", false),
        "SMEDJA_COMPAT_API_KEY" => {
            let url = std::env::var("SMEDJA_COMPAT_BASE_URL").map_err(|_| {
                RpcError::new(
                    codes::INVALID_PARAMS,
                    "set SMEDJA_COMPAT_BASE_URL before connecting",
                )
            })?;
            return check_custom_key(key, &url).await;
        }
        // These providers do not offer a documented authenticated read-only
        // key check. Do not claim their keys have been verified.
        "OLLAMA_API_KEY" | "MINIMAX_API_KEY" | "BERGET_API_KEY" | "GEMINI_API_KEY" => {
            return Ok(json!({"verified": false}))
        }
        _ => {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                "unsupported provider key",
            ))
        }
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|_| RpcError::new(codes::INTERNAL_ERROR, "cannot create HTTP client"))?;
    let request = if header {
        client
            .get(url)
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
    } else {
        client.get(url).bearer_auth(key)
    };
    let response = request
        .send()
        .await
        .map_err(|_| RpcError::new(codes::INTERNAL_ERROR, "provider unreachable; key not saved"))?;
    if !response.status().is_success() {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            format!(
                "provider rejected key or catalog request (HTTP {})",
                response.status()
            ),
        ));
    }
    let body: Value = response.json().await.unwrap_or(Value::Null);
    let models: Vec<&str> = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["id"].as_str())
        .collect();
    Ok(json!({"verified": true, "models": models}))
}

async fn check_custom_key(key: &str, base_url: &str) -> Result<Value, RpcError> {
    let root = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let url = reqwest::Url::parse(&format!("{root}/v1/models"))
        .map_err(|_| RpcError::new(codes::INVALID_PARAMS, "invalid custom provider URL"))?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && local) {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            "custom provider URL must use HTTPS (HTTP is allowed for localhost)",
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
    let response = client.get(url).bearer_auth(key).send().await.map_err(|_| {
        RpcError::new(
            codes::INTERNAL_ERROR,
            "custom provider unreachable; key not saved",
        )
    })?;
    if !response.status().is_success() {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            format!(
                "custom provider rejected key or model request (HTTP {})",
                response.status()
            ),
        ));
    }
    let body: Value = response.json().await.map_err(|_| {
        RpcError::new(
            codes::INVALID_PARAMS,
            "custom provider returned invalid model catalog",
        )
    })?;
    let models: Vec<&str> = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["id"].as_str())
        .collect();
    if let Ok(configured) = std::env::var("SMEDJA_COMPAT_MODEL") {
        if !models.contains(&configured.as_str()) {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                "configured custom model is absent from this account's catalog",
            ));
        }
    }
    Ok(json!({"verified": true, "models": models}))
}

/// Handles `session.context`: token-window usage plus vault warm/cold counts.
///
/// # Errors
///
/// Returns an error when `session_id` is missing or an ingot query fails.
pub(crate) async fn context(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let pt = state.price_table;
    let vt = state.vault;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?;
    let snaps = ig
        .session_token_snapshots(session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let (cumulative_input, cumulative_output) = snaps
        .last()
        .map_or((0i64, 0i64), |s| (s.cumulative_input, s.cumulative_output));
    let used_tok = cumulative_input.saturating_add(cumulative_output);
    let model = ig
        .session_last_model(session_id)
        .await
        .map_err(|e| ingot_err(&e))?
        .unwrap_or_default();
    let window_tok = u64::from(pt.context_window(&model));
    let vt = Arc::clone(&vt);
    let (vault_warm_count, vault_cold_count) = tokio::task::spawn_blocking(move || {
        let guard = vt.blocking_lock();
        let warm = guard.count_by_namespace("warm").unwrap_or(0);
        let cold = guard.count_by_namespace("default").unwrap_or(0);
        (warm, cold)
    })
    .await
    .unwrap_or((0, 0));
    Ok(json!({
        "session_id": session_id,
        "used_tok": used_tok,
        "window_tok": window_tok,
        "model": model,
        "vault_warm_count": vault_warm_count,
        "vault_cold_count": vault_cold_count,
    }))
}

/// Handles `session.history`: returns the ordered turn/message records for a
/// session, sourced from the ingot's checkpoint blobs and audit trail.
///
/// Params: `{ session_id: string }`.
/// Response: `{ session_id, turns: [ { turn_n, created_at, messages } ], audit: [ … ] }`
/// where `turns` is ordered by `turn_n` ascending (each carries the conversation
/// snapshot for that turn) and `audit` is the ordered tool/turn audit trail.
///
/// # Errors
///
/// Returns an error when `session_id` is missing or an ingot read fails.
pub(crate) async fn history(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();

    let checkpoints = ig
        .list_checkpoints(&session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let turns: Vec<Value> = checkpoints
        .iter()
        .map(|cp| {
            // The messages blob is stored as a JSON array; surface it parsed so
            // callers receive structured records rather than an escaped string.
            let messages: Value =
                serde_json::from_str(&cp.messages_json).unwrap_or(Value::Array(Vec::new()));
            json!({
                "turn_n": cp.turn_n,
                "created_at": cp.created_at,
                "messages": messages,
            })
        })
        .collect();

    let audit = ig
        .list_audit_events(&session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let audit_json: Vec<Value> = audit
        .into_iter()
        .map(|ev| serde_json::to_value(&ev).unwrap_or(Value::Null))
        .collect();

    Ok(json!({
        "session_id": session_id,
        "turns": turns,
        "audit": audit_json,
    }))
}

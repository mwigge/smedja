//! Provider-pool runtime handlers: `provider.rescan`.

use serde_json::{json, Value};
use smedja_rpc::{codes, RpcError};

use crate::handlers::HandlerState;

/// Handles `provider.rescan`: re-reads `~/.config/smedja/secrets.env` (so API
/// keys pasted via the TUI `/login` flow take effect without a restart),
/// re-probes every provider, and atomically swaps the rebuilt pool in.
///
/// Safety with respect to in-flight sessions: consumers hold `Arc` snapshots
/// of the pool taken at the start of each turn/RPC, so a swap never drops a
/// provider out from under a running turn — the old pool stays alive until the
/// last snapshot is dropped. A session pinned to a runner that vanished from
/// the new pool degrades to the rotation ring / pool default on its next turn
/// (the same fallback an unconfigured runner gets at startup).
///
/// # Errors
///
/// Returns an error when another rescan is already in progress (concurrent
/// rescans would race a double pool rebuild). Otherwise infallible in practice;
/// the signature matches the handler contract. An empty re-probe is reported
/// via `empty: true` rather than an error so the client can warn without
/// treating the daemon as broken.
pub(crate) async fn rescan(state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    let _permit = try_rescan_permit(&state.rescan_lock)?;
    crate::load_secrets_env();
    let new_pool = crate::provider_pool::build_provider_pool().await;
    new_pool.warn_missing_prices(&state.price_table);

    let runners: Vec<Value> = new_pool
        .list_all_entries()
        .into_iter()
        .map(|(runner, tier, model)| json!({ "runner": runner, "tier": tier, "model": model }))
        .collect();
    let resp = json!({
        "runners": runners,
        "runner_names": new_pool.available_runners(),
        "default_runner": new_pool.default_runner_name(),
        "default_model": new_pool.default_model(),
        "empty": new_pool.is_empty(),
    });
    state.provider_pool.replace(new_pool);
    Ok(resp)
}

/// Acquires the rescan mutex, or errors when another rescan holds it.
fn try_rescan_permit(
    lock: &std::sync::Arc<tokio::sync::Mutex<()>>,
) -> Result<tokio::sync::OwnedMutexGuard<()>, RpcError> {
    lock.clone()
        .try_lock_owned()
        .map_err(|_| RpcError::new(codes::INTERNAL_ERROR, "provider rescan already in progress"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn rescan_permit_rejects_concurrent_rescan() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let first = try_rescan_permit(&lock).expect("first rescan takes the lock");
        let second = try_rescan_permit(&lock);
        assert!(
            second.is_err_and(|e| e.message.contains("already in progress")),
            "a concurrent rescan must be rejected"
        );
        drop(first);
        assert!(
            try_rescan_permit(&lock).is_ok(),
            "the lock frees once the in-flight rescan finishes"
        );
    }
}

//! Command-output filtering and large-response offload for the executor.
//!
//! Owns the return-path compression applied to `bash`/`run_command` output:
//! JSON routes through `SmartCrusher`, text through the command filter registry.
//! Full uncompressed output is teed to the vault recovery namespace and an
//! in-memory hash-addressed store so the `smedja_retrieve` tool can expand it.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use smedja_ingot::{IngotHandle, Session};
use smedja_vault::{Vault, VaultEntry};
use tokio::sync::Mutex;
use uuid::Uuid;

/// In-memory store for content blocks addressed by SHA-256 hash.
/// Used by the `smedja_retrieve` tool to look up compressed context blocks.
pub(crate) fn retrieve_store() -> &'static tokio::sync::Mutex<HashMap<String, String>> {
    static STORE: OnceLock<tokio::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    STORE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Insertion-order tracker for the retrieve store LRU eviction.
fn retrieve_store_order() -> &'static tokio::sync::Mutex<std::collections::VecDeque<String>> {
    static ORDER: OnceLock<tokio::sync::Mutex<std::collections::VecDeque<String>>> =
        OnceLock::new();
    ORDER.get_or_init(|| tokio::sync::Mutex::new(std::collections::VecDeque::new()))
}

/// Vault namespace under which full uncompressed command output is teed for
/// recovery via the `smedja_retrieve` tool.
pub(crate) const FILTER_RECOVERY_NAMESPACE: &str = "filter-recovery";

/// Byte length above which a tool response is offloaded to a temp file rather
/// than injected verbatim into the agent context. Keeps large reads/fetches
/// from saturating the context window.
pub(crate) const LARGE_RESPONSE_THRESHOLD: usize = 100_000;

/// Writes a large tool response to a temp file and returns a compact agent
/// reference. Returns `None` when the write fails; caller falls through to
/// verbatim return.
fn offload_large_response(result: &str, cmd: &str) -> Option<String> {
    let dir = std::env::temp_dir().join("smedja-tool-responses");
    std::fs::create_dir_all(&dir).ok()?;
    let hash = content_hash(result);
    let path = dir.join(&hash);
    std::fs::write(&path, result.as_bytes()).ok()?;
    tracing::debug!(cmd, bytes = result.len(), path = %path.display(), "large response offloaded");
    Some(format!(
        "[large output: {} bytes — use read_file(\"{}\") to retrieve]\n[smedja_retrieve hash={hash} to expand]",
        result.len(),
        path.display(),
    ))
}

/// Computes the lowercase hex SHA-256 content hash used to address a teed
/// full-output recovery entry.
pub(crate) fn content_hash(content: &str) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// Applies command-aware text filtering to a `bash`/`run_command` result on the
/// return path, in-process (no shell hooks, no subprocess).
///
/// JSON results route to [`smedja_adapter::compress_tool_result`] (`SmartCrusher`,
/// unchanged); text routes through the command-keyed filter registry loaded from
/// `.smedja/filters.toml`. When filtering reduces the output (ratio < 1.0) the
/// full uncompressed text is teed to the vault recovery namespace and registered
/// in the `smedja_retrieve` store under its content hash, a trailing recovery
/// marker naming that hash is appended, and the estimated tokens saved are
/// recorded on the tokens-saved ledger. The captured success/failure contract
/// of the result is never altered — only its body text is compressed.
///
/// `SMEDJA_NO_TOOL_COMPRESS=1` is honoured by the underlying compressors and
/// returns the result verbatim.
pub(crate) async fn filter_command_output(
    cmd: &str,
    result: String,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
) -> String {
    // Single branch point: JSON routes through SmartCrusher; text routes through
    // the command filter. The bash/run_command path is text by construction.
    if serde_json::from_str::<Value>(&result).is_ok() {
        let compressed = smedja_adapter::compress_tool_result(&result);
        // Attribute the SmartCrusher saving to its own source so it is not folded
        // into the filter total. The estimate is recorded on this JSON path only.
        record_tokens_saved(cmd, &result, &compressed, "crusher", session, ingot).await;
        if compressed.len() > LARGE_RESPONSE_THRESHOLD {
            if let Some(offloaded) = offload_large_response(&compressed, cmd) {
                return offloaded;
            }
        }
        return compressed;
    }

    let registry = crate::filters::load_filter_registry(workspace);
    let (compressed, ratio) = smedja_adapter::compress_command_output_with(&registry, cmd, &result);

    // No reduction → offload if large, otherwise return verbatim.
    if ratio >= 1.0 {
        if result.len() > LARGE_RESPONSE_THRESHOLD {
            if let Some(offloaded) = offload_large_response(&result, cmd) {
                return offloaded;
            }
        }
        return result;
    }

    // Tee the full uncompressed output to the vault recovery namespace and the
    // in-memory retrieve store, addressed by content hash.
    let hash = content_hash(&result);
    {
        let mut store = retrieve_store().lock().await;
        let mut order = retrieve_store_order().lock().await;
        store.insert(hash.clone(), result.clone());
        order.push_back(hash.clone());
        if store.len() > 512 {
            if let Some(oldest) = order.pop_front() {
                store.remove(&oldest);
            }
        }
    }
    tee_to_vault(&hash, &result, vault).await;

    // Record tokens saved (clamped ≥ 0), separate from billed cost.
    record_tokens_saved(cmd, &result, &compressed, "filter", session, ingot).await;

    // Append the recovery marker naming the hash so the agent can expand it.
    format!("{compressed}\n[smedja_retrieve hash={hash} to expand full output]")
}

/// Tees `full_output` to the vault recovery namespace under `hash`.
///
/// Vault writes are synchronous `SQLite` work; they run on a blocking thread so
/// the async runtime is never blocked. A vault error is logged and swallowed —
/// recovery is best-effort and must never break the tool path.
async fn tee_to_vault(hash: &str, full_output: &str, vault: &Arc<Mutex<Vault>>) {
    let vault = Arc::clone(vault);
    let entry = VaultEntry {
        id: hash.to_owned(),
        embedding: Vec::new(),
        payload: serde_json::json!({ "kind": "filter-recovery" }),
        namespace: FILTER_RECOVERY_NAMESPACE.to_owned(),
        content: full_output.to_owned(),
        source_file: None,
        added_by: Some("output-filter".to_owned()),
        chunk_index: None,
        parent_id: None,
        created_at: 0.0,
        // Recovery rows hold the raw output for hash retrieval, never a semantic
        // embedding, so they carry an empty vector tagged dim 0.
        embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
        dim: 0,
    };
    let join = tokio::task::spawn_blocking(move || {
        let mut guard = vault.blocking_lock();
        guard.upsert(&entry)
    })
    .await;
    match join {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "filter recovery vault tee failed; continuing"),
        Err(e) => tracing::warn!(error = %e, "filter recovery vault tee task panicked; continuing"),
    }
}

/// Records the tokens saved by filtering on the tokens-saved ledger.
///
/// `saved = estimate_tokens(original) - estimate_tokens(compressed)`, clamped at
/// 0. Recorded only when positive, keyed by session (turn `0` at the executor
/// layer, which does not thread a turn index). A ledger error is logged and
/// swallowed — accounting is advisory and must never break the tool path.
async fn record_tokens_saved(
    cmd: &str,
    original: &str,
    compressed: &str,
    source: &str,
    session: Option<&Session>,
    ingot: &IngotHandle,
) {
    let before = smedja_memory::estimate_tokens(original);
    let after = smedja_memory::estimate_tokens(compressed);
    let saved = before.saturating_sub(after);
    if saved == 0 {
        return;
    }
    let Some(session) = session else {
        return;
    };
    let entry = smedja_ingot::TokensSavedEntry {
        id: Uuid::new_v4(),
        session_id: session.id.to_string(),
        turn_n: 0,
        command: cmd.to_owned(),
        tokens_saved: i64::try_from(saved).unwrap_or(i64::MAX),
        source: source.to_owned(),
        created_at: smedja_types::Timestamp::from_micros(0),
    };
    if let Err(e) = ingot.insert_tokens_saved(entry).await {
        tracing::warn!(error = %e, "failed to record tokens-saved; continuing");
    }
}

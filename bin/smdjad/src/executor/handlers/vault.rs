//! Vault tool bodies: `smedja_vault_search`, `smedja_vault_store`,
//! `smedja_retrieve`.
//!
//! These arms have no early-return guards, so they return `String` directly.

use std::sync::Arc;

use serde_json::Value;
use smedja_vault::{Vault, VaultEntry};
use tokio::sync::Mutex;
use uuid::Uuid;

/// `smedja_vault_search` tool body.
pub(crate) async fn vault_search(
    input: &Value,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn crate::embedder_port::Embedder>,
) -> String {
    let query_text = input
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let k = usize::try_from(input.get("k").and_then(Value::as_u64).unwrap_or(5)).unwrap_or(5);
    let ns = input
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();
    let vault = Arc::clone(vault);
    let query_vec = embedder.embed_query(&query_text).await;
    let model_id = embedder.model_id().to_owned();
    let dim = embedder.dim();
    tokio::task::spawn_blocking(move || {
        let guard = vault.blocking_lock();
        match guard.search(&query_vec, &query_text, &ns, k, &model_id, dim) {
            Ok(entries) => {
                let results: Vec<serde_json::Value> = entries
                    .into_iter()
                    .map(|e| {
                        serde_json::json!({
                            "id": e.id,
                            "content": e.content,
                            "namespace": e.namespace,
                            "payload": e.payload,
                        })
                    })
                    .collect();
                serde_json::json!({ "results": results }).to_string()
            }
            Err(e) => format!("error: vault search failed: {e}"),
        }
    })
    .await
    .unwrap_or_else(|e| format!("error: vault search task panicked: {e}"))
}

/// `smedja_vault_store` tool body.
pub(crate) async fn vault_store(
    input: &Value,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn crate::embedder_port::Embedder>,
) -> String {
    let content = input
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let ns = input
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();
    let entry_id = input
        .get("id")
        .and_then(Value::as_str)
        .map_or_else(|| Uuid::new_v4().to_string(), ToOwned::to_owned);
    let payload = input
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let source_file = input
        .get("source_file")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let added_by = input
        .get("added_by")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let vault = Arc::clone(vault);
    let embedding = embedder.embed_query(&content).await;
    let model_id = embedder.model_id().to_owned();
    let dim = embedder.dim();
    tokio::task::spawn_blocking(move || {
        let entry = VaultEntry {
            id: entry_id,
            embedding,
            payload,
            namespace: ns,
            content,
            source_file,
            added_by,
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
            embedder_model_id: model_id,
            dim,
        };
        let mut guard = vault.blocking_lock();
        match guard.upsert(&entry) {
            Ok(()) => serde_json::json!({ "id": entry.id, "stored": true }).to_string(),
            Err(e) => format!("error: vault store failed: {e}"),
        }
    })
    .await
    .unwrap_or_else(|e| format!("error: vault store task panicked: {e}"))
}

/// `smedja_retrieve` tool body: looks up a hash-addressed recovery block.
pub(crate) async fn retrieve(input: &Value) -> String {
    let hash = input.get("hash").and_then(Value::as_str).unwrap_or("");
    let store = super::super::retrieve_store().lock().await;
    if let Some(content) = store.get(hash) {
        // ponytail: audit deferred; log the retrieval.
        tracing::info!(hash, "smedja_retrieve hit");
        content.clone()
    } else {
        tracing::debug!(hash, "smedja_retrieve: hash not found");
        format!("error: hash not found: {hash}")
    }
}

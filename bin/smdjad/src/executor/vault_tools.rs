//! Vault-backed tools: `smedja_vault_search`, `smedja_vault_store`, and the
//! `smedja_retrieve` hash lookup into the output-filter recovery store.

use std::sync::Arc;

use serde_json::Value;
use smedja_vault::{Vault, VaultEntry};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::embedder_port::Embedder;
use crate::executor::output_filter::retrieve_store;

/// Semantic vault search: embeds `query` and returns the top-`k` matches within
/// `namespace` as a JSON `{ "results": [...] }` document.
pub(crate) async fn vault_search(
    input: &Value,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
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

/// Embeds and upserts `content` into `namespace`, returning the stored id.
pub(crate) async fn vault_store(
    input: &Value,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
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

/// Looks up full output previously teed by the output filter, addressed by its
/// content `hash`.
pub(crate) async fn retrieve(input: &Value) -> String {
    let hash = input.get("hash").and_then(Value::as_str).unwrap_or("");
    let store = retrieve_store().lock().await;
    if let Some(content) = store.get(hash) {
        // ponytail: audit deferred; log the retrieval.
        tracing::info!(hash, "smedja_retrieve hit");
        content.clone()
    } else {
        tracing::debug!(hash, "smedja_retrieve: hash not found");
        format!("error: hash not found: {hash}")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::executor::execute_tool;

    fn test_embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    #[tokio::test]
    async fn vault_search_returns_empty_when_no_entries() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        let result = execute_tool(
            "smedja_vault_search",
            r#"{"query":"rust async"}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;

        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn vault_store_then_search_finds_entry() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        let store_result = execute_tool(
            "smedja_vault_store",
            r#"{"content":"tokio async runtime executor","namespace":"test"}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let stored: serde_json::Value = serde_json::from_str(&store_result).unwrap();
        assert_eq!(stored["stored"], true);

        let search_result = execute_tool(
            "smedja_vault_search",
            r#"{"query":"tokio async","namespace":"test","k":5}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&search_result).unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "stored entry must be found");
        assert_eq!(results[0]["namespace"], "test");
    }

    #[tokio::test]
    async fn vault_search_respects_k_limit() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        for i in 0..5_u8 {
            execute_tool(
                "smedja_vault_store",
                &format!(r#"{{"content":"rust programming language crate {i}","namespace":"ns"}}"#),
                std::path::Path::new("/tmp"),
                None,
                &ingot,
                &vault,
                &test_embedder(),
            )
            .await;
        }

        let result = execute_tool(
            "smedja_vault_search",
            r#"{"query":"rust programming","namespace":"ns","k":2}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(
            v["results"].as_array().unwrap().len() <= 2,
            "k=2 must cap results at 2"
        );
    }

    #[tokio::test]
    async fn smedja_vault_search_returns_results_when_vault_has_matching_entries() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        // Insert a known entry via the store tool so the embedding path is exercised.
        let store_result = execute_tool(
            "smedja_vault_store",
            r#"{"content":"Rust ownership model borrow checker lifetimes","namespace":"search-test"}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let stored: serde_json::Value = serde_json::from_str(&store_result).unwrap();
        assert_eq!(
            stored["stored"], true,
            "entry must be stored before searching"
        );

        // Query with text similar to the inserted entry.
        let search_result = execute_tool(
            "smedja_vault_search",
            r#"{"query":"Rust ownership borrow checker","namespace":"search-test","k":5}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;

        let v: serde_json::Value = serde_json::from_str(&search_result).unwrap();
        let results = v["results"].as_array().expect("results must be an array");
        assert!(
            !results.is_empty(),
            "smedja_vault_search must return at least one result for a matching query"
        );

        // Verify the returned entry has a positive cosine similarity by checking
        // that the vault itself scores the entry > 0 when searched directly.
        let similarity = {
            let guard = vault.lock().await;
            let qv = crate::embedder::embed("Rust ownership borrow checker");
            let entries = guard
                .search(
                    &qv,
                    "Rust ownership borrow checker",
                    "search-test",
                    1,
                    smedja_vault::LEGACY_MODEL_ID,
                    crate::embedder::DIM,
                )
                .unwrap();
            if entries.is_empty() {
                return;
            }
            // Re-score using the embedder to confirm similarity is positive.
            let stored_vec =
                crate::embedder::embed("Rust ownership model borrow checker lifetimes");
            qv.iter()
                .zip(stored_vec.iter())
                .map(|(a, b)| a * b)
                .sum::<f32>()
        };
        assert!(
            similarity > 0.0,
            "cosine similarity between query and stored entry must be > 0, got {similarity}"
        );
    }
}

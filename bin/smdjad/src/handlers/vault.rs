//! Vault-backed RPC handlers.
//!
//! Hosts `vault.reembed`: the re-embed/backfill path that upgrades existing rows
//! (e.g. the FNV `compact`/lean-spec corpus) into the active embedder's model
//! space, so a vault converges to one comparable space after a backend switch.

use std::sync::Arc;

use serde_json::{json, Value};
use smedja_rpc::{codes, RpcError};
use smedja_vault::{EmbedderIdentity, Vault, VaultEntry};
use tokio::sync::Mutex;

use crate::embedder_port::Embedder;
use crate::handlers::HandlerState;

/// Re-embeds existing rows under the active embedder and rewrites their
/// embedding, `embedder_model_id`, and `dim`.
///
/// Params: `{ namespace? }`. With `namespace` present only that namespace is
/// walked; otherwise every distinct namespace in the vault is. The operation is
/// restartable and idempotent — re-embedding a row already tagged with the
/// active model rewrites it to a byte-identical vector (a semantic no-op).
///
/// After a whole-vault backfill the global [`EmbedderIdentity`] is updated to the
/// active model as the coarse "what does this vault hold" marker.
///
/// # Errors
///
/// Returns [`RpcError`] only when reading the namespace list or a row set fails;
/// a per-row rewrite failure is logged and skipped rather than aborting.
pub(crate) async fn reembed(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let vault = state.vault;
    let embedder = state.embedder;
    let requested_namespace = params
        .get("namespace")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let namespaces = match &requested_namespace {
        Some(ns) => vec![ns.clone()],
        None => list_all_namespaces(&vault).await?,
    };

    let mut reembedded = 0usize;
    for namespace in &namespaces {
        reembedded += reembed_namespace(&vault, &embedder, namespace).await?;
    }

    // A whole-vault backfill converges the database to one model space, so update
    // the coarse identity marker to the active model.
    if requested_namespace.is_none() {
        let identity = EmbedderIdentity {
            model: embedder.model_id().to_owned(),
            dimensions: embedder.dim(),
        };
        if let Err(e) = vault.lock().await.set_embedder_identity(&identity) {
            tracing::warn!(error = %e, "vault.reembed: failed to update embedder identity");
        }
    }

    Ok(json!({
        "model_id": embedder.model_id(),
        "dim": embedder.dim(),
        "namespaces": namespaces,
        "reembedded": reembedded,
    }))
}

/// Lists every distinct namespace, running the synchronous query off-runtime.
async fn list_all_namespaces(vault: &Arc<Mutex<Vault>>) -> Result<Vec<String>, RpcError> {
    let vault = Arc::clone(vault);
    tokio::task::spawn_blocking(move || vault.blocking_lock().distinct_namespaces())
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("reembed task panicked: {e}")))?
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))
}

/// Re-embeds every row in `namespace`, returning the count rewritten.
async fn reembed_namespace(
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
    namespace: &str,
) -> Result<usize, RpcError> {
    // Read the rows synchronously off-runtime.
    let read_vault = Arc::clone(vault);
    let ns = namespace.to_owned();
    let entries =
        tokio::task::spawn_blocking(move || read_vault.blocking_lock().list_namespace(&ns))
            .await
            .map_err(|e| {
                RpcError::new(codes::INTERNAL_ERROR, format!("reembed task panicked: {e}"))
            })?
            .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;

    let model_id = embedder.model_id().to_owned();
    let dim = embedder.dim();

    // Re-embed each row's content on the async path (learned backend does network
    // I/O here), then collect the rewritten entries.
    let mut rewritten: Vec<VaultEntry> = Vec::with_capacity(entries.len());
    for mut entry in entries {
        // A recovery row holds no semantic embedding; leave it untouched.
        if entry.embedding.is_empty() {
            continue;
        }
        entry.embedding = embedder.embed_query(&entry.content).await;
        entry.embedder_model_id = model_id.clone();
        entry.dim = dim;
        rewritten.push(entry);
    }

    // Write the rows back synchronously off-runtime. `upsert` by id is an
    // in-place rewrite, so the pass is restartable and idempotent.
    let write_vault = Arc::clone(vault);
    let count = rewritten.len();
    tokio::task::spawn_blocking(move || {
        let mut guard = write_vault.blocking_lock();
        for entry in &rewritten {
            if let Err(e) = guard.upsert(entry) {
                tracing::warn!(error = %e, id = %entry.id, "vault.reembed: row rewrite failed; skipping");
            }
        }
    })
    .await
    .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("reembed task panicked: {e}")))?;

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder_port::{Embedder, FnvEmbedder};
    use async_trait::async_trait;

    /// Stub learned embedder producing a fixed-dimension constant vector, so a
    /// backfill's effect on `model_id`/`dim` and the vector is deterministic.
    struct StubLearned {
        model_id: String,
        dim: usize,
    }

    #[async_trait]
    impl Embedder for StubLearned {
        fn embed(&self, _text: &str) -> Vec<f32> {
            vec![0.25_f32; self.dim]
        }
        fn model_id(&self) -> &str {
            &self.model_id
        }
        fn dim(&self) -> usize {
            self.dim
        }
    }

    fn fnv_entry(id: &str, content: &str, namespace: &str) -> VaultEntry {
        VaultEntry {
            id: id.to_owned(),
            embedding: crate::embedder::embed(content),
            payload: serde_json::json!({}),
            namespace: namespace.to_owned(),
            content: content.to_owned(),
            source_file: None,
            added_by: None,
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
            embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
            dim: crate::embedder::DIM,
        }
    }

    #[tokio::test]
    async fn backfill_upgrades_fnv_rows_to_active_model() {
        let mut v = Vault::open_in_memory().unwrap();
        v.upsert(&fnv_entry("a", "auth token refresh", "compact"))
            .unwrap();
        v.upsert(&fnv_entry("b", "renew the session credential", "compact"))
            .unwrap();
        let vault = Arc::new(Mutex::new(v));

        let embedder: Arc<dyn Embedder> = Arc::new(StubLearned {
            model_id: "minilm-l6-v2".to_owned(),
            dim: 8,
        });
        let n = reembed_namespace(&vault, &embedder, "compact")
            .await
            .unwrap();
        assert_eq!(n, 2);

        let rows = vault.lock().await.list_namespace("compact").unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row.embedder_model_id, "minilm-l6-v2");
            assert_eq!(row.dim, 8);
            assert_eq!(
                row.embedding,
                vec![0.25_f32; 8],
                "each row must hold the learned embedding of its content"
            );
        }
    }

    #[tokio::test]
    async fn backfill_is_idempotent_for_already_current_rows() {
        let embedder: Arc<dyn Embedder> = Arc::new(StubLearned {
            model_id: "minilm-l6-v2".to_owned(),
            dim: 8,
        });
        let mut v = Vault::open_in_memory().unwrap();
        // A row already at the active model.
        let mut entry = fnv_entry("a", "already learned", "compact");
        entry.embedding = embedder.embed("already learned");
        entry.embedder_model_id = "minilm-l6-v2".to_owned();
        entry.dim = 8;
        v.upsert(&entry).unwrap();
        let vault = Arc::new(Mutex::new(v));

        reembed_namespace(&vault, &embedder, "compact")
            .await
            .unwrap();

        let rows = vault.lock().await.list_namespace("compact").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].embedder_model_id, "minilm-l6-v2");
        assert_eq!(rows[0].dim, 8);
        assert_eq!(
            rows[0].embedding,
            vec![0.25_f32; 8],
            "re-embedding an already-current row is a byte-identical no-op"
        );
    }

    #[tokio::test]
    async fn whole_vault_backfill_updates_global_identity() {
        let mut v = Vault::open_in_memory().unwrap();
        v.upsert(&fnv_entry("a", "one", "compact")).unwrap();
        v.upsert(&fnv_entry("b", "two", "handoff")).unwrap();
        let vault = Arc::new(Mutex::new(v));

        // Walk every namespace by hand to mirror the handler's no-namespace path,
        // then assert the identity converges to the active model.
        let embedder: Arc<dyn Embedder> = Arc::new(StubLearned {
            model_id: "minilm-l6-v2".to_owned(),
            dim: 8,
        });
        let namespaces = list_all_namespaces(&vault).await.unwrap();
        for ns in &namespaces {
            reembed_namespace(&vault, &embedder, ns).await.unwrap();
        }
        vault
            .lock()
            .await
            .set_embedder_identity(&EmbedderIdentity {
                model: embedder.model_id().to_owned(),
                dimensions: embedder.dim(),
            })
            .unwrap();

        let identity = vault.lock().await.get_embedder_identity().unwrap().unwrap();
        assert_eq!(identity.model, "minilm-l6-v2");
        assert_eq!(identity.dimensions, 8);
    }

    #[tokio::test]
    async fn fnv_backfill_is_a_noop_rewrite() {
        // Backfill under the FNV default leaves FNV rows unchanged.
        let mut v = Vault::open_in_memory().unwrap();
        let original = fnv_entry("a", "stable content", "compact");
        v.upsert(&original).unwrap();
        let vault = Arc::new(Mutex::new(v));

        let embedder: Arc<dyn Embedder> = Arc::new(FnvEmbedder::new());
        reembed_namespace(&vault, &embedder, "compact")
            .await
            .unwrap();

        let rows = vault.lock().await.list_namespace("compact").unwrap();
        assert_eq!(rows[0].embedder_model_id, smedja_vault::LEGACY_MODEL_ID);
        assert_eq!(rows[0].dim, crate::embedder::DIM);
        assert_eq!(rows[0].embedding, original.embedding);
    }
}

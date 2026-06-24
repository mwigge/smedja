//! Vault-backed [`ColdStore`] adapter.
//!
//! Bridges [`smedja_memory::WorkingMemory`]'s cold-retrieval port onto the
//! daemon's synchronous [`Vault`] and bag-of-words embedder. The memory crate
//! owns the abstraction; this adapter is the only place that knows both the
//! vault and the embedder, keeping the dependency arrow pointing into
//! `smedja-memory`.

use std::sync::Arc;

use smedja_adapter::types::Message as AdapterMessage;
use smedja_memory::{estimate_tokens, ColdResult, ColdStore};
use smedja_vault::Vault;
use tokio::sync::Mutex;

use crate::embedder_port::Embedder;

/// Minimum relevance score a vault result must clear to be surfaced as cold
/// context. Hybrid scores below this floor are treated as noise and dropped.
pub(crate) const MIN_COLD_SCORE: f32 = 0.05;

/// Adapts a [`Vault`] behind the [`ColdStore`] port.
///
/// Embeds the query with the daemon embedder, runs the hybrid
/// cosine + keyword + recency search inside [`tokio::task::spawn_blocking`]
/// (because [`Vault`] is synchronous), and maps each surviving row to a
/// [`ColdResult`].
pub(crate) struct VaultColdStore {
    vault: Arc<Mutex<Vault>>,
    /// Resolved embedding backend; embeds the query and tags the comparison
    /// space (`model_id`/`dim`) so only same-model rows are ranked.
    embedder: Arc<dyn Embedder>,
    /// Minimum score a result must clear to be returned.
    min_score: f32,
}

impl VaultColdStore {
    /// Creates an adapter over `vault` and `embedder` using the default score floor.
    pub(crate) fn new(vault: Arc<Mutex<Vault>>, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            vault,
            embedder,
            min_score: MIN_COLD_SCORE,
        }
    }
}

#[async_trait::async_trait]
impl ColdStore for VaultColdStore {
    async fn retrieve(&self, query: &str, namespace: &str, k: usize) -> Vec<ColdResult> {
        let query = query.to_owned();
        let namespace = namespace.to_owned();
        let vault = Arc::clone(&self.vault);
        let min_score = self.min_score;

        // Embed on the async path (the learned backend does network I/O here,
        // degrading to FNV on failure) before the synchronous vault work.
        let query_vec = self.embedder.embed_query(&query).await;
        let model_id = self.embedder.model_id().to_owned();
        let dim = self.embedder.dim();

        // `Vault` is synchronous and acquiring its mutex via `blocking_lock`
        // would stall the async executor, so run the whole search off-runtime.
        let join = tokio::task::spawn_blocking(move || {
            let guard = vault.blocking_lock();
            match guard.search(&query_vec, &query, &namespace, k, &model_id, dim) {
                Ok(entries) => entries
                    .into_iter()
                    .filter_map(|e| {
                        let score = entry_score(&query_vec, &e);
                        if score < min_score {
                            return None;
                        }
                        Some(ColdResult {
                            content: e.content,
                            score,
                            namespace: e.namespace,
                        })
                    })
                    .collect::<Vec<_>>(),
                Err(e) => {
                    tracing::debug!(error = %e, "cold retrieval vault search failed");
                    Vec::new()
                }
            }
        })
        .await;

        match join {
            Ok(results) => results,
            Err(e) => {
                tracing::debug!(error = %e, "cold retrieval task panicked");
                Vec::new()
            }
        }
    }
}

/// Computes the cosine component of the relevance score for floor comparison.
///
/// [`Vault::search`] orders by an internal hybrid score (cosine + keyword +
/// recency) but does not expose it on [`VaultEntry`], so the adapter derives a
/// comparable cosine relevance score from the stored embedding. Embeddings are
/// L2-normalised by the daemon embedder, so this matches the cosine term the
/// vault ranks on. Returns `0.0` when either vector has zero magnitude.
fn entry_score(query_vec: &[f32], entry: &smedja_vault::VaultEntry) -> f32 {
    let stored = &entry.embedding;
    let dot: f32 = query_vec.iter().zip(stored).map(|(x, y)| x * y).sum();
    let mag_a: f32 = query_vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = stored.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        0.0
    } else {
        dot / (mag_a * mag_b)
    }
}

/// Assembles a single delimited `<cold_context>` system message from recalled
/// content, capped at `cold_budget` estimated tokens.
///
/// `recalled` is expected in descending relevance order. Entries are admitted
/// highest-first; an entry that would overflow the budget is skipped so a later,
/// cheaper entry can still fit. Returns the assembled message together with the
/// number of admitted entries, or `None` when nothing is admitted.
pub(crate) fn assemble_cold_block(
    recalled: &[AdapterMessage],
    cold_budget: usize,
) -> Option<(AdapterMessage, usize)> {
    let mut admitted: Vec<&str> = Vec::new();
    let mut spent = 0usize;
    for msg in recalled {
        let cost = estimate_tokens(&msg.content);
        if spent + cost > cold_budget {
            continue;
        }
        spent += cost;
        admitted.push(&msg.content);
    }
    if admitted.is_empty() {
        return None;
    }
    let count = admitted.len();
    let block = format!("<cold_context>\n{}\n</cold_context>", admitted.join("\n\n"));
    Some((AdapterMessage::system(block), count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_vault::VaultEntry;

    /// Default FNV embedder for the cold-store adapter tests.
    fn test_embedder() -> Arc<dyn Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    fn insert(vault: &mut Vault, id: &str, content: &str, namespace: &str) {
        let embedding = crate::embedder::embed(content);
        let entry = VaultEntry {
            id: id.to_owned(),
            embedding,
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
        };
        vault.upsert(&entry).expect("upsert must succeed");
    }

    #[tokio::test]
    async fn vault_cold_store_retrieves_ranked_entries() {
        let mut vault = Vault::open_in_memory().expect("in-memory vault must open");
        insert(
            &mut vault,
            "near",
            "rust async tokio runtime executor scheduling",
            "compact",
        );
        insert(
            &mut vault,
            "far",
            "rust async tokio runtime executor scheduling tasks futures",
            "compact",
        );
        let adapter = VaultColdStore::new(Arc::new(Mutex::new(vault)), test_embedder());

        let results = adapter
            .retrieve("rust async tokio runtime", "compact", 5)
            .await;

        assert_eq!(results.len(), 2, "both matching entries must be returned");
        // Results preserve the vault's descending-score order.
        assert!(
            results[0].score >= results[1].score,
            "results must be ranked by descending score: {} then {}",
            results[0].score,
            results[1].score
        );
        assert!(results.iter().all(|r| r.namespace == "compact"));
    }

    #[tokio::test]
    async fn vault_cold_store_returns_empty_when_no_match() {
        let vault = Vault::open_in_memory().expect("in-memory vault must open");
        let adapter = VaultColdStore::new(Arc::new(Mutex::new(vault)), test_embedder());
        let results = adapter.retrieve("anything", "compact", 5).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn cold_context_block_injected_and_budget_capped() {
        use smedja_memory::{estimate_tokens, WorkingMemory};

        // Populate a vault with several "compact" summaries the query overlaps.
        let mut vault = Vault::open_in_memory().expect("in-memory vault must open");
        for i in 0..5 {
            insert(
                &mut vault,
                &format!("c{i}"),
                &format!(
                    "rust async tokio runtime executor scheduling notes entry number {i} \
                     with additional body text to cost a meaningful number of tokens"
                ),
                "compact",
            );
        }
        let adapter = Arc::new(VaultColdStore::new(
            Arc::new(Mutex::new(vault)),
            test_embedder(),
        ));

        // Tiny tier budget so the cap is exercised: only a fraction admits.
        let budget_tokens = 200usize;
        let cold_budget = budget_tokens / 4;
        let mut mem = WorkingMemory::new(budget_tokens).with_cold_store(adapter);
        mem.set_cold_query("compact", 5);

        let recalled = mem.cold_context("rust async tokio runtime").await;
        assert!(!recalled.is_empty(), "vault must return matching entries");

        let (block, count) =
            assemble_cold_block(&recalled, cold_budget).expect("a cold block must be assembled");

        // The assembled block is a single delimited system message.
        assert_eq!(block.role, smedja_adapter::types::Role::System);
        assert!(block.content.starts_with("<cold_context>"));
        assert!(block.content.ends_with("</cold_context>"));
        assert!(count >= 1 && count <= recalled.len());

        // The block's estimated cost must not exceed the cold budget fraction.
        assert!(
            estimate_tokens(&block.content)
                <= cold_budget + estimate_tokens("<cold_context>\n\n</cold_context>"),
            "cold block cost {} must stay within the cold budget {cold_budget}",
            estimate_tokens(&block.content)
        );

        // The block lands ahead of the user turn, inside the sealed prefix.
        mem.push(block);
        mem.push(AdapterMessage::user("the live user turn"));
        mem.seal_prefix();
        let msgs = mem.messages();
        let cold_idx = msgs
            .iter()
            .position(|m| m.content.contains("<cold_context>"))
            .expect("cold block must be present");
        let user_idx = msgs
            .iter()
            .position(|m| m.content == "the live user turn")
            .expect("user turn must be present");
        assert!(cold_idx < user_idx, "cold block must precede the user turn");
        assert!(
            cold_idx < mem.stable_prefix(),
            "cold block must fall inside the sealed prefix"
        );
    }

    #[test]
    fn assemble_cold_block_returns_none_when_nothing_fits() {
        let recalled = vec![AdapterMessage::system(
            "a fairly long recalled entry that will not fit a zero budget",
        )];
        assert!(assemble_cold_block(&recalled, 0).is_none());
    }

    #[test]
    fn assemble_cold_block_returns_none_for_empty_input() {
        assert!(assemble_cold_block(&[], 1000).is_none());
    }

    #[tokio::test]
    async fn vault_cold_store_drops_below_floor() {
        let mut vault = Vault::open_in_memory().expect("in-memory vault must open");
        // Disjoint vocabulary → cosine ~0, below the score floor.
        insert(
            &mut vault,
            "unrelated",
            "python django orm migrations templating",
            "compact",
        );
        let adapter = VaultColdStore::new(Arc::new(Mutex::new(vault)), test_embedder());
        let results = adapter
            .retrieve("rust async tokio runtime", "compact", 5)
            .await;
        assert!(
            results.is_empty(),
            "results below the score floor must be discarded"
        );
    }
}

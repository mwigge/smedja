//! Cold-stratum retrieval port.
//!
//! [`WorkingMemory`](crate::WorkingMemory) recalls context from beyond the warm
//! window through the [`ColdStore`] abstraction rather than depending on a
//! concrete store. The daemon owns both the vault and the embedder and supplies
//! a vault-backed adapter; this crate stays a pure context-assembly layer with
//! no edge to `smedja-vault`.

/// A single ranked result returned by a [`ColdStore`].
///
/// Results are produced in descending relevance order by the store; the
/// `namespace` records which logical partition the content came from.
#[derive(Debug, Clone, PartialEq)]
pub struct ColdResult {
    /// Recalled text content.
    pub content: String,
    /// Relevance score; higher is more relevant. Composition is store-defined.
    pub score: f32,
    /// Namespace the content was retrieved from.
    pub namespace: String,
}

/// Port for semantic recall of cold-stratum context.
///
/// Implementors embed `query`, search their backing store within `namespace`,
/// and return up to `k` results ranked by descending relevance. Returning an
/// empty `Vec` (rather than an error) is the expected "no match" signal.
#[async_trait::async_trait]
pub trait ColdStore: Send + Sync {
    /// Retrieves up to `k` results for `query` from `namespace`, ranked by
    /// descending relevance score.
    async fn retrieve(&self, query: &str, namespace: &str, k: usize) -> Vec<ColdResult>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records the arguments of the most recent `retrieve` call so the test can
    /// assert the query, namespace, and `k` are forwarded unchanged.
    #[derive(Default)]
    struct RecordingStore {
        calls: Mutex<Vec<(String, String, usize)>>,
    }

    #[async_trait::async_trait]
    impl ColdStore for RecordingStore {
        async fn retrieve(&self, query: &str, namespace: &str, k: usize) -> Vec<ColdResult> {
            self.calls.lock().expect("lock must not be poisoned").push((
                query.to_owned(),
                namespace.to_owned(),
                k,
            ));
            Vec::new()
        }
    }

    #[tokio::test]
    async fn cold_store_retrieve_is_invoked_with_query_namespace_and_k() {
        let store = RecordingStore::default();
        let results = store
            .retrieve("how do I seal the prefix", "compact", 3)
            .await;
        assert!(results.is_empty());

        let calls = store.calls.lock().expect("lock must not be poisoned");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            (
                "how do I seal the prefix".to_owned(),
                "compact".to_owned(),
                3
            )
        );
    }
}

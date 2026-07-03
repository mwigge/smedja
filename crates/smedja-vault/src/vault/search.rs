//! Retrieval: hybrid [`Vault::search`] and pure-cosine [`Vault::query`].

use super::{now_secs, resolve_dim, resolve_model_id, SearchRow, Vault, VaultEntry};
use crate::error::VaultError;
use crate::similarity::cosine_sim;

impl Vault {
    /// Performs a hybrid search: cosine similarity + keyword boost + recency boost.
    ///
    /// Only rows produced by the same model as the query participate: a row whose
    /// resolved `embedder_model_id` ≠ `query_model_id` or resolved `dim` ≠
    /// `query_dim` is skipped before any cosine comparison — never an error.
    /// Comparing vectors from different models is meaningless, so mismatched rows
    /// are excluded from ranking rather than silently compared (or crashed).
    ///
    /// For each surviving entry in `namespace`:
    /// - Compute cosine similarity with `query_vec`.
    /// - Add a keyword boost: for each whitespace-split token in `query_text`,
    ///   count case-insensitive occurrences in `entry.content` and add `0.01` per
    ///   occurrence.
    /// - Add a recency boost of `0.1` when `entry.created_at > (now − 86400.0)`.
    ///
    /// Results are sorted descending by total score. The top `k` are returned.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] on a database failure or [`VaultError::Json`] if
    /// a stored payload cannot be deserialised.
    #[must_use = "the search result is the entire purpose of calling this function"]
    #[allow(clippy::too_many_lines)] // single hybrid-scoring scan; splitting it would obscure the flow
    pub fn search(
        &self,
        query_vec: &[f32],
        query_text: &str,
        namespace: &str,
        k: usize,
        query_model_id: &str,
        query_dim: usize,
    ) -> Result<Vec<VaultEntry>, VaultError> {
        let namespace = if namespace.is_empty() {
            "default"
        } else {
            namespace
        };

        let mut stmt = self.conn.prepare(
            "SELECT id, embedding, payload, namespace, content, source_file, added_by, \
             chunk_index, parent_id, created_at, embedder_model_id, dim \
             FROM vault_entries WHERE namespace = ?1",
        )?;

        let now = now_secs();
        let terms: Vec<String> = query_text
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();

        let rows: Vec<SearchRow> = stmt
            .query_map(rusqlite::params![namespace], |row| {
                Ok(SearchRow {
                    id: row.get(0)?,
                    bytes: row.get(1)?,
                    payload_str: row.get(2)?,
                    ns: row.get(3)?,
                    content: row.get(4)?,
                    source_file: row.get(5)?,
                    added_by: row.get(6)?,
                    chunk_index: row.get(7)?,
                    parent_id: row.get(8)?,
                    created_at: row.get(9)?,
                    model_id: row.get(10)?,
                    dim: row.get(11)?,
                })
            })?
            .collect::<Result<_, rusqlite::Error>>()?;

        let mut scored: Vec<(f32, VaultEntry)> = rows
            .into_iter()
            // Same-model-only: skip any row whose resolved model/dim differs from
            // the query's before it ever reaches `cosine_sim`.
            .filter(|row| {
                resolve_model_id(row.model_id.clone()) == query_model_id
                    && resolve_dim(row.dim, row.bytes.len()) == query_dim
            })
            .map(|row| {
                let SearchRow {
                    id,
                    bytes,
                    payload_str,
                    ns,
                    content,
                    source_file,
                    added_by,
                    chunk_index,
                    parent_id,
                    created_at,
                    model_id,
                    dim,
                } = row;

                let embedder_model_id = resolve_model_id(model_id);
                let resolved_dim = resolve_dim(dim, bytes.len());
                let stored: &[f32] = bytemuck::cast_slice::<u8, f32>(&bytes);
                let cosine_score = cosine_sim(query_vec, stored);

                let content_lower = content.to_lowercase();
                let keyword_boost: f32 = terms
                    .iter()
                    .map(|term| {
                        let mut count = 0usize;
                        let mut start = 0;
                        while let Some(pos) = content_lower[start..].find(term.as_str()) {
                            count += 1;
                            start += pos + term.len();
                        }
                        #[allow(clippy::cast_precision_loss)]
                        // keyword match counts fit comfortably in f32
                        {
                            count as f32 * 0.01
                        }
                    })
                    .sum();

                let recency_boost = if created_at > (now - 86_400.0) {
                    0.1_f32
                } else {
                    0.0_f32
                };

                let total_score = cosine_score + keyword_boost + recency_boost;

                let payload: serde_json::Value = serde_json::from_str(&payload_str)?;

                let entry = VaultEntry {
                    id,
                    embedding: stored.to_vec(),
                    payload,
                    namespace: ns,
                    content,
                    source_file,
                    added_by,
                    chunk_index,
                    parent_id,
                    created_at,
                    embedder_model_id,
                    dim: resolved_dim,
                };

                Ok((total_score, entry))
            })
            .collect::<Result<_, VaultError>>()?;

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);

        Ok(scored.into_iter().map(|(_, e)| e).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{entry, LEGACY_MODEL_ID};
    use super::*;

    // Pure-cosine `query()` tests live with the code in query.rs.

    // ── namespace ────────────────────────────────────────────────────────────

    #[test]
    fn namespace_round_trip() {
        let mut vault = Vault::open_in_memory().unwrap();
        let mut e = entry("ns-entry", vec![1.0_f32, 0.0]);
        e.namespace = "agents".to_string();
        e.content = "agent context".to_string();
        vault.insert(&e).unwrap();

        let results = vault
            .search(&[1.0_f32, 0.0], "agent", "agents", 5, LEGACY_MODEL_ID, 2)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "ns-entry");
        assert_eq!(results[0].namespace, "agents");
    }

    // ── recency boost ─────────────────────────────────────────────────────────

    #[test]
    fn hybrid_recency_boost() {
        let mut vault = Vault::open_in_memory().unwrap();

        // Old entry: created_at far in the past (epoch 0 → no recency boost).
        let mut old = entry("old", vec![1.0_f32, 0.0]);
        old.content = "content".to_string();
        old.created_at = 1.0; // Unix time 1 — effectively ancient
        vault.upsert(&old).unwrap();

        // Recent entry: created_at is now → gets +0.1 recency boost.
        let mut recent = entry("recent", vec![1.0_f32, 0.0]);
        recent.content = "content".to_string();
        recent.created_at = now_secs();
        vault.upsert(&recent).unwrap();

        let results = vault
            .search(&[1.0_f32, 0.0], "", "default", 2, LEGACY_MODEL_ID, 2)
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].id, "recent",
            "recent entry must rank first due to recency boost"
        );
    }

    // ── per-row model/dim tagging ─────────────────────────────────────────────

    #[test]
    fn model_id_and_dim_round_trip_through_insert_and_search() {
        let mut vault = Vault::open_in_memory().unwrap();
        let mut e = entry("tagged", vec![1.0_f32, 0.0, 0.0]);
        e.namespace = "ns".to_string();
        e.content = "tagged content".to_string();
        e.embedder_model_id = "minilm-l6-v2".to_string();
        e.dim = 3;
        vault.insert(&e).unwrap();

        let results = vault
            .search(&[1.0_f32, 0.0, 0.0], "tagged", "ns", 5, "minilm-l6-v2", 3)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].embedder_model_id, "minilm-l6-v2");
        assert_eq!(results[0].dim, 3);
    }

    #[test]
    fn legacy_row_reads_back_with_fnv_default_and_blob_derived_dim() {
        let vault = Vault::open_in_memory().unwrap();
        // Insert a row the legacy way: explicit SQL that leaves the new columns
        // NULL, exactly as a pre-migration database would have stored it.
        let embedding = vec![0.5_f32; 128];
        let bytes = bytemuck::cast_slice::<f32, u8>(&embedding);
        vault
            .conn
            .execute(
                "INSERT INTO vault_entries (id, embedding, payload, namespace, content) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["legacy", bytes, "{}", "default", "legacy content"],
            )
            .unwrap();

        let results = vault
            .search(&embedding, "legacy", "default", 5, LEGACY_MODEL_ID, 128)
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "legacy row must be same-model with FNV id"
        );
        assert_eq!(results[0].embedder_model_id, LEGACY_MODEL_ID);
        assert_eq!(
            results[0].dim, 128,
            "legacy dim must derive from BLOB length / 4"
        );
    }

    // ── same-model-only comparison ────────────────────────────────────────────

    #[test]
    fn search_returns_only_same_model_rows() {
        let mut vault = Vault::open_in_memory().unwrap();

        let mut fnv = entry("fnv", vec![1.0_f32, 0.0]);
        fnv.namespace = "mixed".to_string();
        fnv.content = "shared".to_string();
        fnv.embedder_model_id = LEGACY_MODEL_ID.to_string();
        fnv.dim = 2;
        vault.upsert(&fnv).unwrap();

        // A learned row of a different model AND a different dimension.
        let mut learned = entry("learned", vec![1.0_f32, 0.0, 0.0]);
        learned.namespace = "mixed".to_string();
        learned.content = "shared".to_string();
        learned.embedder_model_id = "minilm".to_string();
        learned.dim = 3;
        vault.upsert(&learned).unwrap();

        // Query under the FNV model: only the FNV row is a candidate; the
        // mismatched-dim learned row is excluded, never compared, never errors.
        let results = vault
            .search(&[1.0_f32, 0.0], "shared", "mixed", 5, LEGACY_MODEL_ID, 2)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "fnv");

        // Query under the learned model: only the learned row is returned.
        let results = vault
            .search(&[1.0_f32, 0.0, 0.0], "shared", "mixed", 5, "minilm", 3)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "learned");
    }

    #[test]
    fn same_model_results_rank_by_descending_hybrid_score() {
        let mut vault = Vault::open_in_memory().unwrap();
        // Regression guard: unchanged hybrid scoring for same-model rows.
        vault.upsert(&entry("exact", vec![1.0_f32, 0.0])).unwrap();
        vault.upsert(&entry("ortho", vec![0.0_f32, 1.0])).unwrap();
        vault.upsert(&entry("close", vec![0.6_f32, 0.8])).unwrap();

        let results = vault
            .search(&[1.0_f32, 0.0], "", "default", 3, LEGACY_MODEL_ID, 2)
            .unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].id, "exact", "highest cosine must rank first");
        assert_eq!(results[2].id, "ortho", "orthogonal must rank last");
    }
}

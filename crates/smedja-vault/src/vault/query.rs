//! Pure-cosine top-K retrieval: [`Vault::query`].

use super::{resolve_dim, resolve_model_id, QueryResult, QueryRow, Vault};
use crate::error::VaultError;
use crate::similarity::cosine_sim;

impl Vault {
    /// Returns the top-K entries by cosine similarity to `query_embedding`.
    ///
    /// Performs a full table scan; suitable for cold stores with fewer than
    /// ~10,000 entries. Returns fewer than `k` results when the vault contains
    /// fewer entries. Returns an empty `Vec` when the vault is empty.
    ///
    /// Only rows produced by the same model as the query participate: a row whose
    /// resolved `embedder_model_id` ≠ `query_model_id` or resolved `dim` ≠
    /// `query_dim` is skipped before any cosine comparison. A mismatched-dimension
    /// row is therefore excluded from ranking rather than raising an error — a
    /// vault mid-migration keeps returning correct same-model results.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if reading any row fails or [`VaultError::Json`]
    /// if a stored payload cannot be deserialised.
    #[must_use = "the query result is the entire purpose of calling this function"]
    pub fn query(
        &self,
        query_embedding: &[f32],
        k: usize,
        query_model_id: &str,
        query_dim: usize,
    ) -> Result<Vec<QueryResult>, VaultError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, embedding, payload, embedder_model_id, dim FROM vault_entries")?;

        let rows: Vec<QueryRow> = stmt
            .query_map([], |row| {
                Ok(QueryRow {
                    id: row.get(0)?,
                    bytes: row.get(1)?,
                    payload_str: row.get(2)?,
                    model_id: row.get(3)?,
                    dim: row.get(4)?,
                })
            })?
            .collect::<Result<_, rusqlite::Error>>()?;

        let mut scored: Vec<(f32, String, serde_json::Value)> = rows
            .into_iter()
            // Same-model-only: a row from a different model or dimension is never
            // fed to `cosine_sim` and never raises an error — it is simply not a
            // candidate.
            .filter(|row| {
                resolve_model_id(row.model_id.clone()) == query_model_id
                    && resolve_dim(row.dim, row.bytes.len()) == query_dim
            })
            .map(
                |QueryRow {
                     id,
                     bytes,
                     payload_str,
                     ..
                 }| {
                    // bytes were written by `bytemuck::cast_slice::<f32, u8>`,
                    // so the length is always a multiple of 4.
                    let stored: &[f32] = bytemuck::cast_slice::<u8, f32>(&bytes);
                    let score = cosine_sim(query_embedding, stored);
                    let payload: serde_json::Value = serde_json::from_str(&payload_str)?;
                    Ok((score, id, payload))
                },
            )
            .collect::<Result<_, VaultError>>()?;

        // Sort descending by score, then take the top K.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);

        let results = scored
            .into_iter()
            .map(|(score, id, payload)| QueryResult { id, score, payload })
            .collect();
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{entry, LEGACY_MODEL_ID};
    use super::*;

    #[test]
    fn query_returns_most_similar() {
        let mut vault = Vault::open_in_memory().unwrap();
        // [1,0] is the query; [1,0] should score 1.0, [0,1] scores 0.0,
        // [0.6, 0.8] is intermediate.
        vault.upsert(&entry("exact", vec![1.0_f32, 0.0])).unwrap();
        vault.upsert(&entry("ortho", vec![0.0_f32, 1.0])).unwrap();
        vault.upsert(&entry("close", vec![0.6_f32, 0.8])).unwrap();

        let results = vault.query(&[1.0_f32, 0.0], 3, LEGACY_MODEL_ID, 2).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0].id, "exact",
            "top result must be the identical vector"
        );
        assert!(
            results[0].score > results[1].score,
            "results must be sorted descending by score"
        );
    }

    #[test]
    fn query_k_larger_than_entries() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault.upsert(&entry("a", vec![1.0, 0.0])).unwrap();
        vault.upsert(&entry("b", vec![0.0, 1.0])).unwrap();
        vault.upsert(&entry("c", vec![0.5, 0.5])).unwrap();

        let results = vault
            .query(&[1.0_f32, 0.0], 10, LEGACY_MODEL_ID, 2)
            .unwrap();
        assert_eq!(results.len(), 3, "must return all entries when k > count");
    }

    #[test]
    fn query_empty_vault() {
        let vault = Vault::open_in_memory().unwrap();
        let results = vault.query(&[1.0_f32, 0.0], 5, LEGACY_MODEL_ID, 2).unwrap();
        assert!(
            results.is_empty(),
            "query on empty vault must return empty vec"
        );
    }

    #[test]
    fn query_excludes_mismatched_dimension_without_error() {
        let mut vault = Vault::open_in_memory().unwrap();

        let mut a = entry("a", vec![1.0_f32, 0.0]);
        a.dim = 2;
        vault.upsert(&a).unwrap();

        let mut b = entry("b", vec![1.0_f32, 0.0, 0.0]);
        b.embedder_model_id = "other".to_string();
        b.dim = 3;
        vault.upsert(&b).unwrap();

        // Querying with a dim-2 FNV vector must NOT raise DimensionMismatch.
        let results = vault.query(&[1.0_f32, 0.0], 5, LEGACY_MODEL_ID, 2).unwrap();
        assert_eq!(results.len(), 1, "only the same-model row is a candidate");
        assert_eq!(results[0].id, "a");
    }
}

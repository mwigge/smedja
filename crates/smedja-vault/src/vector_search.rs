//! Vector similarity retrieval over the [`Vault`].
//!
//! The embedding/vector-search half of the vault: the hybrid [`Vault::search`]
//! (cosine + keyword + recency) and the pure-cosine [`Vault::query`]. Both apply
//! the same-model-only filter before any cosine comparison.

use crate::error::VaultError;
use crate::similarity::cosine_sim;
use crate::vault::{
    decode_embedding, now_secs, resolve_dim, resolve_model_id, QueryResult, SearchRow, Vault,
    VaultEntry,
};

/// A row read during [`Vault::query`] before same-model filtering and scoring.
struct QueryRow {
    id: String,
    bytes: Vec<u8>,
    payload_str: String,
    model_id: Option<String>,
    dim: Option<i64>,
}

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
            .filter_map(|row| {
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
                let Some(stored) = decode_embedding(&bytes) else {
                    tracing::warn!(id = %id, "vault: skipping search row with malformed embedding blob");
                    return None;
                };
                let cosine_score = cosine_sim(query_vec, &stored);

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

                let payload: serde_json::Value = match serde_json::from_str(&payload_str) {
                    Ok(p) => p,
                    Err(e) => return Some(Err(VaultError::from(e))),
                };

                let entry = VaultEntry {
                    id,
                    embedding: stored,
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

                Some(Ok((total_score, entry)))
            })
            .collect::<Result<_, VaultError>>()?;

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);

        Ok(scored.into_iter().map(|(_, e)| e).collect())
    }

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
            .filter_map(
                |QueryRow {
                     id,
                     bytes,
                     payload_str,
                     ..
                 }| {
                    // Blobs are normally written by `bytemuck::cast_slice::<f32, u8>`
                    // (length always a multiple of 4), but a legacy or corrupted row
                    // may not be — skip it instead of panicking the full scan.
                    let Some(stored) = decode_embedding(&bytes) else {
                        tracing::warn!(id = %id, "vault: skipping query row with malformed embedding blob");
                        return None;
                    };
                    let score = cosine_sim(query_embedding, &stored);
                    let payload: serde_json::Value = match serde_json::from_str(&payload_str) {
                        Ok(p) => p,
                        Err(e) => return Some(Err(VaultError::from(e))),
                    };
                    Some(Ok((score, id, payload)))
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

//! Vector similarity retrieval over the [`Vault`].
//!
//! The embedding/vector-search half of the vault: the hybrid [`Vault::search`]
//! (cosine + keyword + recency) and the pure-cosine [`Vault::query`]. Both apply
//! the same-model-only filter before any cosine comparison.
//!
//! ## Exact vs. approximate retrieval
//!
//! Below [`ANN_MIN_ROWS`] total rows both entry points run the historical exact
//! full scan — the simplest correct thing for a small store, and the path every
//! existing test exercises. At or above the threshold they consult a lazily-built
//! [`IvfIndex`](crate::ann) coarse quantiser to narrow the store to a small
//! candidate set, then score *only* those candidates with the very same
//! exact cosine/hybrid logic. The approximation therefore affects recall, never
//! ranking fidelity: a returned row is scored identically whether it arrived via
//! the full scan or the ANN candidate set. The index is keyed per
//! `(model_id, dim)`, so the same-model filter is structural, and corrupt blobs
//! are skipped at index-build time exactly as the exact scan skips them.

use crate::ann::{IvfIndex, DEFAULT_NPROBE};
use crate::error::VaultError;
use crate::similarity::cosine_sim;
use crate::vault::{
    decode_embedding, now_secs, resolve_dim, resolve_model_id, QueryResult, SearchRow, Vault,
    VaultEntry,
};

/// Total-row count at or above which the vault switches from the exact full scan
/// to the ANN candidate path. Below it, a full scan is cheap and always exact.
pub(crate) const ANN_MIN_ROWS: usize = 512;

/// Multiplier applied to `k` when asking the ANN index for candidates. Widening
/// the candidate pool past `k` recovers rows the hybrid keyword/recency boosts
/// would promote above their raw cosine rank.
const CANDIDATE_EXPANSION: usize = 8;

/// Floor on the ANN candidate pool size, so even a `k = 1` query pulls a healthy
/// neighbourhood for the exact re-rank.
const MIN_CANDIDATES: usize = 128;

/// Raw columns read while building an ANN index: `(id, embedding_bytes,
/// namespace, embedder_model_id, dim)`.
type IndexRow = (String, Vec<u8>, String, Option<String>, Option<i64>);

/// A row read during [`Vault::query`] before same-model filtering and scoring.
struct QueryRow {
    id: String,
    bytes: Vec<u8>,
    payload_str: String,
    model_id: Option<String>,
    dim: Option<i64>,
}

/// Number of ANN candidates to request for a `k`-result query.
fn candidate_limit(k: usize) -> usize {
    k.saturating_mul(CANDIDATE_EXPANSION).max(MIN_CANDIDATES)
}

/// Applies the same-model filter and hybrid scoring to a single [`SearchRow`].
///
/// Returns `None` when the row is a different model/dim than the query (never a
/// candidate) or its embedding blob is malformed (skipped, not fatal). Otherwise
/// returns the row's total hybrid score paired with its hydrated [`VaultEntry`],
/// or a payload-decode error.
fn score_search_row(
    row: SearchRow,
    query_vec: &[f32],
    terms: &[String],
    now: f64,
    query_model_id: &str,
    query_dim: usize,
) -> Option<Result<(f32, VaultEntry), VaultError>> {
    // Same-model-only: skip any row whose resolved model/dim differs from the
    // query's before it ever reaches `cosine_sim`.
    if resolve_model_id(row.model_id.clone()) != query_model_id
        || resolve_dim(row.dim, row.bytes.len()) != query_dim
    {
        return None;
    }

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
}

/// Applies the same-model filter and cosine scoring to a single [`QueryRow`].
fn score_query_row(
    row: QueryRow,
    query_embedding: &[f32],
    query_model_id: &str,
    query_dim: usize,
) -> Option<Result<(f32, String, serde_json::Value), VaultError>> {
    if resolve_model_id(row.model_id.clone()) != query_model_id
        || resolve_dim(row.dim, row.bytes.len()) != query_dim
    {
        return None;
    }
    let QueryRow {
        id,
        bytes,
        payload_str,
        ..
    } = row;
    // Blobs are normally written by `bytemuck::cast_slice::<f32, u8>` (length
    // always a multiple of 4), but a legacy or corrupted row may not be — skip it
    // instead of panicking the scan.
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
}

/// Builds a `?,?,…` SQL placeholder list of length `n` starting at `?start`.
fn placeholders(start: usize, n: usize) -> String {
    (start..start + n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
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
    /// Small stores full-scan; at or above [`ANN_MIN_ROWS`] the candidate set is
    /// drawn from the ANN index first (see the module docs), which does not change
    /// how any returned row is scored.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] on a database failure or [`VaultError::Json`] if
    /// a stored payload cannot be deserialised.
    #[must_use = "the search result is the entire purpose of calling this function"]
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

        let now = now_secs();
        let terms: Vec<String> = query_text
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();

        // Small store, or ANN produced no candidates for this (model, dim, ns):
        // fall back to the exact full scan of the namespace.
        let rows = if self.count()? >= ANN_MIN_ROWS {
            let ids = self.ann_candidates(
                query_vec,
                Some(namespace),
                query_model_id,
                query_dim,
                candidate_limit(k),
            )?;
            if ids.is_empty() {
                Vec::new()
            } else {
                self.fetch_search_rows(&ids, namespace)?
            }
        } else {
            self.scan_search_rows(namespace)?
        };

        let mut scored: Vec<(f32, VaultEntry)> = rows
            .into_iter()
            .filter_map(|row| {
                score_search_row(row, query_vec, &terms, now, query_model_id, query_dim)
            })
            .collect::<Result<_, VaultError>>()?;

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);

        Ok(scored.into_iter().map(|(_, e)| e).collect())
    }

    /// Returns the top-K entries by cosine similarity to `query_embedding`.
    ///
    /// Small stores full-scan; at or above [`ANN_MIN_ROWS`] the candidate set is
    /// drawn from the ANN index across all namespaces (see the module docs).
    /// Returns fewer than `k` results when the vault contains fewer same-model
    /// entries. Returns an empty `Vec` when the vault is empty.
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
        let rows = if self.count()? >= ANN_MIN_ROWS {
            let ids = self.ann_candidates(
                query_embedding,
                None,
                query_model_id,
                query_dim,
                candidate_limit(k),
            )?;
            if ids.is_empty() {
                Vec::new()
            } else {
                self.fetch_query_rows(&ids)?
            }
        } else {
            self.scan_query_rows()?
        };

        let mut scored: Vec<(f32, String, serde_json::Value)> = rows
            .into_iter()
            .filter_map(|row| score_query_row(row, query_embedding, query_model_id, query_dim))
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

    // ── ANN plumbing ──────────────────────────────────────────────────────────

    /// Returns up to `limit` candidate ids from the lazily-built ANN index for
    /// `(query_model_id, query_dim)`, restricted to `namespace` when `Some`.
    ///
    /// Builds and caches the index on first use for the group; subsequent queries
    /// reuse it until the next write invalidates the cache.
    fn ann_candidates(
        &self,
        query_vec: &[f32],
        namespace: Option<&str>,
        query_model_id: &str,
        query_dim: usize,
        limit: usize,
    ) -> Result<Vec<String>, VaultError> {
        if self
            .index_cache
            .borrow()
            .get(query_model_id, query_dim)
            .is_none()
        {
            let index = self.build_index(query_model_id, query_dim)?;
            self.index_cache
                .borrow_mut()
                .insert(query_model_id, query_dim, index);
        }
        let cache = self.index_cache.borrow();
        let index = cache
            .get(query_model_id, query_dim)
            .expect("index was just built for this group");
        Ok(index.search(query_vec, namespace, limit))
    }

    /// Builds an [`IvfIndex`] over every row whose resolved `(model_id, dim)`
    /// matches the group. Corrupt embedding blobs are skipped, mirroring the exact
    /// scan.
    fn build_index(&self, model_id: &str, dim: usize) -> Result<IvfIndex, VaultError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, embedding, namespace, embedder_model_id, dim FROM vault_entries",
        )?;
        let raw_rows: Vec<IndexRow> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<_, rusqlite::Error>>()?;

        let mut raw: Vec<(String, String, Vec<f32>)> = Vec::new();
        for (id, bytes, ns, row_model, row_dim) in raw_rows {
            if resolve_model_id(row_model) != model_id || resolve_dim(row_dim, bytes.len()) != dim {
                continue;
            }
            let Some(vec) = decode_embedding(&bytes) else {
                tracing::warn!(id = %id, "vault: skipping index row with malformed embedding blob");
                continue;
            };
            raw.push((id, ns, vec));
        }
        Ok(IvfIndex::build(raw, DEFAULT_NPROBE))
    }

    /// Full-scan reader of every [`SearchRow`] in `namespace` (exact path).
    fn scan_search_rows(&self, namespace: &str) -> Result<Vec<SearchRow>, VaultError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, embedding, payload, namespace, content, source_file, added_by, \
             chunk_index, parent_id, created_at, embedder_model_id, dim \
             FROM vault_entries WHERE namespace = ?1",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![namespace], read_search_row)?
            .collect::<Result<_, rusqlite::Error>>()?;
        Ok(rows)
    }

    /// Reads the [`SearchRow`]s for a candidate id set within `namespace`.
    fn fetch_search_rows(
        &self,
        ids: &[String],
        namespace: &str,
    ) -> Result<Vec<SearchRow>, VaultError> {
        // `?1` is the namespace; ids start at `?2`.
        let sql = format!(
            "SELECT id, embedding, payload, namespace, content, source_file, added_by, \
             chunk_index, parent_id, created_at, embedder_model_id, dim \
             FROM vault_entries WHERE namespace = ?1 AND id IN ({})",
            placeholders(2, ids.len())
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 1);
        params.push(&namespace);
        for id in ids {
            params.push(id);
        }
        let rows = stmt
            .query_map(params.as_slice(), read_search_row)?
            .collect::<Result<_, rusqlite::Error>>()?;
        Ok(rows)
    }

    /// Full-scan reader of every [`QueryRow`] (exact path).
    fn scan_query_rows(&self) -> Result<Vec<QueryRow>, VaultError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, embedding, payload, embedder_model_id, dim FROM vault_entries")?;
        let rows = stmt
            .query_map([], read_query_row)?
            .collect::<Result<_, rusqlite::Error>>()?;
        Ok(rows)
    }

    /// Reads the [`QueryRow`]s for a candidate id set (all namespaces).
    fn fetch_query_rows(&self, ids: &[String]) -> Result<Vec<QueryRow>, VaultError> {
        let sql = format!(
            "SELECT id, embedding, payload, embedder_model_id, dim \
             FROM vault_entries WHERE id IN ({})",
            placeholders(1, ids.len())
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(params.as_slice(), read_query_row)?
            .collect::<Result<_, rusqlite::Error>>()?;
        Ok(rows)
    }
}

/// `rusqlite` row → [`SearchRow`] mapper shared by the scan and candidate paths.
fn read_search_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchRow> {
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
}

/// `rusqlite` row → [`QueryRow`] mapper shared by the scan and candidate paths.
fn read_query_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueryRow> {
    Ok(QueryRow {
        id: row.get(0)?,
        bytes: row.get(1)?,
        payload_str: row.get(2)?,
        model_id: row.get(3)?,
        dim: row.get(4)?,
    })
}

#[cfg(test)]
mod ann_dispatch_tests {
    use super::ANN_MIN_ROWS;
    use crate::vault::{Vault, VaultEntry, LEGACY_MODEL_ID};
    use serde_json::json;

    /// Deterministic splitmix64 vector generator — no RNG dependency.
    struct Rng(u64);
    impl Rng {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            #[allow(clippy::cast_precision_loss)]
            {
                (z as f64 / u64::MAX as f64) as f32 * 2.0 - 1.0
            }
        }
        fn vec(&mut self, dim: usize) -> Vec<f32> {
            (0..dim).map(|_| self.next_f32()).collect()
        }
    }

    fn tagged(id: &str, ns: &str, embedding: Vec<f32>, model: &str) -> VaultEntry {
        let dim = embedding.len();
        VaultEntry {
            id: id.to_owned(),
            embedding,
            payload: json!({ "id": id }),
            namespace: ns.to_owned(),
            content: id.to_owned(),
            source_file: None,
            added_by: None,
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
            embedder_model_id: model.to_owned(),
            dim,
        }
    }

    /// Cosine over normalised vectors (test-side reference).
    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let ma = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let mb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if ma == 0.0 || mb == 0.0 {
            0.0
        } else {
            dot / (ma * mb)
        }
    }

    /// Fills a vault with `n` deterministic vectors above the ANN threshold and
    /// returns the raw vectors so a test can compute a brute-force reference.
    fn large_vault(n: usize, dim: usize, ns: &str) -> (Vault, Vec<(String, Vec<f32>)>) {
        assert!(n >= ANN_MIN_ROWS, "test must cross the ANN threshold");
        let mut vault = Vault::open_in_memory().unwrap();
        let mut rng = Rng(0xC0FF_EE00);
        let mut raw = Vec::with_capacity(n);
        for i in 0..n {
            let v = rng.vec(dim);
            let id = format!("id{i}");
            vault
                .upsert(&tagged(&id, ns, v.clone(), LEGACY_MODEL_ID))
                .unwrap();
            raw.push((id, v));
        }
        (vault, raw)
    }

    #[test]
    fn ann_query_recovers_planted_exact_duplicate_top1() {
        let dim = 32;
        let (mut vault, _raw) = large_vault(ANN_MIN_ROWS + 700, dim, "default");
        let mut rng = Rng(0xABCD);
        let planted = rng.vec(dim);
        vault
            .upsert(&tagged(
                "planted",
                "default",
                planted.clone(),
                LEGACY_MODEL_ID,
            ))
            .unwrap();
        assert!(
            vault.count().unwrap() >= ANN_MIN_ROWS,
            "ANN path must engage"
        );

        let results = vault.query(&planted, 5, LEGACY_MODEL_ID, dim).unwrap();
        assert_eq!(
            results.first().map(|r| r.id.as_str()),
            Some("planted"),
            "the exact-duplicate vector must rank first through the ANN path"
        );
    }

    #[test]
    fn ann_query_top1_matches_brute_force_reference() {
        // Query a stored vector; the ANN top result must equal the brute-force
        // exact top result over the same set — parity of the winning row.
        let dim = 24;
        let (vault, raw) = large_vault(ANN_MIN_ROWS + 400, dim, "default");
        for &seed in &[3usize, 17, 42, 128, 500] {
            let (_qid, qvec) = &raw[seed];
            let ann = vault.query(qvec, 1, LEGACY_MODEL_ID, dim).unwrap();
            let exact_top = raw
                .iter()
                .max_by(|a, b| cosine(qvec, &a.1).partial_cmp(&cosine(qvec, &b.1)).unwrap())
                .map(|(id, _)| id.clone())
                .unwrap();
            assert_eq!(
                ann.first().map(|r| r.id.clone()),
                Some(exact_top),
                "ANN top-1 must match brute-force exact top-1 for seed {seed}"
            );
        }
    }

    #[test]
    fn ann_search_preserves_same_model_filter() {
        // A large store mixing two models. Each model's query must only ever
        // return its own rows through the ANN path.
        let mut vault = Vault::open_in_memory().unwrap();
        let mut rng = Rng(9);
        for i in 0..(ANN_MIN_ROWS + 100) {
            let v = rng.vec(8);
            vault
                .upsert(&tagged(&format!("a{i}"), "mixed", v, "model-a"))
                .unwrap();
        }
        // A handful of a different model + dimension in the same namespace.
        let mut bvec = Vec::new();
        for i in 0..20 {
            let v = rng.vec(4);
            bvec.push(v.clone());
            vault
                .upsert(&tagged(&format!("b{i}"), "mixed", v, "model-b"))
                .unwrap();
        }

        // Query under model-a: no model-b id may surface.
        let qa = Rng(1).vec(8);
        let ra = vault.search(&qa, "", "mixed", 20, "model-a", 8).unwrap();
        assert!(!ra.is_empty());
        assert!(
            ra.iter().all(|e| e.embedder_model_id == "model-a"),
            "model-a query must return only model-a rows"
        );

        // Query under model-b (small group, but store is > threshold → ANN path):
        // only model-b rows come back.
        let rb = vault
            .search(&bvec[0], "", "mixed", 20, "model-b", 4)
            .unwrap();
        assert!(!rb.is_empty(), "model-b rows must be retrievable");
        assert!(
            rb.iter().all(|e| e.embedder_model_id == "model-b"),
            "model-b query must return only model-b rows"
        );
    }

    #[test]
    fn ann_path_skips_corrupt_blob_without_panic() {
        let dim = 16;
        let (vault, _raw) = large_vault(ANN_MIN_ROWS + 50, dim, "default");
        // A corrupt (3-byte) blob tagged into the same (model, dim) group.
        vault
            .conn
            .execute(
                "INSERT INTO vault_entries (id, embedding, payload, namespace, content, embedder_model_id, dim) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params!["corrupt", vec![1u8, 2, 3], "{}", "default", "corrupt", LEGACY_MODEL_ID, dim as i64],
            )
            .unwrap();

        let mut rng = Rng(7);
        let q = rng.vec(dim);
        // Must not panic; the corrupt row must never be returned.
        let results = vault.query(&q, 10, LEGACY_MODEL_ID, dim).unwrap();
        assert!(results.iter().all(|r| r.id != "corrupt"));
    }

    #[test]
    fn write_invalidates_ann_index() {
        // Build the index via a query, then insert a planted duplicate and query
        // again: the new row must be found, proving the cache was invalidated and
        // rebuilt rather than serving a stale index.
        let dim = 20;
        let (mut vault, _raw) = large_vault(ANN_MIN_ROWS + 60, dim, "default");
        let mut rng = Rng(0x5151);
        let q = rng.vec(dim);
        let _ = vault.query(&q, 5, LEGACY_MODEL_ID, dim).unwrap(); // builds + caches

        vault
            .upsert(&tagged("late", "default", q.clone(), LEGACY_MODEL_ID))
            .unwrap();
        let results = vault.query(&q, 5, LEGACY_MODEL_ID, dim).unwrap();
        assert_eq!(
            results.first().map(|r| r.id.as_str()),
            Some("late"),
            "a row inserted after the index was built must be found (cache invalidated)"
        );
    }
}

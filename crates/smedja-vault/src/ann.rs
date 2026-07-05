//! In-process approximate-nearest-neighbour index (IVF-Flat) for the vault.
//!
//! The exact [`Vault::search`](crate::Vault::search) /
//! [`Vault::query`](crate::Vault::query) paths full-scan every row and compute a
//! cosine similarity per row. That is correct but linear: it does not scale past
//! a large `compact`/`warm`/`handoff` namespace. This module adds an
//! *inverted-file* coarse quantiser that partitions the vectors into
//! `≈ sqrt(n)` clusters so a query only compares against a handful of clusters
//! (`nprobe`) rather than the whole store — an `O(sqrt(n))` candidate scan.
//!
//! The index is **candidate-generation only**: it narrows the store down to a
//! small set of ids whose true rows are then fetched and scored by the existing
//! exact cosine/hybrid logic. Ranking fidelity therefore never depends on the
//! approximation — only recall does, which the caller widens with a candidate
//! expansion factor.
//!
//! ## Coexistence with exact search
//!
//! An [`IvfIndex`] is built lazily per `(embedder_model_id, dim)` group, so it
//! is *structurally* same-model: a group only ever contains rows produced by one
//! model at one dimension, exactly the set the same-model filter would keep.
//! Corrupt (non-multiple-of-four) embedding blobs are skipped at build time, the
//! same way the exact path skips them at scan time. Small stores never build an
//! index at all — the [`Vault`](crate::Vault) dispatches to the exact scan below
//! a threshold, so the approximation is only ever engaged where a full scan
//! would actually hurt.
//!
//! ## Determinism
//!
//! Cluster seeding is evenly spaced over the (insertion-ordered) input and the
//! refinement runs a fixed iteration count with lowest-index tie-breaking, so a
//! given set of vectors always produces the same partitioning. There is no RNG.

use std::cell::Cell;
use std::collections::HashMap;

/// Number of Lloyd refinement iterations run while fitting the coarse centroids.
///
/// A handful of passes is enough to settle the partitioning for candidate
/// generation; more would cost build time without improving recall meaningfully.
const KMEANS_ITERS: usize = 8;

/// Default number of clusters probed per query when the caller does not override
/// it. Kept small and constant so the candidate scan stays `O(sqrt(n))`.
pub(crate) const DEFAULT_NPROBE: usize = 8;

/// A single vector held in the index, tagged with its namespace so a
/// namespace-scoped [`Vault::search`](crate::Vault::search) can filter candidates
/// without leaving the index.
struct IvfEntry {
    id: String,
    namespace: String,
    /// L2-normalised copy of the stored embedding, so cosine similarity is a
    /// plain dot product during candidate selection.
    unit: Vec<f32>,
}

/// An IVF-Flat coarse quantiser over one `(model_id, dim)` group.
pub(crate) struct IvfIndex {
    /// L2-normalised cluster centroids.
    centroids: Vec<Vec<f32>>,
    /// Inverted lists: `lists[c]` holds indices into [`IvfIndex::entries`] for the
    /// entries assigned to centroid `c`.
    lists: Vec<Vec<u32>>,
    entries: Vec<IvfEntry>,
    /// Clusters probed per query (capped at the cluster count).
    nprobe: usize,
    /// Number of vector comparisons performed by the most recent
    /// [`IvfIndex::search`] (centroid probes + candidate dot products). Exposed
    /// for tests asserting the query does not degrade into a full scan.
    last_compare_count: Cell<usize>,
}

/// L2-normalises `v` in place; a zero vector is left unchanged (its dot products
/// are all zero, which is the intended "no similarity" outcome).
fn normalise(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-12 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl IvfIndex {
    /// Builds an index over `(id, namespace, embedding)` triples.
    ///
    /// Embeddings are normalised on ingest. `nprobe` is clamped to at least one
    /// and at most the cluster count. An empty input yields an empty index whose
    /// [`search`](IvfIndex::search) always returns nothing.
    pub(crate) fn build(raw: Vec<(String, String, Vec<f32>)>, nprobe: usize) -> Self {
        let entries: Vec<IvfEntry> = raw
            .into_iter()
            .map(|(id, namespace, mut unit)| {
                normalise(&mut unit);
                IvfEntry {
                    id,
                    namespace,
                    unit,
                }
            })
            .collect();

        let n = entries.len();
        if n == 0 {
            return Self {
                centroids: Vec::new(),
                lists: Vec::new(),
                entries,
                nprobe: 1,
                last_compare_count: Cell::new(0),
            };
        }

        // `≈ sqrt(n)` clusters keeps both the centroid probe and the per-list
        // scan on the order of `sqrt(n)`.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation
        )]
        let nlist = ((n as f64).sqrt().ceil() as usize).clamp(1, n);
        let dim = entries[0].unit.len();

        // Deterministic seeding: evenly spaced picks across the input order.
        let mut centroids: Vec<Vec<f32>> = (0..nlist)
            .map(|c| entries[c * n / nlist].unit.clone())
            .collect();

        let mut assignment = vec![0u32; n];
        for _ in 0..KMEANS_ITERS {
            // Assign every entry to its nearest (max-dot) centroid.
            for (i, e) in entries.iter().enumerate() {
                assignment[i] = nearest_centroid(&centroids, &e.unit);
            }
            // Recompute centroids as the normalised mean of their members. An
            // empty cluster keeps its previous centroid so the count is stable.
            let mut sums = vec![vec![0.0f32; dim]; nlist];
            let mut counts = vec![0usize; nlist];
            for (i, e) in entries.iter().enumerate() {
                let c = assignment[i] as usize;
                counts[c] += 1;
                for (s, x) in sums[c].iter_mut().zip(&e.unit) {
                    *s += x;
                }
            }
            for c in 0..nlist {
                if counts[c] > 0 {
                    normalise(&mut sums[c]);
                    centroids[c] = std::mem::take(&mut sums[c]);
                }
            }
        }

        // Final assignment into inverted lists.
        let mut lists: Vec<Vec<u32>> = vec![Vec::new(); nlist];
        for (i, e) in entries.iter().enumerate() {
            let c = nearest_centroid(&centroids, &e.unit) as usize;
            #[allow(clippy::cast_possible_truncation)]
            lists[c].push(i as u32);
        }

        Self {
            centroids,
            lists,
            entries,
            nprobe: nprobe.clamp(1, nlist),
            last_compare_count: Cell::new(0),
        }
    }

    /// Number of indexed vectors.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Comparisons performed by the most recent [`search`](IvfIndex::search).
    #[cfg(test)]
    pub(crate) fn last_compare_count(&self) -> usize {
        self.last_compare_count.get()
    }

    /// Returns up to `limit` candidate ids nearest to `query`, cosine-descending.
    ///
    /// When `namespace` is `Some`, only candidates in that namespace are
    /// returned. The result is a *candidate* set for exact re-ranking by the
    /// caller, not a final ranking.
    pub(crate) fn search(
        &self,
        query: &[f32],
        namespace: Option<&str>,
        limit: usize,
    ) -> Vec<String> {
        if self.entries.is_empty() || limit == 0 {
            self.last_compare_count.set(0);
            return Vec::new();
        }

        let mut q = query.to_vec();
        normalise(&mut q);

        // Probe the `nprobe` nearest centroids.
        let mut centroid_scores: Vec<(usize, f32)> = self
            .centroids
            .iter()
            .enumerate()
            .map(|(c, cen)| (c, dot(&q, cen)))
            .collect();
        let mut compares = self.centroids.len();
        centroid_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        centroid_scores.truncate(self.nprobe);

        // Gather and score candidates from the probed lists.
        let mut scored: Vec<(f32, u32)> = Vec::new();
        for (c, _) in centroid_scores {
            for &idx in &self.lists[c] {
                let e = &self.entries[idx as usize];
                if let Some(ns) = namespace {
                    if e.namespace != ns {
                        continue;
                    }
                }
                compares += 1;
                scored.push((dot(&q, &e.unit), idx));
            }
        }
        self.last_compare_count.set(compares);

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        scored
            .into_iter()
            .map(|(_, idx)| self.entries[idx as usize].id.clone())
            .collect()
    }
}

/// Lazily-built, per-`(model_id, dim)` cache of [`IvfIndex`] instances.
///
/// The [`Vault`](crate::Vault) owns one of these behind a `RefCell`. It is
/// invalidated wholesale on any write: the vault is a cold, read-mostly store, so
/// a full rebuild on the next read is far simpler — and less bug-prone — than
/// incrementally patching inverted lists, and it can never drift from the table.
#[derive(Default)]
pub(crate) struct IndexCache {
    map: HashMap<(String, usize), IvfIndex>,
}

impl IndexCache {
    /// Returns the index for `(model_id, dim)` if one has been built.
    pub(crate) fn get(&self, model_id: &str, dim: usize) -> Option<&IvfIndex> {
        self.map.get(&(model_id.to_owned(), dim))
    }

    /// Stores a freshly built index for `(model_id, dim)`.
    pub(crate) fn insert(&mut self, model_id: &str, dim: usize, index: IvfIndex) {
        self.map.insert((model_id.to_owned(), dim), index);
    }

    /// Drops every cached index; called after any write to the table.
    pub(crate) fn clear(&mut self) {
        self.map.clear();
    }
}

/// Index of the centroid with the greatest dot product with `v`, breaking ties
/// toward the lowest index for determinism.
fn nearest_centroid(centroids: &[Vec<f32>], v: &[f32]) -> u32 {
    let mut best = 0u32;
    let mut best_score = f32::NEG_INFINITY;
    for (c, cen) in centroids.iter().enumerate() {
        let s = dot(v, cen);
        if s > best_score {
            best_score = s;
            #[allow(clippy::cast_possible_truncation)]
            {
                best = c as u32;
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic pseudo-random f32 vector generator (splitmix64) so the
    /// tests do not need an RNG dependency.
    struct Rng(u64);
    impl Rng {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            // Map to [-1, 1).
            #[allow(clippy::cast_precision_loss)]
            {
                (z as f64 / u64::MAX as f64) as f32 * 2.0 - 1.0
            }
        }
        fn vec(&mut self, dim: usize) -> Vec<f32> {
            (0..dim).map(|_| self.next_f32()).collect()
        }
    }

    fn brute_force_topk(data: &[(String, Vec<f32>)], query: &[f32], k: usize) -> Vec<String> {
        let mut q = query.to_vec();
        normalise(&mut q);
        let mut scored: Vec<(f32, String)> = data
            .iter()
            .map(|(id, v)| {
                let mut u = v.clone();
                normalise(&mut u);
                (dot(&q, &u), id.clone())
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        scored.truncate(k);
        scored.into_iter().map(|(_, id)| id).collect()
    }

    #[test]
    fn empty_index_returns_nothing() {
        let idx = IvfIndex::build(Vec::new(), DEFAULT_NPROBE);
        assert_eq!(idx.len(), 0);
        assert!(idx.search(&[1.0, 0.0, 0.0], None, 5).is_empty());
    }

    #[test]
    fn build_is_deterministic() {
        let mut rng = Rng(42);
        let raw: Vec<(String, String, Vec<f32>)> = (0..200)
            .map(|i| (format!("id{i}"), "ns".to_owned(), rng.vec(16)))
            .collect();
        let a = IvfIndex::build(raw.clone(), DEFAULT_NPROBE);
        let b = IvfIndex::build(raw, DEFAULT_NPROBE);
        let q = {
            let mut r = Rng(7);
            r.vec(16)
        };
        assert_eq!(
            a.search(&q, None, 10),
            b.search(&q, None, 10),
            "the same input must produce the same candidate ordering"
        );
    }

    #[test]
    fn full_recall_matches_exact_top_k() {
        // With nprobe == nlist the IVF scans every list, so its candidate set is
        // the whole store and its top-k is *exactly* the brute-force top-k.
        let mut rng = Rng(123);
        let dim = 24;
        let raw: Vec<(String, String, Vec<f32>)> = (0..300)
            .map(|i| (format!("id{i}"), "ns".to_owned(), rng.vec(dim)))
            .collect();
        let flat: Vec<(String, Vec<f32>)> = raw
            .iter()
            .map(|(id, _, v)| (id.clone(), v.clone()))
            .collect();

        // nprobe far above any plausible nlist (~18) forces a full-recall scan.
        let idx = IvfIndex::build(raw, 100_000);
        for seed in [1u64, 2, 3, 99] {
            let q = Rng(seed).vec(dim);
            let ann = idx.search(&q, None, 10);
            let exact = brute_force_topk(&flat, &q, 10);
            assert_eq!(
                ann, exact,
                "full-recall IVF must match brute-force exact top-k for seed {seed}"
            );
        }
    }

    #[test]
    fn default_nprobe_scales_without_full_scan() {
        let mut rng = Rng(555);
        let dim = 32;
        let n = 4000;
        let raw: Vec<(String, String, Vec<f32>)> = (0..n)
            .map(|i| (format!("id{i}"), "ns".to_owned(), rng.vec(dim)))
            .collect();
        let idx = IvfIndex::build(raw, DEFAULT_NPROBE);

        let q = Rng(1).vec(dim);
        let hits = idx.search(&q, None, 10);
        assert_eq!(
            hits.len(),
            10,
            "must still return a full page of candidates"
        );

        // The whole point: a query must not touch every vector.
        let compares = idx.last_compare_count();
        assert!(
            compares < n / 2,
            "IVF must avoid a full scan: compared {compares} of {n} vectors"
        );
    }

    #[test]
    fn recovers_planted_nearest_neighbour() {
        // A vector identical to a stored one must come back as the top candidate
        // under the default (approximate) nprobe: its own centroid is the nearest
        // centroid to the query, so its list is always probed.
        let mut rng = Rng(9);
        let dim = 20;
        let mut raw: Vec<(String, String, Vec<f32>)> = (0..1000)
            .map(|i| (format!("id{i}"), "ns".to_owned(), rng.vec(dim)))
            .collect();
        let planted = rng.vec(dim);
        raw.push(("planted".to_owned(), "ns".to_owned(), planted.clone()));

        let idx = IvfIndex::build(raw, DEFAULT_NPROBE);
        let hits = idx.search(&planted, None, 5);
        assert_eq!(
            hits.first().map(String::as_str),
            Some("planted"),
            "the exact-duplicate vector must be recovered as the top candidate"
        );
    }

    #[test]
    fn namespace_filter_excludes_other_namespaces() {
        let mut rng = Rng(77);
        let dim = 12;
        let mut raw: Vec<(String, String, Vec<f32>)> = (0..600)
            .map(|i| {
                let ns = if i % 2 == 0 { "warm" } else { "cold" };
                (format!("id{i}"), ns.to_owned(), rng.vec(dim))
            })
            .collect();
        let target = rng.vec(dim);
        raw.push(("warm-target".to_owned(), "warm".to_owned(), target.clone()));

        let idx = IvfIndex::build(raw, 100_000); // full recall for a clean assertion
        let hits = idx.search(&target, Some("warm"), 50);
        assert!(
            hits.iter().all(|id| id != "id1"),
            "a cold-namespace id must never appear in a warm-scoped search"
        );
        assert!(
            hits.contains(&"warm-target".to_owned()),
            "the warm target must be found under the warm namespace filter"
        );
    }
}

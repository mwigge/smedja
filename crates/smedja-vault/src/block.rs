//! Live shared memory blocks — Letta-style, concurrently-editable blocks.
//!
//! A *shared block* is a named, durable coordination surface that several agents
//! (e.g. the roles of a `task.parallel` fan-out) read from and append to *during*
//! a run, rather than only pulling a snapshot taken at spawn time. Every segment
//! is a row in the [`SHARED_BLOCK_NAMESPACE`] namespace of the same
//! [`Vault`](crate::Vault) that backs the rest of cold memory, so blocks are
//! persisted and survive a restart.
//!
//! Two write paths, mirroring Letta's block semantics:
//!
//! * **Additive append** ([`Vault::block_append`]) — every call adds a new,
//!   uniquely-keyed segment. Appends never overwrite one another, so concurrent
//!   contributors cannot clobber each other's writes; the block is an append-only
//!   log that every reader sees in full. This is the concurrency-safe default.
//! * **Owned rewrite** ([`Vault::block_rewrite`]) — replaces the block's single
//!   *canonical* segment (last writer wins). Use it for the one authoritative
//!   value a designated owner maintains, distinct from the append log.
//!
//! Reads ([`Vault::block_read`]) return every segment of a block ordered by
//! creation time, so a late-joining agent immediately sees the whole
//! conversation-so-far. Reads are keyed by `block_id` (an id-prefix range scan),
//! never by vector similarity, so recall does not depend on a good query vector.

use crate::error::VaultError;
use crate::vault::{now_secs, Vault, VaultEntry};

/// Namespace holding every shared memory block segment.
///
/// A single namespace holds all blocks; individual blocks are separated by an
/// `id` prefix (`"{block_id}::…"`), so a block is read with an id-range scan.
pub const SHARED_BLOCK_NAMESPACE: &str = "shared_block";

/// Which write path produced a [`BlockSegment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSegmentKind {
    /// A concurrency-safe append-log entry ([`Vault::block_append`]).
    Append,
    /// The single owner-maintained canonical value ([`Vault::block_rewrite`]).
    Canonical,
}

impl BlockSegmentKind {
    /// The string tag persisted in the segment payload.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Canonical => "canonical",
        }
    }
}

/// One segment of a shared memory block.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockSegment {
    /// Full row id (`"{block_id}::a…"` for appends, `"{block_id}::canonical"`).
    pub id: String,
    /// The block this segment belongs to.
    pub block_id: String,
    /// Free-text identifier of the agent/role that wrote the segment.
    pub author: String,
    /// The segment body.
    pub content: String,
    /// Whether this is an append-log entry or the canonical value.
    pub kind: BlockSegmentKind,
    /// Unix timestamp (seconds) the segment was written.
    pub created_at: f64,
}

/// Suffix (relative to `"{block_id}::"`) of the single canonical segment.
const CANONICAL_SUFFIX: &str = "canonical";

/// Returns the `[lo, hi)` id range that contains every segment of `block_id`.
///
/// `'::'` separates the block id from the per-segment suffix; `';'` is the byte
/// immediately after `':'`, so `[ "{id}::", "{id}:;" )` captures `"{id}::*"`
/// without a `LIKE` pattern (no wildcard-escaping hazard on the block id).
fn block_id_range(block_id: &str) -> (String, String) {
    (format!("{block_id}::"), format!("{block_id}:;"))
}

impl Vault {
    /// Appends a new segment to the shared block `block_id` (append-only).
    ///
    /// Every call adds a distinct row — appends never overwrite one another — so
    /// concurrent contributors coordinating through one block cannot clobber each
    /// other's writes. The new segment's sequence number is derived from the
    /// current append count, which is unique under the `&mut self` borrow that
    /// every call site already serialises (the vault runs behind one mutex).
    ///
    /// The row is a normal [`VaultEntry`] written via [`Vault::upsert`], so it is
    /// durable and (optionally) vector-searchable; block reads, however, key on
    /// `block_id` rather than similarity.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] on a database failure or [`VaultError::Json`] if
    /// the payload cannot be serialised.
    #[must_use = "check the Result to confirm the append was persisted"]
    pub fn block_append(
        &mut self,
        block_id: &str,
        author: &str,
        content: &str,
        embedding: Vec<f32>,
        model_id: &str,
        dim: usize,
    ) -> Result<BlockSegment, VaultError> {
        let seq = self.block_append_count(block_id)?;
        let id = format!("{block_id}::a{seq:012}");
        self.write_segment(
            &id,
            block_id,
            author,
            content,
            BlockSegmentKind::Append,
            embedding,
            model_id,
            dim,
        )
    }

    /// Rewrites the single canonical segment of `block_id` (last writer wins).
    ///
    /// Unlike [`Vault::block_append`], this targets one fixed row id, so a
    /// designated owner can maintain a single authoritative value that replaces
    /// its previous contents in place. The append log is untouched.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] on a database failure or [`VaultError::Json`] if
    /// the payload cannot be serialised.
    #[must_use = "check the Result to confirm the rewrite was persisted"]
    pub fn block_rewrite(
        &mut self,
        block_id: &str,
        author: &str,
        content: &str,
        embedding: Vec<f32>,
        model_id: &str,
        dim: usize,
    ) -> Result<BlockSegment, VaultError> {
        let id = format!("{block_id}::{CANONICAL_SUFFIX}");
        self.write_segment(
            &id,
            block_id,
            author,
            content,
            BlockSegmentKind::Canonical,
            embedding,
            model_id,
            dim,
        )
    }

    /// Reads every segment of the shared block `block_id`, oldest first.
    ///
    /// Returns the full block — the canonical value (if any) plus every append —
    /// so a late-joining reader sees the whole coordination log. Ordering is by
    /// `created_at` then `id` for a stable, reproducible read.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] on a database failure.
    #[must_use = "the block segments are the entire purpose of calling this function"]
    pub fn block_read(&self, block_id: &str) -> Result<Vec<BlockSegment>, VaultError> {
        let (lo, hi) = block_id_range(block_id);
        let mut stmt = self.conn.prepare(
            "SELECT id, content, added_by, created_at \
             FROM vault_entries \
             WHERE namespace = ?1 AND id >= ?2 AND id < ?3 \
             ORDER BY created_at ASC, id ASC",
        )?;
        let block_owned = block_id.to_owned();
        let rows = stmt
            .query_map(rusqlite::params![SHARED_BLOCK_NAMESPACE, lo, hi], |row| {
                let id: String = row.get(0)?;
                let content: String = row.get(1)?;
                let author: Option<String> = row.get(2)?;
                let created_at: f64 = row.get(3)?;
                let kind = if id.ends_with(&format!("::{CANONICAL_SUFFIX}")) {
                    BlockSegmentKind::Canonical
                } else {
                    BlockSegmentKind::Append
                };
                Ok(BlockSegment {
                    id,
                    block_id: block_owned.clone(),
                    author: author.unwrap_or_default(),
                    content,
                    kind,
                    created_at,
                })
            })?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        Ok(rows)
    }

    /// Returns the number of append segments already stored for `block_id`.
    ///
    /// Append ids are `"{block_id}::a…"`; the canonical id (`"::canonical"`) sorts
    /// outside `["::a", "::b")`, so it is excluded from the count.
    fn block_append_count(&self, block_id: &str) -> Result<usize, VaultError> {
        let lo = format!("{block_id}::a");
        let hi = format!("{block_id}::b");
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM vault_entries \
             WHERE namespace = ?1 AND id >= ?2 AND id < ?3",
            rusqlite::params![SHARED_BLOCK_NAMESPACE, lo, hi],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// Persists one segment row and returns the [`BlockSegment`] view of it.
    ///
    /// Uses [`Vault::upsert`] (not [`Vault::insert`]) deliberately: append-log
    /// segments must never be dropped by near-duplicate dedup, and the canonical
    /// row must replace in place by id.
    #[allow(clippy::too_many_arguments)]
    fn write_segment(
        &mut self,
        id: &str,
        block_id: &str,
        author: &str,
        content: &str,
        kind: BlockSegmentKind,
        embedding: Vec<f32>,
        model_id: &str,
        dim: usize,
    ) -> Result<BlockSegment, VaultError> {
        let created_at = now_secs();
        let entry = VaultEntry {
            id: id.to_owned(),
            embedding,
            payload: serde_json::json!({
                "block_id": block_id,
                "author": author,
                "kind": kind.as_str(),
            }),
            namespace: SHARED_BLOCK_NAMESPACE.to_owned(),
            content: content.to_owned(),
            source_file: None,
            added_by: Some(author.to_owned()),
            chunk_index: None,
            parent_id: None,
            created_at,
            embedder_model_id: model_id.to_owned(),
            dim,
        };
        self.upsert(&entry)?;
        Ok(BlockSegment {
            id: id.to_owned(),
            block_id: block_id.to_owned(),
            author: author.to_owned(),
            content: content.to_owned(),
            kind,
            created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Vault;

    fn emb() -> Vec<f32> {
        vec![0.1_f32, 0.2, 0.3]
    }

    #[test]
    fn append_then_read_returns_the_segment() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault
            .block_append("fan-1", "impl", "started work", emb(), "m", 3)
            .unwrap();
        let segs = vault.block_read("fan-1").unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].content, "started work");
        assert_eq!(segs[0].author, "impl");
        assert_eq!(segs[0].kind, BlockSegmentKind::Append);
    }

    #[test]
    fn two_agents_appending_one_block_both_see_each_other() {
        // The core shared-block guarantee: additive appends from two distinct
        // authors are both durable and both visible to every reader.
        let mut vault = Vault::open_in_memory().unwrap();
        vault
            .block_append("fan-1", "impl", "impl: parser done", emb(), "m", 3)
            .unwrap();
        vault
            .block_append("fan-1", "test", "test: added cases", emb(), "m", 3)
            .unwrap();

        let segs = vault.block_read("fan-1").unwrap();
        let bodies: Vec<&str> = segs.iter().map(|s| s.content.as_str()).collect();
        assert!(
            bodies.contains(&"impl: parser done"),
            "reader must see impl's append; got {bodies:?}"
        );
        assert!(
            bodies.contains(&"test: added cases"),
            "reader must see test's append; got {bodies:?}"
        );
        assert_eq!(segs.len(), 2, "no append may clobber another");
    }

    #[test]
    fn appends_get_distinct_ids() {
        let mut vault = Vault::open_in_memory().unwrap();
        let a = vault.block_append("b", "x", "one", emb(), "m", 3).unwrap();
        let b = vault.block_append("b", "x", "two", emb(), "m", 3).unwrap();
        assert_ne!(a.id, b.id, "each append must get a unique id");
    }

    #[test]
    fn rewrite_replaces_canonical_in_place() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault
            .block_rewrite("b", "owner", "v1", emb(), "m", 3)
            .unwrap();
        vault
            .block_rewrite("b", "owner", "v2", emb(), "m", 3)
            .unwrap();

        let segs = vault.block_read("b").unwrap();
        let canon: Vec<&BlockSegment> = segs
            .iter()
            .filter(|s| s.kind == BlockSegmentKind::Canonical)
            .collect();
        assert_eq!(canon.len(), 1, "only one canonical segment may exist");
        assert_eq!(canon[0].content, "v2", "owner rewrite is last-writer-wins");
    }

    #[test]
    fn append_and_canonical_coexist() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault
            .block_rewrite("b", "owner", "canonical value", emb(), "m", 3)
            .unwrap();
        vault
            .block_append("b", "worker", "log line", emb(), "m", 3)
            .unwrap();
        let segs = vault.block_read("b").unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(
            segs.iter()
                .filter(|s| s.kind == BlockSegmentKind::Canonical)
                .count(),
            1
        );
        assert_eq!(
            segs.iter()
                .filter(|s| s.kind == BlockSegmentKind::Append)
                .count(),
            1
        );
    }

    #[test]
    fn blocks_are_isolated_by_id() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault
            .block_append("block-a", "x", "in a", emb(), "m", 3)
            .unwrap();
        vault
            .block_append("block-b", "x", "in b", emb(), "m", 3)
            .unwrap();
        let a = vault.block_read("block-a").unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].content, "in a");
        let b = vault.block_read("block-b").unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].content, "in b");
    }

    #[test]
    fn read_unknown_block_is_empty() {
        let vault = Vault::open_in_memory().unwrap();
        assert!(vault.block_read("nope").unwrap().is_empty());
    }
}

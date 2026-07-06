//! [`Vault`] — vector KV cold store backed by `SQLite`.

use crate::error::VaultError;

/// Default model identifier assumed for rows that predate per-row model tagging.
///
/// Legacy rows were all produced by the FNV-1a bag-of-words embedder, so a row
/// whose `embedder_model_id` column is absent or NULL reads back as this id.
pub const LEGACY_MODEL_ID: &str = "fnv-bow-128";

/// A single entry stored in the vault.
#[derive(Debug, Clone, PartialEq)]
pub struct VaultEntry {
    /// Unique identifier for the entry.
    pub id: String,
    /// Embedding vector stored as raw `f32` components.
    pub embedding: Vec<f32>,
    /// Arbitrary JSON payload associated with the entry.
    pub payload: serde_json::Value,
    /// Namespace grouping for the entry. Defaults to `"default"` when empty.
    pub namespace: String,
    /// Raw text content used for keyword boosting and deduplication.
    pub content: String,
    /// Optional path to the source file that produced this entry.
    pub source_file: Option<String>,
    /// Optional identifier of the agent or process that inserted this entry.
    pub added_by: Option<String>,
    /// Position of this chunk within its parent document.
    pub chunk_index: Option<i64>,
    /// Identifier of the parent entry when this entry is a chunk.
    pub parent_id: Option<String>,
    /// Unix timestamp (seconds since epoch) when the entry was created.
    ///
    /// Set to `0.0` on construction; [`Vault::insert`] fills in the current
    /// wall-clock time when the stored value is `0.0`.
    pub created_at: f64,
    /// Identifier of the embedding model that produced [`VaultEntry::embedding`].
    ///
    /// Persisted alongside the embedding so [`Vault::search`]/[`Vault::query`]
    /// compare only same-model vectors. Legacy rows lacking this column read
    /// back as [`LEGACY_MODEL_ID`].
    pub embedder_model_id: String,
    /// Dimension of [`VaultEntry::embedding`] as reported by its producing model.
    ///
    /// Legacy rows lacking this column derive it from the stored BLOB length
    /// divided by four (the byte width of an `f32`).
    pub dim: usize,
}

/// A single result returned by [`Vault::query`].
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Identifier matching a [`VaultEntry::id`].
    pub id: String,
    /// Cosine similarity score in `[0.0, 1.0]`.
    pub score: f32,
    /// The payload from the matching [`VaultEntry`].
    pub payload: serde_json::Value,
}

/// A single diary entry stored in the vault.
#[derive(Debug, Clone, PartialEq)]
pub struct DiaryEntry {
    /// Auto-incremented row identifier.
    pub id: i64,
    /// Role that wrote the diary entry (e.g. `"coder"`, `"reviewer"`).
    pub role: String,
    /// Free-text body of the diary entry.
    pub entry: String,
    /// Unix timestamp (seconds since epoch) when the entry was created.
    pub created_at: f64,
}

/// Identity of the embedding model whose vectors are stored in this vault.
///
/// Once set, [`Vault::insert`] rejects embeddings whose dimension does not
/// match `dimensions`.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedderIdentity {
    /// Name or identifier of the embedding model.
    pub model: String,
    /// Number of dimensions produced by the model.
    pub dimensions: usize,
}

/// Vector KV cold store.
///
/// Embeddings are stored in `SQLite` as little-endian `f32` BLOBs. Retrieval
/// performs a full scan and returns the top-K entries by cosine similarity.
///
/// All operations are synchronous; callers inside an async runtime should use
/// [`tokio::task::spawn_blocking`] to avoid blocking the executor thread.
pub struct Vault {
    pub(crate) conn: rusqlite::Connection,
    /// Lazily-built ANN indices, one per `(embedder_model_id, dim)` group.
    ///
    /// Behind a `RefCell` so the read paths ([`Vault::search`]/[`Vault::query`])
    /// can build and cache an index through `&self`. Invalidated on every write
    /// via [`Vault::invalidate_index`]. `Vault` is already `!Sync` (it owns a
    /// `rusqlite::Connection`) and runs behind a mutex at every call site, so the
    /// interior mutability adds no new sharing hazard.
    pub(crate) index_cache: std::cell::RefCell<crate::ann::IndexCache>,
}

/// A fully-hydrated row read during [`Vault::search`].
///
/// The whole row set is materialised before scoring so the prepared statement is
/// dropped before the same-model filter and cosine math run.
pub(crate) struct SearchRow {
    pub(crate) id: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) payload_str: String,
    pub(crate) ns: String,
    pub(crate) content: String,
    pub(crate) source_file: Option<String>,
    pub(crate) added_by: Option<String>,
    pub(crate) chunk_index: Option<i64>,
    pub(crate) parent_id: Option<String>,
    pub(crate) created_at: f64,
    pub(crate) model_id: Option<String>,
    pub(crate) dim: Option<i64>,
}

/// Returns the current Unix time as a floating-point number of seconds.
pub(crate) fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Resolves the stored `embedder_model_id`, defaulting a legacy NULL to
/// [`LEGACY_MODEL_ID`].
pub(crate) fn resolve_model_id(stored: Option<String>) -> String {
    stored.unwrap_or_else(|| LEGACY_MODEL_ID.to_owned())
}

/// Resolves the stored `dim`, deriving a legacy NULL from the BLOB byte length.
///
/// Embeddings are written as little-endian `f32` BLOBs, so a legacy row's
/// dimension is its byte length divided by four.
pub(crate) fn resolve_dim(stored: Option<i64>, embedding_bytes_len: usize) -> usize {
    match stored {
        Some(d) if d >= 0 => usize::try_from(d).unwrap_or(embedding_bytes_len / 4),
        _ => embedding_bytes_len / 4,
    }
}

/// Decodes a little-endian `f32` embedding BLOB.
///
/// Returns `None` when `bytes.len()` is not a multiple of four — the signature
/// of a truncated, legacy, or externally-corrupted row. Callers skip such rows
/// so a single malformed BLOB cannot turn a full-scan `query`/`search` into a
/// store-wide panic. Decoding byte-by-byte (rather than `bytemuck::cast_slice`)
/// also sidesteps the alignment requirement that `cast_slice` imposes on the
/// borrowed `&[u8]`.
pub(crate) fn decode_embedding(bytes: &[u8]) -> Option<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

impl Vault {
    /// Opens or creates a vault database at `path`.
    ///
    /// Runs schema bootstrap on every open (idempotent via `CREATE TABLE IF NOT EXISTS`).
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database cannot be opened or the
    /// schema bootstrap fails.
    #[must_use = "check the Result; a failed open means the vault is unavailable"]
    pub fn open(path: &std::path::Path) -> Result<Self, VaultError> {
        let conn = rusqlite::Connection::open(path)?;
        let vault = Self {
            conn,
            index_cache: std::cell::RefCell::new(crate::ann::IndexCache::default()),
        };
        vault.migrate()?;
        Ok(vault)
    }

    /// Opens an in-memory vault.
    ///
    /// Useful for tests and ephemeral sessions where durability is not required.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the in-memory connection cannot be
    /// established or the schema bootstrap fails.
    #[must_use = "check the Result; a failed open means the in-memory vault is unavailable"]
    pub fn open_in_memory() -> Result<Self, VaultError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let vault = Self {
            conn,
            index_cache: std::cell::RefCell::new(crate::ann::IndexCache::default()),
        };
        vault.migrate()?;
        Ok(vault)
    }

    /// Applies the database schema (idempotent).
    ///
    /// Creates all tables and then attempts to add any columns that may be
    /// missing from databases created before this migration. The `ALTER TABLE`
    /// calls are executed with errors suppressed — `SQLite` returns an error for
    /// a duplicate column which is the expected outcome on a fully-migrated
    /// database.
    fn migrate(&self) -> Result<(), VaultError> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS vault_entries (
                id           TEXT PRIMARY KEY,
                embedding    BLOB NOT NULL,
                payload      TEXT NOT NULL,
                namespace    TEXT NOT NULL DEFAULT 'default',
                content      TEXT NOT NULL DEFAULT '',
                source_file  TEXT,
                added_by     TEXT,
                chunk_index  INTEGER,
                parent_id    TEXT,
                created_at   REAL NOT NULL DEFAULT 0.0,
                embedder_model_id TEXT,
                dim          INTEGER
            );

            CREATE TABLE IF NOT EXISTS diary (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                role       TEXT NOT NULL,
                entry      TEXT NOT NULL,
                created_at REAL NOT NULL
            );

            CREATE TABLE IF NOT EXISTS vault_meta (
                id       INTEGER PRIMARY KEY,
                meta_key TEXT NOT NULL,
                meta_val TEXT NOT NULL
            );
            ",
        )?;

        // Idempotent column additions for databases created before this migration.
        // SQLite does not support "ADD COLUMN IF NOT EXISTS", so we swallow the
        // "duplicate column name" error that occurs on already-migrated databases.
        for col_def in &[
            "ALTER TABLE vault_entries ADD COLUMN namespace TEXT NOT NULL DEFAULT 'default'",
            "ALTER TABLE vault_entries ADD COLUMN content TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE vault_entries ADD COLUMN source_file TEXT",
            "ALTER TABLE vault_entries ADD COLUMN added_by TEXT",
            "ALTER TABLE vault_entries ADD COLUMN chunk_index INTEGER",
            "ALTER TABLE vault_entries ADD COLUMN parent_id TEXT",
            "ALTER TABLE vault_entries ADD COLUMN created_at REAL NOT NULL DEFAULT 0.0",
            // Per-row model tagging. Nullable on purpose: a NULL marks a legacy
            // row that predates tagging, which reads back as `LEGACY_MODEL_ID`
            // with `dim` derived from the stored BLOB length.
            "ALTER TABLE vault_entries ADD COLUMN embedder_model_id TEXT",
            "ALTER TABLE vault_entries ADD COLUMN dim INTEGER",
        ] {
            let _ = self.conn.execute(col_def, []); // "duplicate column" errors are expected
        }

        Ok(())
    }

    /// Drops every cached ANN index.
    ///
    /// Called after any mutation of `vault_entries` so the next read rebuilds the
    /// index from the current table state. The vault is read-mostly, so a full
    /// rebuild on the next read is cheaper to reason about than incrementally
    /// maintaining inverted lists and cannot drift from the table.
    pub(crate) fn invalidate_index(&mut self) {
        self.index_cache.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, embedding: Vec<f32>) -> VaultEntry {
        let dim = embedding.len();
        VaultEntry {
            id: id.to_string(),
            embedding,
            payload: json!({ "turn": id }),
            namespace: String::new(),
            content: String::new(),
            source_file: None,
            added_by: None,
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
            embedder_model_id: LEGACY_MODEL_ID.to_string(),
            dim,
        }
    }

    // ── basic CRUD ────────────────────────────────────────────────────────────

    #[test]
    fn upsert_and_count() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault.upsert(&entry("a", vec![1.0, 0.0])).unwrap();
        vault.upsert(&entry("b", vec![0.0, 1.0])).unwrap();
        assert_eq!(vault.count().unwrap(), 2);
    }

    #[test]
    fn upsert_replaces_existing() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault.upsert(&entry("a", vec![1.0, 0.0])).unwrap();
        vault.upsert(&entry("a", vec![0.5, 0.5])).unwrap();
        assert_eq!(vault.count().unwrap(), 1);
    }

    #[test]
    fn remove_decrements_count() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault.upsert(&entry("a", vec![1.0, 0.0])).unwrap();
        vault.remove("a").unwrap();
        assert_eq!(vault.count().unwrap(), 0);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut vault = Vault::open_in_memory().unwrap();
        // Should not panic or return an error.
        vault.remove("does-not-exist").unwrap();
        assert_eq!(vault.count().unwrap(), 0);
    }

    // ── query ────────────────────────────────────────────────────────────────

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

    // ── diary ────────────────────────────────────────────────────────────────

    #[test]
    fn diary_write_and_read() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault.diary_write("coder", "first entry").unwrap();
        vault.diary_write("coder", "second entry").unwrap();

        let entries = vault.diary_read("coder").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "coder");
        assert_eq!(entries[0].entry, "first entry");
        assert_eq!(entries[1].entry, "second entry");
        assert!(
            entries[0].created_at <= entries[1].created_at,
            "entries must be returned in ascending created_at order"
        );
    }

    // ── dedup ────────────────────────────────────────────────────────────────

    #[test]
    fn dedup_drops_near_duplicate() {
        let mut vault = Vault::open_in_memory().unwrap();

        // Entry A with longer content.
        let mut a = entry("a", vec![1.0_f32, 0.0]);
        a.content = "this is a longer content string that wins the dedup race".to_string();
        vault.insert(&a).unwrap();
        assert_eq!(vault.count().unwrap(), 1);

        // Entry B with nearly identical embedding but shorter content → should be dropped.
        let mut b = entry("b", vec![0.9999_f32, 0.0141]);
        b.content = "short".to_string();
        vault.insert(&b).unwrap();

        // Only A remains.
        assert_eq!(vault.count().unwrap(), 1);
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

    // ── embedder identity ─────────────────────────────────────────────────────

    #[test]
    fn embedder_mismatch_returns_error() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault
            .set_embedder_identity(&EmbedderIdentity {
                model: "test-model".to_string(),
                dimensions: 2,
            })
            .unwrap();

        // Attempt to insert with 3-dimensional embedding → mismatch.
        let mut e = entry("x", vec![1.0_f32, 0.0, 0.0]);
        e.namespace = "default".to_string();
        let err = vault.insert(&e).unwrap_err();
        assert!(
            matches!(err, VaultError::EmbedderMismatch { .. }),
            "expected EmbedderMismatch, got {err:?}"
        );
    }

    #[test]
    fn embedder_identity_round_trip() {
        let mut vault = Vault::open_in_memory().unwrap();
        assert!(vault.get_embedder_identity().unwrap().is_none());

        vault
            .set_embedder_identity(&EmbedderIdentity {
                model: "text-embedding-3-small".to_string(),
                dimensions: 1536,
            })
            .unwrap();

        let stored = vault.get_embedder_identity().unwrap().unwrap();
        assert_eq!(stored.model, "text-embedding-3-small");
        assert_eq!(stored.dimensions, 1536);
    }

    #[test]
    fn embedder_identity_round_trips_with_special_chars_in_model_name() {
        // A model name containing a quote and a backslash must survive
        // set -> get byte-identically. Before the serde_json fix, the value was
        // interpolated raw into a JSON string literal, producing invalid JSON
        // that get_embedder_identity could never parse (parse error forever).
        let mut vault = Vault::open_in_memory().unwrap();
        let tricky = r#"my"model\with/control"#;
        vault
            .set_embedder_identity(&EmbedderIdentity {
                model: tricky.to_string(),
                dimensions: 384,
            })
            .unwrap();

        let stored = vault
            .get_embedder_identity()
            .expect("stored identity must be valid JSON and parse back")
            .expect("an identity was set, so it must be present");
        assert_eq!(
            stored.model, tricky,
            "model name must round-trip byte-identically"
        );
        assert_eq!(stored.dimensions, 384);
    }

    #[test]
    fn insert_dedup_delete_and_replace_are_atomic() {
        // Insert A, then insert a near-duplicate B (cosine 1.0 here) with LONGER
        // content. That path deletes A and inserts B. Both statements now run in a
        // single transaction, so the vault can never be observed in a state where A
        // has been deleted but B is absent. We assert the committed end state: the
        // vault holds exactly one entry, and it is B (A gone, B present).
        let mut vault = Vault::open_in_memory().unwrap();

        let mut a = entry("a", vec![1.0_f32, 0.0]);
        a.namespace = "default".to_string();
        a.content = "short".to_string();
        vault.insert(&a).unwrap();

        let mut b = entry("b", vec![1.0_f32, 0.0]);
        b.namespace = "default".to_string();
        b.content = "short but a much longer replacement body".to_string();
        vault.insert(&b).unwrap();

        // Exactly one entry survived the delete-then-insert.
        assert_eq!(
            vault.count().unwrap(),
            1,
            "dedup must leave exactly one entry after replacing A with B"
        );

        // The survivor is B, not A. If the delete had committed without the insert
        // (the non-atomic failure mode), this vault would be empty and this search
        // would return nothing.
        let results = vault
            .search(
                &[1.0_f32, 0.0],
                "replacement",
                "default",
                5,
                LEGACY_MODEL_ID,
                2,
            )
            .unwrap();
        assert_eq!(results.len(), 1, "only B must remain");
        assert_eq!(results[0].id, "b", "A must be gone, B must be present");
        assert_eq!(
            results[0].content,
            "short but a much longer replacement body"
        );
    }

    #[test]
    fn count_by_namespace_isolates_entries() {
        let mut vault = Vault::open_in_memory().unwrap();

        let mut e1 = entry("warm1", vec![1.0, 0.0]);
        e1.namespace = "warm".to_string();
        let mut e2 = entry("warm2", vec![0.5, 0.5]);
        e2.namespace = "warm".to_string();
        let mut e3 = entry("cold1", vec![0.0, 1.0]);
        e3.namespace = "default".to_string();

        vault.upsert(&e1).unwrap();
        vault.upsert(&e2).unwrap();
        vault.upsert(&e3).unwrap();

        assert_eq!(vault.count_by_namespace("warm").unwrap(), 2);
        assert_eq!(vault.count_by_namespace("default").unwrap(), 1);
        assert_eq!(vault.count_by_namespace("missing").unwrap(), 0);
        assert_eq!(vault.count().unwrap(), 3);
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
    fn mixed_fnv_and_semantic_store_returns_only_same_model_rows() {
        // A store built with the FNV default (128-dim, `fnv-bow-128`) that is
        // later written to by the semantic default (384-dim, `all-minilm-l6-v2`)
        // must keep both readable: an FNV query returns only the FNV row, and a
        // semantic query only the semantic row — the migration/identity invariant
        // that makes the zero-downtime default swap safe.
        let mut vault = Vault::open_in_memory().unwrap();

        let mut fnv = entry("legacy-fnv", vec![1.0_f32; 128]);
        fnv.namespace = "mem".to_string();
        fnv.content = "shared token".to_string();
        fnv.embedder_model_id = LEGACY_MODEL_ID.to_string(); // fnv-bow-128
        fnv.dim = 128;
        vault.upsert(&fnv).unwrap();

        let mut sem = entry("new-semantic", vec![0.5_f32; 384]);
        sem.namespace = "mem".to_string();
        sem.content = "shared token".to_string();
        sem.embedder_model_id = "all-minilm-l6-v2".to_string();
        sem.dim = 384;
        vault.upsert(&sem).unwrap();

        // FNV query (128-dim): only the pre-existing FNV row is a candidate; the
        // 384-dim semantic row is excluded, never compared, never errors.
        let fnv_hits = vault
            .search(&[1.0_f32; 128], "shared", "mem", 5, LEGACY_MODEL_ID, 128)
            .unwrap();
        assert_eq!(fnv_hits.len(), 1);
        assert_eq!(fnv_hits[0].id, "legacy-fnv");
        assert_eq!(fnv_hits[0].embedder_model_id, LEGACY_MODEL_ID);

        // Semantic query (384-dim): only the new semantic row is returned.
        let sem_hits = vault
            .search(&[0.5_f32; 384], "shared", "mem", 5, "all-minilm-l6-v2", 384)
            .unwrap();
        assert_eq!(sem_hits.len(), 1);
        assert_eq!(sem_hits[0].id, "new-semantic");
        assert_eq!(sem_hits[0].dim, 384);
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

    // ── malformed embedding blobs must not panic the retrieval path ───────────

    /// Inserts a row whose embedding BLOB is 3 bytes — not a multiple of 4 — while
    /// tagging it with a `dim`/`model_id` that passes the same-model filter, so
    /// the row reaches the decode step. Before the fix, `bytemuck::cast_slice::<u8,
    /// f32>` on a 3-byte slice panicked, turning any full-scan `query`/`search`
    /// over the store into a store-wide DoS.
    fn insert_corrupt_row(vault: &Vault, id: &str, namespace: &str, dim: i64) {
        // 3 raw bytes: length is not a multiple of 4.
        let corrupt: Vec<u8> = vec![1, 2, 3];
        vault
            .conn
            .execute(
                "INSERT INTO vault_entries \
                 (id, embedding, payload, namespace, content, embedder_model_id, dim) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    id,
                    corrupt,
                    "{}",
                    namespace,
                    "corrupt",
                    LEGACY_MODEL_ID,
                    dim
                ],
            )
            .unwrap();
    }

    #[test]
    fn query_skips_row_with_non_multiple_of_four_blob() {
        let vault = Vault::open_in_memory().unwrap();
        // dim = 1 matches the query dim below, so the row survives the same-model
        // filter and is handed to the decode step (fail-before panicked here).
        insert_corrupt_row(&vault, "corrupt", "default", 1);

        let results = vault
            .query(&[1.0_f32], 5, LEGACY_MODEL_ID, 1)
            .expect("query must skip the malformed row, not panic");
        assert!(
            results.is_empty(),
            "the corrupt row must be skipped, leaving no results"
        );
    }

    #[test]
    fn search_skips_row_with_non_multiple_of_four_blob() {
        let mut vault = Vault::open_in_memory().unwrap();
        insert_corrupt_row(&vault, "corrupt", "ns", 1);

        // A well-formed same-model row so we can confirm the scan continues past
        // the corrupt one rather than aborting the whole call.
        let mut good = entry("good", vec![1.0_f32]);
        good.namespace = "ns".to_string();
        good.content = "good content".to_string();
        good.dim = 1;
        vault.insert(&good).unwrap();

        let results = vault
            .search(&[1.0_f32], "good", "ns", 5, LEGACY_MODEL_ID, 1)
            .expect("search must skip the malformed row, not panic");
        assert_eq!(results.len(), 1, "only the well-formed row is returned");
        assert_eq!(results[0].id, "good");
    }

    #[test]
    fn insert_dedup_scan_skips_corrupt_neighbour() {
        let mut vault = Vault::open_in_memory().unwrap();
        // A corrupt neighbour in the same namespace as the incoming insert. The
        // dedup scan visits every same-namespace row and previously panicked on
        // this blob before it could persist the new entry.
        insert_corrupt_row(&vault, "corrupt", "default", 1);

        let good = entry("good", vec![1.0_f32]);
        vault
            .insert(&good)
            .expect("insert must skip the corrupt dedup neighbour, not panic");

        let results = vault
            .query(&[1.0_f32], 5, LEGACY_MODEL_ID, 1)
            .expect("query after insert must not panic");
        assert_eq!(results.len(), 1, "the freshly inserted row is present");
        assert_eq!(results[0].id, "good");
    }
}

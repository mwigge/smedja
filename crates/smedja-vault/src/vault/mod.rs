//! [`Vault`] — vector KV cold store backed by `SQLite`.
//!
//! The implementation is split across cohesive submodules:
//! - [`types`] — the public [`VaultEntry`]/[`QueryResult`] data types.
//! - [`entries`] — insert, upsert, remove, listing, and counting.
//! - [`search`] — hybrid [`Vault::search`] and pure-cosine [`Vault::query`].
//! - [`diary`] — append-only role-scoped [`DiaryEntry`] storage.
//! - [`identity`] — the [`EmbedderIdentity`] guard for stored vectors.
//!
//! This module owns the [`Vault`] type itself, the shared row structs and
//! helper functions, and the connection bootstrap / schema migration.

mod diary;
mod entries;
mod identity;
mod query;
mod search;
mod types;

pub use diary::DiaryEntry;
pub use identity::EmbedderIdentity;
pub use types::{QueryResult, VaultEntry, LEGACY_MODEL_ID};

use crate::error::VaultError;

/// Vector KV cold store.
///
/// Embeddings are stored in `SQLite` as little-endian `f32` BLOBs. Retrieval
/// performs a full scan and returns the top-K entries by cosine similarity.
///
/// All operations are synchronous; callers inside an async runtime should use
/// [`tokio::task::spawn_blocking`] to avoid blocking the executor thread.
pub struct Vault {
    conn: rusqlite::Connection,
}

/// A row read during the dedup scan in [`Vault::insert`].
pub(crate) struct DedupeRow {
    pub(crate) id: String,
    pub(crate) embedding: Vec<u8>,
    pub(crate) content_len: usize,
}

/// A row read during [`Vault::query`] before same-model filtering and scoring.
pub(crate) struct QueryRow {
    pub(crate) id: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) payload_str: String,
    pub(crate) model_id: Option<String>,
    pub(crate) dim: Option<i64>,
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
        let vault = Self { conn };
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
        let vault = Self { conn };
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
}

/// Shared test-only constructor for a minimal legacy [`VaultEntry`].
///
/// Lives here (rather than in a test-only module) so every submodule's
/// `#[cfg(test)]` block can build fixtures without duplicating the boilerplate.
#[cfg(test)]
pub(crate) fn entry(id: &str, embedding: Vec<f32>) -> VaultEntry {
    let dim = embedding.len();
    VaultEntry {
        id: id.to_string(),
        embedding,
        payload: serde_json::json!({ "turn": id }),
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

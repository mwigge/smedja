use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::GraphError;
use crate::indexer::{index_directory, index_file_with_lang, lang_and_query_for_ext};
use crate::types::{Symbol, SymbolKind};

/// Persistent store for code-graph symbols.
///
/// Wraps a [`rusqlite::Connection`] and owns all table operations for the
/// `symbols` table.  On construction the schema is bootstrapped via an
/// idempotent `CREATE TABLE IF NOT EXISTS` statement.
pub struct GraphStore {
    conn: Connection,
}

impl GraphStore {
    /// Opens (or creates) a graph database file at `path` and runs schema migrations.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Db`] if the file cannot be opened or migrations fail.
    pub fn open(path: &Path) -> Result<Self, GraphError> {
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Opens an in-memory `SQLite` database and runs schema migrations.
    ///
    /// Useful for tests and ephemeral indexing sessions.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Db`] if the connection cannot be established or
    /// migrations fail.
    pub fn open_in_memory() -> Result<Self, GraphError> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Applies the `symbols` and `indexed_files` table DDL idempotently.
    fn migrate(&self) -> Result<(), GraphError> {
        self.conn.execute_batch(
            "PRAGMA journal_mode = WAL;

             CREATE TABLE IF NOT EXISTS symbols (
                 id           TEXT PRIMARY KEY,
                 workspace_id TEXT NOT NULL,
                 file_path    TEXT NOT NULL,
                 name         TEXT NOT NULL,
                 kind         TEXT NOT NULL,
                 start_line   INTEGER NOT NULL,
                 end_line     INTEGER NOT NULL,
                 snippet      TEXT NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_symbols_name      ON symbols(name);
             CREATE INDEX IF NOT EXISTS idx_symbols_workspace ON symbols(workspace_id);

             CREATE TABLE IF NOT EXISTS indexed_files (
                 workspace_id TEXT    NOT NULL,
                 file_path    TEXT    NOT NULL,
                 indexed_at   REAL    NOT NULL,
                 PRIMARY KEY (workspace_id, file_path)
             );",
        )?;
        Ok(())
    }

    /// Indexes all `.rs` files under `root`, associating them with `workspace_id`.
    ///
    /// Returns the total number of symbols inserted.  Call [`Self::clear_workspace`]
    /// before this method when re-indexing to avoid duplicate symbols.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Io`] on filesystem errors, or [`GraphError::Db`] on
    /// database errors.  [`GraphError::ParseFailed`] is logged and skipped — it is
    /// not propagated so that a single bad file does not abort the whole index run.
    pub fn index_workspace(
        &mut self,
        root: &Path,
        workspace_id: &str,
    ) -> Result<usize, GraphError> {
        index_directory(&self.conn, root, workspace_id)
    }

    /// Incrementally re-indexes source files under `root` for `workspace_id`,
    /// mtime-based (Aider-style) — no commit hash required.
    ///
    /// For every recognised source file:
    /// - A file whose filesystem modification time has **not** advanced beyond
    ///   the stored `indexed_at` timestamp is **skipped** (assumed unchanged).
    /// - A file that is new, or whose mtime is newer than the recorded
    ///   `indexed_at`, is re-indexed: its old symbols are deleted and fresh ones
    ///   inserted, and its `indexed_at` is bumped.
    ///
    /// After the walk, files that were previously indexed but no longer exist on
    /// disk are pruned (their symbols and `indexed_files` rows are removed) so a
    /// deleted file's symbols do not linger.
    ///
    /// Returns the **total** number of symbols stored for `workspace_id` after
    /// the run (so `/index` reports the repo's whole symbol count, not just the
    /// delta from an incremental pass).
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::InvalidRoot`] when `root` is not a directory,
    /// [`GraphError::Io`] on filesystem errors, or [`GraphError::Db`] on database
    /// errors.
    pub fn index_workspace_incremental(
        &mut self,
        root: &Path,
        workspace_id: &str,
    ) -> Result<usize, GraphError> {
        // Fail loudly on a bad root before touching the graph — a
        // nonexistent/relative-bad path must not report Ok and silently prune
        // every previously-indexed file below.
        crate::indexer::validate_root(root)?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        // Files seen on this pass; anything previously indexed but absent here
        // is a deletion and is pruned afterwards.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for entry in crate::indexer::workspace_files(root) {
            let abs_path = entry.path();

            let ext = abs_path.extension().and_then(|s| s.to_str()).unwrap_or("");
            let Some((lang, query_str)) = lang_and_query_for_ext(ext) else {
                continue;
            };

            let rel_path = abs_path.strip_prefix(root).map_or_else(
                |_| abs_path.to_string_lossy().into_owned(),
                |p| p.to_string_lossy().into_owned(),
            );
            seen.insert(rel_path.clone());

            // Filesystem modification time as epoch f64.
            let mtime: f64 = abs_path
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0.0, |d| d.as_secs_f64());

            // When was this file last indexed (regardless of commit)?
            let existing: Option<f64> = self
                .conn
                .query_row(
                    "SELECT indexed_at FROM indexed_files \
                     WHERE workspace_id = ?1 AND file_path = ?2",
                    rusqlite::params![workspace_id, rel_path],
                    |row| row.get(0),
                )
                .ok();

            if let Some(indexed_at) = existing {
                // Skip if the file has not been modified since it was indexed.
                if mtime <= indexed_at {
                    tracing::debug!(
                        file = %rel_path,
                        "incremental: skipping unchanged file"
                    );
                    continue;
                }
            }

            // Remove stale symbols for this file before re-inserting.
            self.conn.execute(
                "DELETE FROM symbols WHERE workspace_id = ?1 AND file_path = ?2",
                rusqlite::params![workspace_id, rel_path],
            )?;

            // Re-index the file.
            let source = std::fs::read_to_string(abs_path)?;
            match index_file_with_lang(
                &self.conn,
                &rel_path,
                &source,
                workspace_id,
                &lang,
                query_str,
            ) {
                Ok(n) => {
                    tracing::debug!(file = %rel_path, symbols = n, "incremental: indexed");
                }
                Err(GraphError::ParseFailed { ref path }) => {
                    tracing::warn!(file = %path, "incremental: parse failed — skipping");
                }
                Err(e) => return Err(e),
            }

            // Record the file as indexed at `now`.
            self.conn.execute(
                "INSERT OR REPLACE INTO indexed_files \
                 (workspace_id, file_path, indexed_at) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![workspace_id, rel_path, now],
            )?;
        }

        self.prune_deleted_files(workspace_id, &seen)?;

        self.symbol_count(workspace_id)
    }

    /// Removes symbols and `indexed_files` rows for files that were previously
    /// indexed under `workspace_id` but are absent from `seen` (deleted on disk).
    fn prune_deleted_files(
        &self,
        workspace_id: &str,
        seen: &std::collections::HashSet<String>,
    ) -> Result<(), GraphError> {
        let previously: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT file_path FROM indexed_files WHERE workspace_id = ?1")?;
            let rows = stmt.query_map(rusqlite::params![workspace_id], |row| {
                row.get::<_, String>(0)
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for file_path in previously {
            if seen.contains(&file_path) {
                continue;
            }
            self.conn.execute(
                "DELETE FROM symbols WHERE workspace_id = ?1 AND file_path = ?2",
                rusqlite::params![workspace_id, file_path],
            )?;
            self.conn.execute(
                "DELETE FROM indexed_files WHERE workspace_id = ?1 AND file_path = ?2",
                rusqlite::params![workspace_id, file_path],
            )?;
        }
        Ok(())
    }

    /// Returns the top-`k` symbols whose name contains `query` (case-insensitive).
    ///
    /// `_depth` is accepted for API compatibility but unused in Phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Db`] if the SELECT fails.
    // ponytail: depth traversal deferred
    pub fn graph_query(
        &self,
        query: &str,
        k: usize,
        _depth: u8,
    ) -> Result<Vec<Symbol>, GraphError> {
        let pattern = format!("%{}%", query.to_lowercase());

        let mut stmt = self.conn.prepare(
            "SELECT id, workspace_id, file_path, name, kind, start_line, end_line, snippet
               FROM symbols
              WHERE LOWER(name) LIKE ?1
              LIMIT ?2",
        )?;

        // k is a collection-size bound — wrapping from usize to i64 only on
        // platforms where usize > i64::MAX, which is not a realistic case.
        #[allow(clippy::cast_possible_wrap)]
        let k_i64 = k as i64;
        let rows = stmt.query_map(rusqlite::params![pattern, k_i64], |row| {
            let kind_str: String = row.get(4)?;
            let kind = SymbolKind::try_from_str(&kind_str).unwrap_or(SymbolKind::Function);
            Ok(Symbol {
                id: row.get(0)?,
                workspace_id: row.get(1)?,
                file_path: row.get(2)?,
                name: row.get(3)?,
                kind,
                start_line: row.get::<_, u32>(5)?,
                end_line: row.get::<_, u32>(6)?,
                snippet: row.get(7)?,
            })
        })?;

        let mut symbols = Vec::with_capacity(k);
        for row in rows {
            symbols.push(row?);
        }
        Ok(symbols)
    }

    /// Removes all symbols belonging to `workspace_id`.
    ///
    /// Call before [`Self::index_workspace`] when re-indexing to prevent
    /// duplicate symbols from accumulating.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Db`] if the DELETE fails.
    pub fn clear_workspace(&mut self, workspace_id: &str) -> Result<(), GraphError> {
        self.conn.execute(
            "DELETE FROM symbols WHERE workspace_id = ?1",
            rusqlite::params![workspace_id],
        )?;
        Ok(())
    }

    /// Returns the number of symbols stored for `workspace_id`.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Db`] if the COUNT query fails.
    pub fn symbol_count(&self, workspace_id: &str) -> Result<usize, GraphError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM symbols WHERE workspace_id = ?1",
            rusqlite::params![workspace_id],
            |row| row.get(0),
        )?;
        // COUNT(*) is never negative; sign loss and truncation are intentional.
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Ok(n as usize)
    }
}

use std::path::Path;

use rusqlite::Connection;

use crate::error::GraphError;
use crate::indexer::index_directory;
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

    /// Applies the `symbols` table DDL idempotently.
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
             CREATE INDEX IF NOT EXISTS idx_symbols_workspace ON symbols(workspace_id);",
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

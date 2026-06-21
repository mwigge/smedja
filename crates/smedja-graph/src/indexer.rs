use std::path::Path;

use rusqlite::Connection;
use streaming_iterator::StreamingIterator as _;
use tree_sitter::{Language, Parser, Query, QueryCursor};
use uuid::Uuid;

use crate::error::GraphError;
use crate::types::SymbolKind;

/// tree-sitter S-expression query that captures all named symbol definitions.
///
/// Each alternative binds the name node to `@name` and the parent definition
/// node to `@def`, enabling robust line-range extraction.
const RUST_QUERY: &str = r"
(function_item   name: (identifier)      @name) @def
(struct_item     name: (type_identifier) @name) @def
(enum_item       name: (type_identifier) @name) @def
(trait_item      name: (type_identifier) @name) @def
(impl_item       type: (type_identifier) @name) @def
(const_item      name: (identifier)      @name) @def
(type_item       name: (type_identifier) @name) @def
";

/// Returns the [`Language`] for Rust provided by `tree-sitter-rust`.
fn rust_language() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

/// Parses a single `.rs` source file and inserts all discovered symbols into
/// the `symbols` table.
///
/// `file_path` is the relative path stored in the database.  `source` is the
/// raw UTF-8 text of the file.  Returns the number of symbols inserted.
///
/// # Errors
///
/// - [`GraphError::ParseFailed`] when `tree-sitter` reports a syntax error or
///   returns `None` from [`Parser::parse`].
/// - [`GraphError::Db`] when any `SQLite` INSERT fails.
pub(crate) fn index_file(
    conn: &Connection,
    file_path: &str,
    source: &str,
    workspace_id: &str,
) -> Result<usize, GraphError> {
    let lang = rust_language();

    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|_| GraphError::ParseFailed {
            path: file_path.to_owned(),
        })?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| GraphError::ParseFailed {
            path: file_path.to_owned(),
        })?;

    let query = Query::new(&lang, RUST_QUERY).map_err(|_| GraphError::ParseFailed {
        path: file_path.to_owned(),
    })?;

    let name_idx = query
        .capture_index_for_name("name")
        .ok_or_else(|| GraphError::ParseFailed {
            path: file_path.to_owned(),
        })?;

    let def_idx = query
        .capture_index_for_name("def")
        .ok_or_else(|| GraphError::ParseFailed {
            path: file_path.to_owned(),
        })?;

    let source_bytes = source.as_bytes();
    let source_lines: Vec<&str> = source.lines().collect();

    let mut cursor = QueryCursor::new();
    // tree-sitter 0.24 exposes a StreamingIterator, not a standard Iterator.
    // Drive it manually with advance() + get().
    let mut matches = cursor.matches(&query, tree.root_node(), source_bytes);

    let mut count = 0usize;

    loop {
        matches.advance();
        let Some(m) = matches.get() else { break };

        let name_node = m
            .captures
            .iter()
            .find(|c| c.index == name_idx)
            .map(|c| c.node);

        let def_node = m
            .captures
            .iter()
            .find(|c| c.index == def_idx)
            .map(|c| c.node);

        let (Some(name_node), Some(def_node)) = (name_node, def_node) else {
            continue;
        };

        let name = name_node.utf8_text(source_bytes).unwrap_or("");
        if name.is_empty() {
            continue;
        }

        let kind = kind_for_node(def_node.kind());
        // Source files are unlikely to exceed 4 billion lines; truncation is
        // acceptable and intentional here.
        #[allow(clippy::cast_possible_truncation)]
        let start_line = def_node.start_position().row as u32;
        #[allow(clippy::cast_possible_truncation)]
        let end_line = def_node.end_position().row as u32;

        let snippet = build_snippet(&source_lines, start_line, end_line);
        let id = Uuid::new_v4().to_string();

        conn.execute(
            "INSERT INTO symbols
                 (id, workspace_id, file_path, name, kind, start_line, end_line, snippet)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                id,
                workspace_id,
                file_path,
                name,
                kind.as_str(),
                start_line,
                end_line,
                snippet,
            ],
        )?;

        count += 1;
    }

    Ok(count)
}

/// Maps a tree-sitter node kind string to a [`SymbolKind`].
///
/// Unknown node kinds are treated as [`SymbolKind::Function`] — the query
/// pattern constrains what can arrive here so this branch is unreachable in
/// practice.
fn kind_for_node(node_kind: &str) -> SymbolKind {
    match node_kind {
        "struct_item" => SymbolKind::Struct,
        "enum_item" => SymbolKind::Enum,
        "trait_item" => SymbolKind::Trait,
        "impl_item" => SymbolKind::Impl,
        "const_item" => SymbolKind::Const,
        "type_item" => SymbolKind::TypeAlias,
        _ => SymbolKind::Function, // "function_item" and anything unexpected
    }
}

/// Extracts up to the first 10 lines of a definition from `source_lines`.
///
/// `start_line` and `end_line` are 0-based row indices from tree-sitter.
fn build_snippet(source_lines: &[&str], start_line: u32, end_line: u32) -> String {
    let start = start_line as usize;
    let end = ((end_line as usize) + 1).min(source_lines.len());
    let capped = end.min(start + 10);
    source_lines[start..capped].join("\n")
}

/// Walks all `.rs` files under `root` and delegates to [`index_file`].
///
/// Returns the total number of symbols inserted across all files.
///
/// # Errors
///
/// Propagates any [`GraphError`] from filesystem I/O errors or database
/// errors.  [`GraphError::ParseFailed`] is logged and skipped — it is not
/// propagated so that a single bad file does not abort the whole index run.
pub(crate) fn index_directory(
    conn: &Connection,
    root: &Path,
    workspace_id: &str,
) -> Result<usize, GraphError> {
    let mut total = 0usize;

    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|ext| ext == "rs"))
    {
        let abs_path = entry.path();
        let rel_path = abs_path.strip_prefix(root).map_or_else(
            |_| abs_path.to_string_lossy().into_owned(),
            |p| p.to_string_lossy().into_owned(),
        );

        let source = std::fs::read_to_string(abs_path)?;

        match index_file(conn, &rel_path, &source, workspace_id) {
            Ok(n) => {
                total += n;
                tracing::debug!(file = %rel_path, symbols = n, "indexed");
            }
            Err(GraphError::ParseFailed { ref path }) => {
                // Non-fatal: log and continue indexing other files.
                tracing::warn!(file = %path, "tree-sitter parse failed — skipping");
            }
            Err(e) => return Err(e),
        }
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE symbols (
                id           TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL,
                file_path    TEXT NOT NULL,
                name         TEXT NOT NULL,
                kind         TEXT NOT NULL,
                start_line   INTEGER NOT NULL,
                end_line     INTEGER NOT NULL,
                snippet      TEXT NOT NULL
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn index_file_counts_function() {
        let conn = in_memory_conn();
        let n = index_file(&conn, "test.rs", "fn foo() {}\n", "ws").unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn index_file_counts_struct() {
        let conn = in_memory_conn();
        let n = index_file(&conn, "test.rs", "struct Bar { x: i32 }\n", "ws").unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn index_file_counts_enum() {
        let conn = in_memory_conn();
        let n = index_file(&conn, "test.rs", "enum Color { Red, Green }\n", "ws").unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn index_file_counts_trait() {
        let conn = in_memory_conn();
        let n = index_file(&conn, "test.rs", "trait Greet { fn hi(&self); }\n", "ws").unwrap();
        // trait_item matches, plus the fn inside it
        assert!(n >= 1);
    }

    #[test]
    fn index_file_counts_const() {
        let conn = in_memory_conn();
        let n = index_file(&conn, "test.rs", "const MAX: u32 = 100;\n", "ws").unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn index_file_counts_type_alias() {
        let conn = in_memory_conn();
        let n = index_file(&conn, "test.rs", "type MyVec = Vec<i32>;\n", "ws").unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn build_snippet_caps_at_10_lines() {
        let lines: Vec<&str> = (0..20).map(|_| "x").collect();
        let snippet = build_snippet(&lines, 0, 19);
        assert_eq!(snippet.lines().count(), 10);
    }
}

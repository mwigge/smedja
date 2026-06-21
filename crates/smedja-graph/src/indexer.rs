use std::path::Path;

use rusqlite::Connection;
use streaming_iterator::StreamingIterator as _;
use tree_sitter::{Language, Parser, Query, QueryCursor};
use uuid::Uuid;

use crate::error::GraphError;
use crate::types::SymbolKind;

/// tree-sitter query for Rust source files.
///
/// Captures function items, struct/enum/trait/impl definitions, constants,
/// and type aliases.  Each alternative binds the name node to `@name` and
/// the enclosing definition node to `@def`.
const RUST_QUERY: &str = r"
(function_item   name: (identifier)      @name) @def
(struct_item     name: (type_identifier) @name) @def
(enum_item       name: (type_identifier) @name) @def
(trait_item      name: (type_identifier) @name) @def
(impl_item       type: (type_identifier) @name) @def
(const_item      name: (identifier)      @name) @def
(type_item       name: (type_identifier) @name) @def
";

/// tree-sitter query for Go source files.
///
/// Captures top-level function and method declarations.
const GO_QUERY: &str = r"
(function_declaration name: (identifier)   @name) @def
(method_declaration   name: (field_identifier) @name) @def
";

/// tree-sitter query for Python source files.
///
/// Captures function and class definitions.
const PYTHON_QUERY: &str = r"
(function_definition name: (identifier) @name) @def
(class_definition    name: (identifier) @name) @def
";

/// tree-sitter query for TypeScript/TSX source files.
///
/// Captures function declarations, class declarations, and interface
/// declarations.
const TYPESCRIPT_QUERY: &str = r"
(function_declaration name: (identifier)      @name) @def
(class_declaration    name: (type_identifier) @name) @def
(interface_declaration name: (type_identifier) @name) @def
";

/// File extensions that the indexer recognises, paired with their language
/// and query string.
///
/// Returning `None` for an extension means the file is silently skipped.
pub(crate) fn lang_and_query_for_ext(ext: &str) -> Option<(Language, &'static str)> {
    match ext {
        "rs" => Some((tree_sitter_rust::LANGUAGE.into(), RUST_QUERY)),
        "go" => Some((tree_sitter_go::LANGUAGE.into(), GO_QUERY)),
        "py" => Some((tree_sitter_python::LANGUAGE.into(), PYTHON_QUERY)),
        "ts" | "tsx" => Some((
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            TYPESCRIPT_QUERY,
        )),
        _ => None,
    }
}

/// Parses a single Rust (`.rs`) source file and inserts all discovered symbols
/// into the `symbols` table.
///
/// Convenience wrapper around [`index_file_with_lang`] that selects the Rust
/// grammar and query automatically.  `file_path` is the relative path stored
/// in the database.  `source` is the raw UTF-8 text of the file.  Returns the
/// number of symbols inserted.
///
/// # Errors
///
/// - [`GraphError::ParseFailed`] when `tree-sitter` reports a syntax error or
///   returns `None` from [`Parser::parse`].
/// - [`GraphError::Db`] when any `SQLite` INSERT fails.
#[cfg(test)]
pub(crate) fn index_file(
    conn: &Connection,
    file_path: &str,
    source: &str,
    workspace_id: &str,
) -> Result<usize, GraphError> {
    let lang: Language = tree_sitter_rust::LANGUAGE.into();
    index_file_with_lang(conn, file_path, source, workspace_id, &lang, RUST_QUERY)
}

/// Parses a source file with an explicit [`Language`] and query string and
/// inserts all discovered symbols into the `symbols` table.
///
/// Used by [`index_directory`] to dispatch the correct grammar per file
/// extension.  Returns the number of symbols inserted, or 0 when the query
/// compiles but matches nothing.
///
/// # Errors
///
/// - [`GraphError::ParseFailed`] when tree-sitter fails to parse or the query
///   string is invalid.
/// - [`GraphError::Db`] on any `SQLite` INSERT failure.
pub(crate) fn index_file_with_lang(
    conn: &Connection,
    file_path: &str,
    source: &str,
    workspace_id: &str,
    lang: &Language,
    query_str: &str,
) -> Result<usize, GraphError> {
    let mut parser = Parser::new();
    parser
        .set_language(lang)
        .map_err(|_| GraphError::ParseFailed {
            path: file_path.to_owned(),
        })?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| GraphError::ParseFailed {
            path: file_path.to_owned(),
        })?;

    // If the query fails to compile (e.g. a node type that doesn't exist in
    // this grammar version) treat it as a parse failure — the file is skipped,
    // not the whole directory walk.
    let Ok(query) = Query::new(lang, query_str) else {
        return Err(GraphError::ParseFailed {
            path: file_path.to_owned(),
        });
    };

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
/// patterns constrain what can arrive here and function-like constructs are
/// the most common match.
fn kind_for_node(node_kind: &str) -> SymbolKind {
    match node_kind {
        // Rust
        "enum_item" => SymbolKind::Enum,
        "impl_item" => SymbolKind::Impl,
        "const_item" => SymbolKind::Const,
        "type_item" => SymbolKind::TypeAlias,
        // Struct-like: Rust structs, Python/TypeScript classes
        "struct_item" | "class_definition" | "class_declaration" => SymbolKind::Struct,
        // Trait-like: Rust traits, TypeScript interfaces
        "trait_item" | "interface_declaration" => SymbolKind::Trait,
        // Everything else (function_item, function_declaration, method_declaration,
        // function_definition) is treated as a function.
        _ => SymbolKind::Function,
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

/// Walks source files under `root`, detects language by extension, and
/// delegates to [`index_file_with_lang`].
///
/// Recognised extensions: `.rs`, `.go`, `.py`, `.ts`, `.tsx`.
/// All other file extensions are silently skipped.
///
/// Returns the total number of symbols inserted across all files.
///
/// # Errors
///
/// Propagates any [`GraphError`] from filesystem I/O or database errors.
/// [`GraphError::ParseFailed`] is logged and skipped — it is not propagated
/// so that a single bad file does not abort the whole index run.
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
        .filter(|e| e.file_type().is_file())
    {
        let abs_path = entry.path();
        let ext = abs_path.extension().and_then(|s| s.to_str()).unwrap_or("");

        let Some((lang, query_str)) = lang_and_query_for_ext(ext) else {
            continue;
        };

        let rel_path = abs_path.strip_prefix(root).map_or_else(
            |_| abs_path.to_string_lossy().into_owned(),
            |p| p.to_string_lossy().into_owned(),
        );

        let source = std::fs::read_to_string(abs_path)?;

        match index_file_with_lang(conn, &rel_path, &source, workspace_id, &lang, query_str) {
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

    // ── Rust ──────────────────────────────────────────────────────────────────

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

    // ── Go ────────────────────────────────────────────────────────────────────

    #[test]
    fn index_go_file_finds_function() {
        let conn = in_memory_conn();
        let source = "package main\n\nfunc Hello() string { return \"hi\" }\n";
        let lang = tree_sitter_go::LANGUAGE.into();
        let n = index_file_with_lang(&conn, "main.go", source, "ws-go", &lang, GO_QUERY).unwrap();
        assert!(n >= 1, "expected ≥ 1 Go symbol, got {n}");
    }

    #[test]
    fn index_go_method_finds_method() {
        let conn = in_memory_conn();
        let source = "package main\ntype T struct{}\nfunc (t T) Run() error { return nil }\n";
        let lang = tree_sitter_go::LANGUAGE.into();
        let n =
            index_file_with_lang(&conn, "method.go", source, "ws-go-m", &lang, GO_QUERY).unwrap();
        assert!(n >= 1, "expected ≥ 1 Go method symbol, got {n}");
    }

    // ── Python ────────────────────────────────────────────────────────────────

    #[test]
    fn index_python_file_finds_function() {
        let conn = in_memory_conn();
        let source = "def greet(name):\n    return f\"hello {name}\"\n";
        let lang = tree_sitter_python::LANGUAGE.into();
        let n =
            index_file_with_lang(&conn, "hello.py", source, "ws-py", &lang, PYTHON_QUERY).unwrap();
        assert!(n >= 1, "expected ≥ 1 Python symbol, got {n}");
    }

    #[test]
    fn index_python_file_finds_class() {
        let conn = in_memory_conn();
        let source = "class MyService:\n    pass\n";
        let lang = tree_sitter_python::LANGUAGE.into();
        let n = index_file_with_lang(
            &conn,
            "service.py",
            source,
            "ws-py-class",
            &lang,
            PYTHON_QUERY,
        )
        .unwrap();
        assert!(n >= 1, "expected ≥ 1 Python class symbol, got {n}");
    }

    // ── TypeScript ────────────────────────────────────────────────────────────

    #[test]
    fn index_typescript_file_finds_function() {
        let conn = in_memory_conn();
        let source = "function add(a: number, b: number): number { return a + b; }\n";
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        let n = index_file_with_lang(&conn, "add.ts", source, "ws-ts", &lang, TYPESCRIPT_QUERY)
            .unwrap();
        assert!(n >= 1, "expected ≥ 1 TypeScript symbol, got {n}");
    }

    #[test]
    fn index_typescript_file_finds_class() {
        let conn = in_memory_conn();
        let source = "class Agent { run(): void {} }\n";
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        let n = index_file_with_lang(
            &conn,
            "agent.ts",
            source,
            "ws-ts-class",
            &lang,
            TYPESCRIPT_QUERY,
        )
        .unwrap();
        assert!(n >= 1, "expected ≥ 1 TypeScript class symbol, got {n}");
    }

    #[test]
    fn index_typescript_file_finds_interface() {
        let conn = in_memory_conn();
        let source = "interface Runner { run(): void; }\n";
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        let n = index_file_with_lang(
            &conn,
            "runner.ts",
            source,
            "ws-ts-iface",
            &lang,
            TYPESCRIPT_QUERY,
        )
        .unwrap();
        assert!(n >= 1, "expected ≥ 1 TypeScript interface symbol, got {n}");
    }

    // ── build_snippet ─────────────────────────────────────────────────────────

    #[test]
    fn build_snippet_caps_at_10_lines() {
        let lines: Vec<&str> = (0..20).map(|_| "x").collect();
        let snippet = build_snippet(&lines, 0, 19);
        assert_eq!(snippet.lines().count(), 10);
    }
}

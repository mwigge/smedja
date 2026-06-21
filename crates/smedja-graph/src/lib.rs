//! `smedja-graph` — tree-sitter Rust symbol indexer and name-based `graph_query` tool.
//!
//! Walks Rust source files, parses them with `tree-sitter-rust`, extracts named
//! symbols (functions, structs, enums, traits, impls, consts, type aliases), stores
//! them in `SQLite`, and answers [`GraphStore::graph_query`] queries by name substring.

pub mod error;
pub mod indexer;
pub mod store;
pub mod types;

pub use error::GraphError;
pub use store::GraphStore;
pub use types::{Symbol, SymbolKind};

#[cfg(test)]
mod tests {
    use super::{GraphStore, SymbolKind};

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Creates an isolated temp directory, writes `source` to `<dir>/test.rs`,
    /// and returns `(dir, store)` with the temp dir kept alive.
    fn setup(source: &str) -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("test.rs"), source).expect("write");
        let store = GraphStore::open_in_memory().expect("open_in_memory");
        (dir, store)
    }

    // ── 1. function ───────────────────────────────────────────────────────────

    #[test]
    fn index_rust_snippet_finds_function() {
        let (dir, mut store) = setup("fn hello() {}\n");
        let count = store.index_workspace(dir.path(), "ws-fn").unwrap();
        assert!(count >= 1, "expected at least 1 symbol, got {count}");

        let results = store.graph_query("hello", 10, 0).unwrap();
        let found = results
            .iter()
            .any(|s| s.name == "hello" && s.kind == SymbolKind::Function);
        assert!(found, "did not find fn hello in results: {results:?}");
    }

    // ── 2. struct ─────────────────────────────────────────────────────────────

    #[test]
    fn index_rust_snippet_finds_struct() {
        let (dir, mut store) = setup("struct Foo { x: i32 }\n");
        store.index_workspace(dir.path(), "ws-struct").unwrap();

        let results = store.graph_query("Foo", 10, 0).unwrap();
        let found = results
            .iter()
            .any(|s| s.name == "Foo" && s.kind == SymbolKind::Struct);
        assert!(found, "did not find struct Foo: {results:?}");
    }

    // ── 3. case-insensitive ───────────────────────────────────────────────────

    #[test]
    fn graph_query_case_insensitive() {
        let (dir, mut store) = setup("fn WorkingMemory() {}\n");
        store.index_workspace(dir.path(), "ws-case").unwrap();

        let results = store.graph_query("workingmemory", 10, 0).unwrap();
        assert!(
            results.iter().any(|s| s.name == "WorkingMemory"),
            "case-insensitive query missed WorkingMemory: {results:?}"
        );
    }

    // ── 4. top-K ──────────────────────────────────────────────────────────────

    #[test]
    fn graph_query_returns_top_k() {
        let (dir, mut store) =
            setup("fn foo_1(){} fn foo_2(){} fn foo_3(){} fn foo_4(){} fn foo_5(){}\n");
        store.index_workspace(dir.path(), "ws-topk").unwrap();

        let results = store.graph_query("foo", 3, 0).unwrap();
        assert_eq!(
            results.len(),
            3,
            "expected exactly 3 results, got {}",
            results.len()
        );
    }

    // ── 5. clear_workspace ────────────────────────────────────────────────────

    #[test]
    fn clear_workspace_removes_symbols() {
        let (dir, mut store) = setup("fn alpha() {}\nstruct Beta;\n");
        store.index_workspace(dir.path(), "ws-clear").unwrap();

        let before = store.symbol_count("ws-clear").unwrap();
        assert!(before >= 1, "should have symbols before clear");

        store.clear_workspace("ws-clear").unwrap();
        assert_eq!(store.symbol_count("ws-clear").unwrap(), 0);
    }

    // ── 6. re-index replaces old symbols ──────────────────────────────────────

    #[test]
    fn reindex_replaces_old_symbols() {
        let (dir, mut store) = setup("fn gamma() {}\n");
        store.index_workspace(dir.path(), "ws-reindex").unwrap();
        let first_count = store.symbol_count("ws-reindex").unwrap();
        assert!(first_count >= 1);

        store.clear_workspace("ws-reindex").unwrap();
        store.index_workspace(dir.path(), "ws-reindex").unwrap();
        let second_count = store.symbol_count("ws-reindex").unwrap();

        assert_eq!(
            first_count, second_count,
            "re-index should produce the same count"
        );
    }

    // ── 7. workspace isolation ────────────────────────────────────────────────

    #[test]
    fn symbol_count_by_workspace() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();

        std::fs::write(dir_a.path().join("a.rs"), "fn aaa() {}\n").unwrap();
        std::fs::write(dir_b.path().join("b.rs"), "fn bbb() {}\nfn ccc() {}\n").unwrap();

        let mut store = GraphStore::open_in_memory().unwrap();
        store.index_workspace(dir_a.path(), "ws-a").unwrap();
        store.index_workspace(dir_b.path(), "ws-b").unwrap();

        let count_a = store.symbol_count("ws-a").unwrap();
        let count_b = store.symbol_count("ws-b").unwrap();

        assert_eq!(count_a, 1, "ws-a should have 1 symbol, got {count_a}");
        assert_eq!(count_b, 2, "ws-b should have 2 symbols, got {count_b}");
    }

    // ── 8. empty result ───────────────────────────────────────────────────────

    #[test]
    fn graph_query_empty_result() {
        let (dir, mut store) = setup("fn visible() {}\n");
        store.index_workspace(dir.path(), "ws-empty").unwrap();

        let results = store.graph_query("zzznomatch", 10, 0).unwrap();
        assert!(results.is_empty(), "expected empty result for zzznomatch");
    }

    // ── 9. index smedja itself ────────────────────────────────────────────────

    #[test]
    fn index_smedja_itself() {
        let root = std::path::Path::new("/data/src/smedja/crates/smedja-memory/src");
        if !root.exists() {
            // Skip when the path is not available in this environment.
            return;
        }

        let mut store = GraphStore::open_in_memory().unwrap();
        store.index_workspace(root, "ws-smedja").unwrap();

        let results = store.graph_query("WorkingMemory", 10, 0).unwrap();
        assert!(
            !results.is_empty(),
            "expected WorkingMemory symbol in smedja-memory, got none"
        );
    }
}

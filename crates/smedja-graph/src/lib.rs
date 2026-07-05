//! `smedja-graph` — multi-language tree-sitter symbol indexer and `graph_query` tool.
//!
//! Walks source files with recognised extensions (`.rs`, `.go`, `.py`, `.ts`, `.tsx`),
//! parses them with the appropriate tree-sitter grammar, extracts named symbols
//! (functions, classes, structs, enums, traits, impls, consts, type aliases), stores
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

    // ── 10. incremental re-index — unchanged files are skipped, total stable ──

    #[test]
    fn incremental_reindex_unchanged_returns_stable_total() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn hello() {}\n").expect("write");

        let mut store = GraphStore::open_in_memory().unwrap();

        // First index run — should insert symbols and report the repo total.
        let first = store
            .index_workspace_incremental(dir.path(), "ws-incr")
            .unwrap();
        assert!(
            first >= 1,
            "expected at least 1 symbol on first index, got {first}"
        );

        // Second run — file is unchanged, so nothing is re-parsed, but the
        // reported total (whole-repo symbol count) stays the same.
        let second = store
            .index_workspace_incremental(dir.path(), "ws-incr")
            .unwrap();
        assert_eq!(
            second, first,
            "unchanged file must keep the same total symbol count"
        );
    }

    // ── 11. incremental re-index — a modified file is re-indexed ──────────────

    #[test]
    fn incremental_reindex_picks_up_modifications() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("b.rs");
        std::fs::write(&file, "fn world() {}\n").expect("write");

        let mut store = GraphStore::open_in_memory().unwrap();
        let first = store
            .index_workspace_incremental(dir.path(), "ws-mod")
            .unwrap();
        assert_eq!(first, 1, "one function on first pass, got {first}");

        // Rewrite the file with an extra symbol and force a newer mtime so the
        // mtime-skip does not fire.
        std::fs::write(&file, "fn world() {}\nfn again() {}\n").expect("rewrite");
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        filetime_set(&file, future);

        let second = store
            .index_workspace_incremental(dir.path(), "ws-mod")
            .unwrap();
        assert_eq!(second, 2, "modified file must be re-indexed, got {second}");
    }

    // ── 12. incremental re-index — a deleted file is pruned ───────────────────

    #[test]
    fn incremental_reindex_prunes_deleted_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keep = dir.path().join("keep.rs");
        let gone = dir.path().join("gone.rs");
        std::fs::write(&keep, "fn keep() {}\n").expect("write");
        std::fs::write(&gone, "fn gone() {}\n").expect("write");

        let mut store = GraphStore::open_in_memory().unwrap();
        let first = store
            .index_workspace_incremental(dir.path(), "ws-del")
            .unwrap();
        assert_eq!(first, 2, "two functions on first pass, got {first}");

        std::fs::remove_file(&gone).expect("remove");
        let second = store
            .index_workspace_incremental(dir.path(), "ws-del")
            .unwrap();
        assert_eq!(
            second, 1,
            "deleted file's symbol must be pruned, got {second}"
        );
        assert!(
            store.graph_query("gone", 5, 0).unwrap().is_empty(),
            "pruned symbol must not be queryable"
        );
    }

    /// Sets a file's mtime to `t` for deterministic incremental-index tests,
    /// without pulling in an extra crate: shells out to `touch -d`.
    fn filetime_set(path: &std::path::Path, t: std::time::SystemTime) {
        let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let status = std::process::Command::new("touch")
            .arg("-d")
            .arg(format!("@{secs}"))
            .arg(path)
            .status()
            .expect("touch");
        assert!(status.success(), "touch failed");
    }

    // ── 13. integration: index smedja repo; query WorkingMemory ─────────────

    #[test]
    #[ignore = "requires the full smedja repository at /data/src/smedja"]
    fn integration_index_smedja_and_query_working_memory() {
        let root = std::path::Path::new("/data/src/smedja");

        let mut store = GraphStore::open_in_memory().unwrap();
        store.index_workspace(root, "smedja-self").unwrap();

        let results = store.graph_query("WorkingMemory", 2, 0).unwrap();
        assert!(
            !results.is_empty(),
            "graph_query(WorkingMemory) must return at least one symbol"
        );
    }

    // ── 14. Go grammar ────────────────────────────────────────────────────────

    #[test]
    fn index_go_snippet_finds_function() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("main.go"),
            "package main\n\nfunc RunServer() error { return nil }\n",
        )
        .expect("write");

        let mut store = GraphStore::open_in_memory().expect("open_in_memory");
        let count = store.index_workspace(dir.path(), "ws-go").unwrap();
        assert!(count >= 1, "expected ≥ 1 Go symbol, got {count}");

        let results = store.graph_query("RunServer", 10, 0).unwrap();
        assert!(
            results.iter().any(|s| s.name == "RunServer"),
            "did not find RunServer in results: {results:?}"
        );
    }

    // ── 15. Python grammar ────────────────────────────────────────────────────

    #[test]
    fn index_python_snippet_finds_function() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("service.py"),
            "def process_request(req):\n    return req\n",
        )
        .expect("write");

        let mut store = GraphStore::open_in_memory().expect("open_in_memory");
        let count = store.index_workspace(dir.path(), "ws-py").unwrap();
        assert!(count >= 1, "expected ≥ 1 Python symbol, got {count}");

        let results = store.graph_query("process_request", 10, 0).unwrap();
        assert!(
            results.iter().any(|s| s.name == "process_request"),
            "did not find process_request in results: {results:?}"
        );
    }

    // ── 16. TypeScript grammar ────────────────────────────────────────────────

    #[test]
    fn index_typescript_snippet_finds_function() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("agent.ts"),
            "function executeAgent(task: string): void {}\n",
        )
        .expect("write");

        let mut store = GraphStore::open_in_memory().expect("open_in_memory");
        let count = store.index_workspace(dir.path(), "ws-ts").unwrap();
        assert!(count >= 1, "expected ≥ 1 TypeScript symbol, got {count}");

        let results = store.graph_query("executeAgent", 10, 0).unwrap();
        assert!(
            results.iter().any(|s| s.name == "executeAgent"),
            "did not find executeAgent in results: {results:?}"
        );
    }
}

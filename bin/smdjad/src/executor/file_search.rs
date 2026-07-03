//! Native directory-traversal handlers: `list_files`, `grep_files`, and
//! `find_files`, plus the minimal glob matcher they share.

use serde_json::Value;

use crate::executor::fs_tools::assert_within_workspace;
use crate::executor::glob::glob_match;

/// Lists workspace files under `path` up to `depth`, hiding dotfiles and
/// optionally filtering file names by a glob `pattern`.
pub(crate) async fn list_files(input: &Value, workspace: &std::path::Path) -> String {
    let dir_str = input.get("path").and_then(Value::as_str).unwrap_or(".");
    let full = match assert_within_workspace(workspace, dir_str) {
        Ok(p) => p,
        Err(err) => return err,
    };
    let depth: usize = input
        .get("depth")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .try_into()
        .unwrap_or(usize::MAX);
    let pattern = input
        .get("pattern")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let max_depth = if depth == 0 { usize::MAX } else { depth };
    match tokio::task::spawn_blocking(move || {
        let mut entries = Vec::new();
        let walker = walkdir::WalkDir::new(&full)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));
        for entry in walker.filter_map(std::result::Result::ok).skip(1) {
            let name = entry.path().strip_prefix(&full).unwrap_or(entry.path());
            let name_str = name.to_string_lossy().into_owned();
            if let Some(ref pat) = pattern {
                // Simple glob: only match file name portion against the pattern.
                let file_name = entry.file_name().to_string_lossy();
                if !glob_match(pat, &file_name) {
                    continue;
                }
            }
            entries.push(name_str);
        }
        entries.sort();
        entries.join("\n")
    })
    .await
    {
        Ok(result) => result,
        Err(e) => format!("error listing {dir_str}: {e}"),
    }
}

/// Searches files under `path` for lines containing the literal `pattern`,
/// returning up to `max_results` JSON hit records.
pub(crate) async fn grep_files(input: &Value, workspace: &std::path::Path) -> String {
    let pattern = match input.get("pattern").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p.to_owned(),
        _ => return "error: pattern field required".into(),
    };
    let sub = input.get("path").and_then(Value::as_str).unwrap_or(".");
    let max_results: usize = input
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .try_into()
        .unwrap_or(usize::MAX);
    let root = match assert_within_workspace(workspace, sub) {
        Ok(p) => p,
        Err(e) => return e,
    };
    tokio::task::spawn_blocking(move || {
        let mut hits: Vec<serde_json::Value> = Vec::new();
        let walker = walkdir::WalkDir::new(&root).into_iter();
        'files: for entry in walker.filter_map(std::result::Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let rel = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .into_owned();
            for (n, line) in text.lines().enumerate() {
                if line.contains(pattern.as_str()) {
                    hits.push(serde_json::json!({
                        "file": rel,
                        "line": n + 1,
                        "text": line.trim_end(),
                    }));
                    if hits.len() >= max_results {
                        break 'files;
                    }
                }
            }
        }
        let count = hits.len();
        serde_json::json!({"matches": hits, "count": count}).to_string()
    })
    .await
    .unwrap_or_else(|e| format!("error: grep_files failed: {e}"))
}

/// Finds files under `path` whose name matches the glob `pattern`, returning up
/// to `max_results` relative paths.
pub(crate) async fn find_files(input: &Value, workspace: &std::path::Path) -> String {
    let pattern = input
        .get("pattern")
        .and_then(Value::as_str)
        .unwrap_or("*")
        .to_owned();
    let sub = input.get("path").and_then(Value::as_str).unwrap_or(".");
    let max_results: usize = input
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .try_into()
        .unwrap_or(usize::MAX);
    let root = match assert_within_workspace(workspace, sub) {
        Ok(p) => p,
        Err(e) => return e,
    };
    tokio::task::spawn_blocking(move || {
        let mut files: Vec<String> = Vec::new();
        let walker = walkdir::WalkDir::new(&root).into_iter();
        for entry in walker.filter_map(std::result::Result::ok).skip(1) {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if glob_match(&pattern, &name) {
                let rel = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .into_owned();
                files.push(rel);
                if files.len() >= max_results {
                    break;
                }
            }
        }
        files.sort();
        let count = files.len();
        serde_json::json!({"files": files, "count": count}).to_string()
    })
    .await
    .unwrap_or_else(|e| format!("error: find_files failed: {e}"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::executor::execute_tool;

    fn test_embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    fn mk_ingot_vault() -> (
        smedja_ingot::IngotHandle,
        std::sync::Arc<tokio::sync::Mutex<smedja_vault::Vault>>,
    ) {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;
        (
            IngotHandle::new(Ingot::open_in_memory().unwrap()),
            Arc::new(Mutex::new(Vault::open_in_memory().unwrap())),
        )
    }

    fn session_with_mode(mode: Option<&str>) -> smedja_ingot::Session {
        smedja_ingot::Session {
            id: uuid::Uuid::new_v4(),
            created_at: smedja_types::Timestamp::from_micros(0),
            updated_at: smedja_types::Timestamp::from_micros(0),
            status: "active".to_owned(),
            task_id: None,
            mode: mode.map(str::to_owned),
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
    }

    // ── grep_files ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn grep_files_finds_matching_lines() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::write(ws.join("a.txt"), "hello world\nno match here\n").unwrap();
        std::fs::write(ws.join("b.txt"), "world domination\n").unwrap();

        let result = execute_tool(
            "grep_files",
            r#"{"pattern":"world"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        let matches = v["matches"].as_array().unwrap();
        assert_eq!(v["count"], 2, "two lines contain 'world'; got: {result}");
        assert!(
            matches
                .iter()
                .any(|m| m["text"].as_str().unwrap().contains("hello world")),
            "hello world match must appear; got: {result}"
        );
    }

    #[tokio::test]
    async fn grep_files_respects_max_results() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        // Write 12 matching lines across two files
        std::fs::write(ws.join("x.txt"), "match\n".repeat(6)).unwrap();
        std::fs::write(ws.join("y.txt"), "match\n".repeat(6)).unwrap();

        let result = execute_tool(
            "grep_files",
            r#"{"pattern":"match","max_results":3}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["count"], 3, "max_results=3 must cap at 3; got: {result}");
    }

    #[tokio::test]
    async fn grep_files_rejects_path_outside_workspace() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();

        let result = execute_tool(
            "grep_files",
            r#"{"pattern":"x","path":"../../etc"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            result.contains("path outside workspace") || result.starts_with("error:"),
            "traversal must be rejected; got: {result}"
        );
    }

    // ── find_files ────────────────────────────────────────────────────────────

    #[tokio::test]
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    async fn find_files_matches_glob_pattern() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::write(ws.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(ws.join("lib.rs"), "pub fn lib() {}").unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[package]").unwrap();

        let result = execute_tool(
            "find_files",
            r#"{"pattern":"*.rs"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 2, "two .rs files; got: {result}");
        assert!(
            files
                .iter()
                .any(|f| f.as_str().unwrap().ends_with("main.rs")),
            "main.rs must appear; got: {result}"
        );
        assert!(
            files.iter().all(|f| f.as_str().unwrap().ends_with(".rs")),
            "only .rs files; got: {result}"
        );
    }

    #[tokio::test]
    async fn find_files_respects_max_results() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        for i in 0..10u8 {
            std::fs::write(ws.join(format!("f{i}.txt")), "").unwrap();
        }

        let result = execute_tool(
            "find_files",
            r#"{"pattern":"*.txt","max_results":4}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["count"], 4, "max_results=4 must cap at 4; got: {result}");
    }

    #[tokio::test]
    async fn find_files_rejects_path_outside_workspace() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();

        let result = execute_tool(
            "find_files",
            r#"{"pattern":"*","path":"../../etc"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            result.contains("path outside workspace") || result.starts_with("error:"),
            "traversal must be rejected; got: {result}"
        );
    }

    #[tokio::test]
    async fn grep_find_allowed_in_review_session() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::write(ws.join("a.rs"), "fn main() {}").unwrap();
        let session = session_with_mode(Some("review"));

        let grep_result = execute_tool(
            "grep_files",
            r#"{"pattern":"fn main"}"#,
            &ws,
            Some(&session),
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            !grep_result.contains("TOOL_BLOCKED"),
            "grep_files must be allowed in review session; got: {grep_result}"
        );

        let find_result = execute_tool(
            "find_files",
            r#"{"pattern":"*.rs"}"#,
            &ws,
            Some(&session),
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            !find_result.contains("TOOL_BLOCKED"),
            "find_files must be allowed in review session; got: {find_result}"
        );
    }
}

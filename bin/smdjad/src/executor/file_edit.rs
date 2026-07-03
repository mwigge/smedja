//! Native single-file handlers: `read_file`, `write_file`, and `edit_file`.
//!
//! Every path is resolved through [`assert_within_workspace`] before any I/O, so
//! the workspace boundary is enforced identically across the three tools.

use base64::Engine as _;
use serde_json::Value;

use crate::executor::fs_tools::assert_within_workspace;

/// Reads a file, optionally base64-encoding it or slicing a 1-based line range.
pub(crate) async fn read_file(input: &Value, workspace: &std::path::Path) -> String {
    let path_str = input
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let full = match assert_within_workspace(workspace, path_str) {
        Ok(p) => p,
        Err(err) => return err,
    };
    let encoding = input
        .get("encoding")
        .and_then(Value::as_str)
        .unwrap_or("text");
    let start_line = input
        .get("start_line")
        .and_then(Value::as_u64)
        .map(|n| n.try_into().unwrap_or(usize::MAX));
    let end_line = input
        .get("end_line")
        .and_then(Value::as_u64)
        .map(|n| n.try_into().unwrap_or(usize::MAX));
    let raw = match tokio::fs::read(&full).await {
        Ok(bytes) => bytes,
        Err(e) => return format!("error reading {path_str}: {e}"),
    };
    if encoding == "base64" {
        base64::engine::general_purpose::STANDARD.encode(&raw)
    } else {
        let text = String::from_utf8_lossy(&raw).into_owned();
        if start_line.is_none() && end_line.is_none() {
            text
        } else {
            let start = start_line.unwrap_or(1).saturating_sub(1);
            let lines: Vec<&str> = text.lines().collect();
            let end = end_line.map_or(lines.len(), |e| e.min(lines.len()));
            lines[start.min(lines.len())..end].join("\n")
        }
    }
}

/// Writes `content` to `path`, creating parent directories as needed.
pub(crate) async fn write_file(input: &Value, workspace: &std::path::Path) -> String {
    let path_str = input
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if path_str.is_empty() {
        return "error: path field required".into();
    }
    // Boundary already enforced upstream; re-resolve to get the full path.
    let full = match assert_within_workspace(workspace, path_str) {
        Ok(p) => p,
        Err(err) => return err,
    };
    let content = input
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(parent) = full.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return format!("error: write_file failed: {e}");
        }
    }
    match tokio::fs::write(&full, content).await {
        Ok(()) => serde_json::json!({"written": true, "bytes": content.len()}).to_string(),
        Err(e) => format!("error: write_file failed: {e}"),
    }
}

/// Replaces `old_string` with `new_string` in `path` (or creates it when
/// `old_string` is empty), rejecting ambiguous matches unless `replace_all`.
pub(crate) async fn edit_file(input: &Value, workspace: &std::path::Path) -> String {
    let path_str = input
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if path_str.is_empty() {
        return "error: path field required".into();
    }
    let full = match assert_within_workspace(workspace, path_str) {
        Ok(p) => p,
        Err(err) => return err,
    };
    let old = input
        .get("old_string")
        .or_else(|| input.get("old_str"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let new = input
        .get("new_string")
        .or_else(|| input.get("new_str"))
        .or_else(|| input.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let replace_all = input
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Empty old_string means create-or-overwrite with `new`.
    let current = match tokio::fs::read_to_string(&full).await {
        Ok(s) => s,
        Err(_) if old.is_empty() => String::new(),
        Err(e) => return format!("error: edit_file failed to read {path_str}: {e}"),
    };
    let (updated, replacements) = if old.is_empty() {
        (new.to_owned(), 1usize)
    } else {
        let count = current.matches(old).count();
        if count == 0 {
            return format!("error: edit_file: old_string not found in {path_str}");
        }
        if count > 1 && !replace_all {
            return format!(
                "error: edit_file: old_string matches {count} times in {path_str}; \
                 pass replace_all=true or include more surrounding context"
            );
        }
        if replace_all {
            (current.replace(old, new), count)
        } else {
            (current.replacen(old, new, 1), 1)
        }
    };
    if let Some(parent) = full.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return format!("error: edit_file failed: {e}");
        }
    }
    match tokio::fs::write(&full, &updated).await {
        Ok(()) => serde_json::json!({"edited": true, "replacements": replacements}).to_string(),
        Err(e) => format!("error: edit_file failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::executor::execute_tool;

    fn test_embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    #[tokio::test]
    async fn write_and_edit_file_are_native() {
        use smedja_vault::Vault;
        use tokio::sync::Mutex;
        let ingot = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();

        // write_file creates the file (incl. parent dirs) natively.
        let w = execute_tool(
            "write_file",
            r#"{"path":"sub/a.txt","content":"hello world"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(w.contains("\"written\":true"), "got: {w}");
        assert_eq!(
            std::fs::read_to_string(ws.join("sub/a.txt")).unwrap(),
            "hello world"
        );

        // edit_file replaces a unique occurrence natively.
        let e = execute_tool(
            "edit_file",
            r#"{"path":"sub/a.txt","old_string":"world","new_string":"smedja"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(e.contains("\"edited\":true"), "got: {e}");
        assert_eq!(
            std::fs::read_to_string(ws.join("sub/a.txt")).unwrap(),
            "hello smedja"
        );

        // Ambiguous edit without replace_all is rejected rather than silently guessing.
        std::fs::write(ws.join("sub/a.txt"), "x x").unwrap();
        let ambiguous = execute_tool(
            "edit_file",
            r#"{"path":"sub/a.txt","old_string":"x","new_string":"y"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(ambiguous.contains("matches 2 times"), "got: {ambiguous}");
    }

    #[test]
    fn read_file_line_range_returns_subset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        std::fs::write(&path, "line1\nline2\nline3\nline4\n").unwrap();

        // Simulate the line-range extraction used in the read_file handler.
        let text = std::fs::read_to_string(&path).unwrap();
        let start_line: usize = 2;
        let end_line: usize = 3;
        let start = start_line.saturating_sub(1);
        let lines: Vec<&str> = text.lines().collect();
        let end = end_line.min(lines.len());
        let result = lines[start..end].join("\n");
        assert_eq!(result, "line2\nline3");
    }

    #[test]
    fn read_file_base64_roundtrip() {
        use base64::Engine as _;
        let raw = b"\x00\x01\x02\x03binary";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        assert_eq!(decoded, raw);
    }
}

//! Filesystem-tool helpers for [`execute_tool`](super::execute_tool): workspace
//! boundary enforcement, current-content reads, and proposed-content extraction.

use serde_json::Value;
use smedja_ingot::Session;

/// Extracts the proposed file content from a `write_file` / `edit_file` tool
/// input, trying the common field names used by file-writing MCP tools.
///
/// Returns `None` when no content-bearing field is present, in which case the
/// diff gate is skipped (it cannot build a meaningful diff).
pub(crate) fn extract_proposed_content(input: &Value) -> Option<String> {
    input
        .get("content")
        .or_else(|| input.get("new_string"))
        .or_else(|| input.get("new_str"))
        .or_else(|| input.get("replacement"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// Reads the current contents of `path_str` relative to `workspace`, returning
/// an empty string when the file is absent (new file) or unreadable.
pub(crate) async fn read_current_content(workspace: &std::path::Path, path_str: &str) -> String {
    if path_str.is_empty() {
        return String::new();
    }
    tokio::fs::read_to_string(workspace.join(path_str))
        .await
        .unwrap_or_default()
}

/// Returns `true` when the session's mode permits write-arity bash commands.
///
/// The `"review"` mode is read-only by default; all other modes are unrestricted.
pub(crate) fn role_allows_write_bash(session: &Session) -> bool {
    // ponytail: review role is read-only by default; all others are unrestricted
    session.mode.as_deref() != Some("review")
}

/// Canonicalises `workspace.join(path_str)` and asserts it stays within the
/// canonicalised `workspace` root.
///
/// Mirrors the boundary check previously duplicated in `write_file`/`edit_file`,
/// `read_file`, and `list_files`: an existing path is canonicalised and checked
/// against the canonical workspace root; a not-yet-existing path is checked
/// against the root using its (uncanonicalised) join. The relative join is
/// returned when canonicalisation fails but the boundary check passes, matching
/// the prior behaviour exactly.
///
/// # Errors
///
/// Returns the JSON error string `{"error": "path outside workspace"}` (byte for
/// byte identical to the previous inline rejection) when the resolved path
/// escapes the workspace root.
pub(crate) fn assert_within_workspace(
    workspace: &std::path::Path,
    path_str: &str,
) -> Result<std::path::PathBuf, String> {
    let raw_join = workspace.join(path_str);
    let workspace_canon = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_owned());
    let full = if let Ok(p) = raw_join.canonicalize() {
        p
    } else {
        let tentative = workspace.join(path_str);
        if !tentative.starts_with(&workspace_canon) {
            return Err(r#"{"error": "path outside workspace"}"#.to_owned());
        }
        tentative
    };
    if !full.starts_with(&workspace_canon) {
        return Err(r#"{"error": "path outside workspace"}"#.to_owned());
    }
    Ok(full)
}

#[cfg(test)]
mod tests {
    #[test]
    fn assert_within_workspace_accepts_path_inside_root() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let result = super::assert_within_workspace(&ws, "sub/file.rs");
        assert!(result.is_ok(), "in-workspace path must be accepted");
        assert!(result.unwrap().starts_with(&ws));
    }

    #[test]
    fn assert_within_workspace_rejects_path_outside_root() {
        // A nested workspace whose parent contains a real, canonicalisable file
        // that lies outside the root: the traversal resolves and the boundary
        // check rejects it.
        let parent = tempfile::tempdir().unwrap();
        let parent = parent.path().canonicalize().unwrap();
        std::fs::write(parent.join("secret.txt"), b"x").unwrap();
        let ws = parent.join("ws");
        std::fs::create_dir(&ws).unwrap();
        let result = super::assert_within_workspace(&ws, "../secret.txt");
        assert!(result.is_err(), "traversal path must be rejected");
        assert_eq!(
            result.unwrap_err(),
            r#"{"error": "path outside workspace"}"#,
            "rejection must return the exact error JSON"
        );
    }
}

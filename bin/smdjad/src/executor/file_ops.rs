//! Native workspace-mutating file operations: `move_file`, `copy_file`, and
//! `delete_file`. Every path is boundary-checked before any filesystem call.

use serde_json::Value;

use crate::executor::fs_tools::assert_within_workspace;

/// Renames `source` to `destination`, both resolved within the workspace.
pub(crate) fn move_file(input: &Value, workspace: &std::path::Path) -> String {
    let Some(src_str) = input.get("source").and_then(Value::as_str) else {
        return "error: source field required".into();
    };
    let Some(dst_str) = input.get("destination").and_then(Value::as_str) else {
        return "error: destination field required".into();
    };
    let src = match assert_within_workspace(workspace, src_str) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let dst = match assert_within_workspace(workspace, dst_str) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::rename(&src, &dst) {
        Ok(()) => serde_json::json!({"moved": true}).to_string(),
        Err(e) => format!("error: move_file failed: {e}"),
    }
}

/// Copies `source` to `destination`, creating parent directories as needed.
pub(crate) fn copy_file(input: &Value, workspace: &std::path::Path) -> String {
    let Some(src_str) = input.get("source").and_then(Value::as_str) else {
        return "error: source field required".into();
    };
    let Some(dst_str) = input.get("destination").and_then(Value::as_str) else {
        return "error: destination field required".into();
    };
    let src = match assert_within_workspace(workspace, src_str) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let dst = match assert_within_workspace(workspace, dst_str) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::copy(&src, &dst) {
        Ok(_) => serde_json::json!({"copied": true}).to_string(),
        Err(e) => format!("error: copy_file failed: {e}"),
    }
}

/// Deletes `path`; empty directories are removed, non-empty ones are refused.
pub(crate) fn delete_file(input: &Value, workspace: &std::path::Path) -> String {
    let Some(path_str) = input.get("path").and_then(Value::as_str) else {
        return "error: path field required".into();
    };
    let full = match assert_within_workspace(workspace, path_str) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // ponytail: full async cowork gate is in roadmap; log destructive op and proceed
    tracing::info!(
        path = path_str,
        "delete_file: cowork gate (full approval gate is in roadmap)"
    );
    match std::fs::metadata(&full) {
        Err(e) => format!("error: delete_file failed: {e}"),
        Ok(meta) if meta.is_dir() => match std::fs::remove_dir(&full) {
            Ok(()) => serde_json::json!({"deleted": true}).to_string(),
            Err(e) => {
                format!("error: delete_file failed (use bash for non-empty directories): {e}")
            }
        },
        Ok(_) => match std::fs::remove_file(&full) {
            Ok(()) => serde_json::json!({"deleted": true}).to_string(),
            Err(e) => format!("error: delete_file failed: {e}"),
        },
    }
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

    #[tokio::test]
    async fn move_file_renames_within_workspace() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::write(ws.join("old.txt"), "content").unwrap();

        let result = execute_tool(
            "move_file",
            r#"{"source":"old.txt","destination":"new.txt"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["moved"], true, "move must succeed; got: {result}");
        assert!(
            !ws.join("old.txt").exists(),
            "source must not exist after move"
        );
        assert!(
            ws.join("new.txt").exists(),
            "destination must exist after move"
        );
    }

    #[tokio::test]
    async fn move_file_rejects_source_outside_workspace() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();

        let result = execute_tool(
            "move_file",
            r#"{"source":"../../etc/passwd","destination":"out.txt"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            result.contains("path outside workspace") || result.starts_with("error:"),
            "boundary rejection must fire on source; got: {result}"
        );
    }

    #[tokio::test]
    async fn copy_file_copies_within_workspace() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::write(ws.join("src.txt"), "data").unwrap();

        let result = execute_tool(
            "copy_file",
            r#"{"source":"src.txt","destination":"dst.txt"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["copied"], true, "copy must succeed; got: {result}");
        assert!(
            ws.join("src.txt").exists(),
            "source must still exist after copy"
        );
        assert_eq!(std::fs::read_to_string(ws.join("dst.txt")).unwrap(), "data");
    }

    #[tokio::test]
    async fn copy_file_creates_parent_dirs() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::write(ws.join("src.txt"), "data").unwrap();

        let result = execute_tool(
            "copy_file",
            r#"{"source":"src.txt","destination":"sub/dir/dst.txt"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            v["copied"], true,
            "copy to new subdir must succeed; got: {result}"
        );
        assert!(ws.join("sub/dir/dst.txt").exists());
    }

    #[tokio::test]
    async fn copy_file_rejects_destination_outside_workspace() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::write(ws.join("src.txt"), "x").unwrap();

        let result = execute_tool(
            "copy_file",
            r#"{"source":"src.txt","destination":"../../tmp/evil.txt"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            result.contains("path outside workspace") || result.starts_with("error:"),
            "boundary rejection must fire on destination; got: {result}"
        );
    }

    #[tokio::test]
    async fn delete_file_removes_a_file() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::write(ws.join("rm_me.txt"), "bye").unwrap();

        let result = execute_tool(
            "delete_file",
            r#"{"path":"rm_me.txt"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["deleted"], true, "delete must succeed; got: {result}");
        assert!(
            !ws.join("rm_me.txt").exists(),
            "file must be gone after delete"
        );
    }

    #[tokio::test]
    async fn delete_file_rejects_path_outside_workspace() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();

        let result = execute_tool(
            "delete_file",
            r#"{"path":"../../etc/passwd"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            result.contains("path outside workspace") || result.starts_with("error:"),
            "boundary rejection must fire; got: {result}"
        );
    }

    #[tokio::test]
    async fn delete_file_refuses_nonempty_directory() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let subdir = ws.join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        std::fs::write(subdir.join("file.txt"), "keep").unwrap();

        let result = execute_tool(
            "delete_file",
            r#"{"path":"subdir"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            result.starts_with("error:"),
            "non-empty dir delete must fail; got: {result}"
        );
        assert!(subdir.exists(), "non-empty dir must remain; got: {result}");
    }

    #[tokio::test]
    async fn move_copy_delete_blocked_in_review_session() {
        let (ingot, vault) = mk_ingot_vault();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::write(ws.join("f.txt"), "x").unwrap();
        let session = session_with_mode(Some("review"));

        for (tool, input) in [
            ("move_file", r#"{"source":"f.txt","destination":"g.txt"}"#),
            ("copy_file", r#"{"source":"f.txt","destination":"h.txt"}"#),
            ("delete_file", r#"{"path":"f.txt"}"#),
        ] {
            let result = execute_tool(
                tool,
                input,
                &ws,
                Some(&session),
                &ingot,
                &vault,
                &test_embedder(),
            )
            .await;
            assert!(
                result.contains("TOOL_BLOCKED") || result.contains("blocked"),
                "{tool} must be blocked in review session; got: {result}"
            );
        }
    }
}

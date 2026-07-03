//! Workspace-bounded resolution of a change's `tasks.md` and reading its pending
//! slices.

use std::path::{Path, PathBuf};

/// Resolves the change's `tasks.md` path, returning it only when it canonically
/// resolves inside `workspace_root`.
///
/// Canonicalising the change directory catches symlink escapes and a
/// non-canonical `SMEDJA_WORKSPACE` that the `..`/`/` name check alone would
/// miss. Returns `None` when the change directory is absent (no work to do) or
/// the resolved path would escape the workspace.
pub(crate) fn safe_tasks_path(workspace_root: &Path, change_name: &str) -> Option<PathBuf> {
    let ws_canon = workspace_root.canonicalize().ok()?;
    let change_dir = ws_canon.join("openspec").join("changes").join(change_name);
    // Canonicalise the change directory (the file itself may be absent) and
    // assert it stays within the workspace root.
    let dir_canon = change_dir.canonicalize().ok()?;
    if !dir_canon.starts_with(&ws_canon) {
        tracing::warn!(
            change = change_name,
            "loop.run: tasks path escapes the workspace root; refusing to read it"
        );
        return None;
    }
    Some(dir_canon.join("tasks.md"))
}

/// Reads the pending slices (`- [ ] ` lines) from the change's `tasks.md`.
///
/// Returns an empty vector when the file is absent or the path would escape the
/// workspace — a loop with no readable pending work completes immediately.
pub(crate) async fn read_pending_slices(workspace_root: &Path, change_name: &str) -> Vec<String> {
    let Some(tasks_path) = safe_tasks_path(workspace_root, change_name) else {
        return Vec::new();
    };
    match tokio::fs::read_to_string(&tasks_path).await {
        Ok(content) => content
            .lines()
            .filter(|l| l.starts_with("- [ ] "))
            .map(|l| l.trim_start_matches("- [ ] ").to_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    #[test]
    fn safe_tasks_path_accepts_in_workspace_and_rejects_symlink_escape() {
        let ws = TempDir::new().unwrap();
        let ws_root = ws.path().canonicalize().unwrap();

        // A normal in-workspace change resolves.
        let good = ws_root.join("openspec").join("changes").join("good");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join("tasks.md"), "- [ ] 1.1 do it\n").unwrap();
        assert!(super::safe_tasks_path(&ws_root, "good").is_some());

        // A change dir that symlinks outside the workspace is rejected.
        let outside = TempDir::new().unwrap();
        let outside_canon = outside.path().canonicalize().unwrap();
        std::fs::write(outside_canon.join("tasks.md"), "- [ ] evil\n").unwrap();
        let evil_link = ws_root.join("openspec").join("changes").join("evil");
        std::os::unix::fs::symlink(&outside_canon, &evil_link).unwrap();
        assert!(
            super::safe_tasks_path(&ws_root, "evil").is_none(),
            "a symlinked change dir escaping the workspace must be refused"
        );
    }

    #[tokio::test]
    async fn umbrella_tasks_md_coarse_lines_are_read_as_slice_list() {
        // Task 4.1/4.2: the umbrella's coarse `- [ ]` lines are read as the slice
        // list; each coarse group maps to exactly one slice the engine iterates.
        let ws = TempDir::new().unwrap();
        let ws_root = ws.path().canonicalize().unwrap();
        let change_dir = ws_root.join("openspec").join("changes").join("umbrella");
        std::fs::create_dir_all(&change_dir).unwrap();
        // An umbrella tasks.md lists slices coarsely — one `- [ ]` per slice, no
        // granular per-step decomposition. The `## ` headings and a `[x]` line
        // must NOT be read as slices.
        std::fs::write(
            change_dir.join("tasks.md"),
            "## Slices\n\n\
             - [ ] Slice 1: store the umbrella\n\
             - [ ] Slice 2: resolve the pointer\n\
             - [x] already done — must be skipped\n\
             - [ ] Slice 3: hybrid loading\n",
        )
        .unwrap();

        let slices = super::read_pending_slices(&ws_root, "umbrella").await;
        assert_eq!(
            slices,
            vec![
                "Slice 1: store the umbrella".to_owned(),
                "Slice 2: resolve the pointer".to_owned(),
                "Slice 3: hybrid loading".to_owned(),
            ],
            "each coarse `- [ ]` line must map to exactly one pending slice"
        );
    }
}

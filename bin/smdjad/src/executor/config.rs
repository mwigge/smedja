//! Workspace `[tools]` configuration read from `<workspace>/.smedja/workspace.toml`.
//!
//! A missing or unparseable file always resolves to defaults so startup is never
//! blocked by config trouble.

/// Bash tool configuration: blocked command substrings and an optional default
/// timeout.
#[derive(serde::Deserialize, Default)]
pub(crate) struct BashConfig {
    pub(crate) blocked_patterns: Option<Vec<String>>,
    pub(crate) timeout_secs: Option<u64>,
}

/// Loads the `[tools.bash]` section for `workspace`, or defaults when absent.
pub(crate) fn bash_config(workspace: &std::path::Path) -> BashConfig {
    #[derive(serde::Deserialize, Default)]
    struct WorkspaceToml {
        tools: Option<ToolsSection>,
    }
    #[derive(serde::Deserialize, Default)]
    struct ToolsSection {
        bash: Option<BashConfig>,
    }
    let path = workspace.join(".smedja").join("workspace.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<WorkspaceToml>(&s).ok())
        .and_then(|c| c.tools?.bash)
        .unwrap_or_default()
}

/// Returns `true` when the workspace `[tools]` config has `confirm_edits = true`.
///
/// Reads `<workspace>/.smedja/workspace.toml`.  A missing or unparseable file
/// resolves to `false` so startup is never blocked by config trouble.
pub(crate) fn is_confirm_edits_enabled(workspace: &std::path::Path) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct WorkspaceToml {
        tools: Option<ToolsSection>,
    }
    #[derive(serde::Deserialize, Default)]
    struct ToolsSection {
        confirm_edits: Option<bool>,
    }
    let path = workspace.join(".smedja").join("workspace.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<WorkspaceToml>(&s).ok())
        .and_then(|c| c.tools?.confirm_edits)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{bash_config, is_confirm_edits_enabled};

    // ── is_confirm_edits_enabled ──────────────────────────────────────────────

    #[test]
    fn confirm_edits_defaults_to_false_when_no_workspace_toml() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !is_confirm_edits_enabled(dir.path()),
            "missing workspace.toml must resolve to false"
        );
    }

    #[test]
    fn confirm_edits_false_when_key_absent() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(smedja.join("workspace.toml"), "[workspace]\nname = \"x\"\n").unwrap();
        assert!(
            !is_confirm_edits_enabled(dir.path()),
            "missing [tools] key must resolve to false"
        );
    }

    #[test]
    fn confirm_edits_true_when_enabled_in_workspace_toml() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(
            smedja.join("workspace.toml"),
            "[tools]\nconfirm_edits = true\n",
        )
        .unwrap();
        assert!(
            is_confirm_edits_enabled(dir.path()),
            "confirm_edits = true must be detected"
        );
    }

    #[test]
    fn confirm_edits_false_when_explicitly_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(
            smedja.join("workspace.toml"),
            "[tools]\nconfirm_edits = false\n",
        )
        .unwrap();
        assert!(
            !is_confirm_edits_enabled(dir.path()),
            "confirm_edits = false must resolve to false"
        );
    }

    // --- WI-014: bash blocked_patterns (config parse) ---

    #[test]
    fn bash_blocked_patterns_empty_when_no_workspace_toml() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            bash_config(dir.path())
                .blocked_patterns
                .unwrap_or_default()
                .is_empty(),
            "missing workspace.toml must return empty patterns"
        );
    }

    #[test]
    fn bash_blocked_patterns_loaded_from_workspace_toml() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(
            smedja.join("workspace.toml"),
            "[tools.bash]\nblocked_patterns = [\"rm -rf /\", \"curl * | sh\"]\n",
        )
        .unwrap();
        let patterns = bash_config(dir.path()).blocked_patterns.unwrap_or_default();
        assert_eq!(patterns, vec!["rm -rf /", "curl * | sh"]);
    }
}

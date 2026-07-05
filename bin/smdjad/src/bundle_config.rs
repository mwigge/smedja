//! Daemon-side loader for the runner-agnostic skills/rules/agents *bundle*.
//!
//! A workspace's `.smedja/` tree is always a bundle source. A workspace may
//! additionally point at one external bundle root (an `agent-toolkit-bundle/`
//! style folder with `skills/`, `rules/`, and `agents/` subtrees) via
//! `<workspace>/.smedja/config.toml`:
//!
//! ```toml
//! [bundle]
//! root = "../shared-agent-toolkit"   # relative to the workspace, or absolute
//! ```
//!
//! A missing or unparseable file resolves to "no external root" and never blocks
//! startup, mirroring [`crate::methodology_config`].

use std::path::{Path, PathBuf};

use serde::Deserialize;
use smedja_plugins::Bundle;

/// The relevant slice of a smedja config document.
#[derive(Debug, Default, Deserialize)]
struct ConfigDoc {
    bundle: Option<RawBundle>,
}

#[derive(Debug, Default, Deserialize)]
struct RawBundle {
    root: Option<String>,
}

/// Resolves the optional external bundle root for `workspace_root`.
///
/// Returns `None` when no `[bundle] root` is configured. A relative `root` is
/// resolved against the workspace; an absolute `root` is used as-is.
#[must_use]
pub fn external_bundle_root(workspace_root: &Path) -> Option<PathBuf> {
    let config_path = workspace_root.join(".smedja").join("config.toml");
    let content = std::fs::read_to_string(&config_path).ok()?;
    let doc: ConfigDoc = match toml::from_str(&content) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, path = %config_path.display(), "invalid [bundle] config; ignoring external root");
            return None;
        }
    };
    let root = doc.bundle?.root?;
    let path = PathBuf::from(&root);
    Some(if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    })
}

/// Loads the merged bundle for `workspace_root` (`.smedja/` sources plus any
/// configured external root).
#[must_use]
pub fn load_bundle(workspace_root: &Path) -> Bundle {
    let external = external_bundle_root(workspace_root);
    Bundle::load(workspace_root, external.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_has_no_external_root() {
        let dir = tempfile::tempdir().unwrap();
        assert!(external_bundle_root(dir.path()).is_none());
    }

    #[test]
    fn relative_root_resolves_against_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(
            smedja.join("config.toml"),
            "[bundle]\nroot = \"../toolkit\"\n",
        )
        .unwrap();
        let root = external_bundle_root(dir.path()).expect("root resolved");
        assert_eq!(root, dir.path().join("../toolkit"));
    }

    #[test]
    fn absolute_root_used_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(
            smedja.join("config.toml"),
            "[bundle]\nroot = \"/opt/shared-toolkit\"\n",
        )
        .unwrap();
        assert_eq!(
            external_bundle_root(dir.path()),
            Some(PathBuf::from("/opt/shared-toolkit"))
        );
    }

    #[test]
    fn unparseable_config_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(smedja.join("config.toml"), "[bundle\nroot = ").unwrap();
        assert!(external_bundle_root(dir.path()).is_none());
    }

    #[test]
    fn load_bundle_reads_local_smedja_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join(".smedja/skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(
            skills.join("demo.md"),
            "---\nname: demo\ndescription: A demo skill.\n---\nbody\n",
        )
        .unwrap();
        let bundle = load_bundle(dir.path());
        assert!(bundle.find("demo").is_some());
    }
}

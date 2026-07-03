//! Symlink-based synchronisation of skills from a source bundle.

use std::path::Path;

use super::SkillRegistry;
use crate::error::PluginsError;

/// Result of a [`SkillRegistry::sync_from`] operation.
#[derive(Debug, Default)]
pub struct SyncResult {
    /// Skills newly symlinked into the registry.
    pub linked: usize,
    /// Existing symlinks whose target was updated.
    pub updated: usize,
    /// Skills whose symlink already pointed to the correct source.
    pub skipped: usize,
    /// Entries skipped because a real file/directory (not a symlink) exists at
    /// the target path.  Each tuple is `(entry_name, reason)`.
    pub errors: Vec<(String, String)>,
}

impl SkillRegistry {
    /// Creates symlinks in `skills_dir` for every skill found directly under
    /// `source_dir`.
    ///
    /// For each entry in `source_dir`:
    /// - A directory containing `SKILL.md` is symlinked as
    ///   `skills_dir/<dirname>` → `<abs source>/<dirname>`.
    /// - A flat `.md` file is symlinked as
    ///   `skills_dir/<filename>` → `<abs source>/<filename>`.
    ///
    /// Existing symlinks pointing to the right target are skipped.  Stale
    /// symlinks are replaced.  Real files or directories are not clobbered —
    /// they are recorded in [`SyncResult::errors`].
    ///
    /// # Errors
    ///
    /// Returns [`PluginsError::Io`] when `source_dir` cannot be read or when
    /// the registry directory cannot be created.
    pub fn sync_from(&self, source_dir: &Path) -> Result<SyncResult, PluginsError> {
        std::fs::create_dir_all(&self.skills_dir)?;

        let abs_source = source_dir
            .canonicalize()
            .unwrap_or_else(|_| source_dir.to_owned());

        let mut result = SyncResult::default();

        for entry in std::fs::read_dir(&abs_source)? {
            let entry = entry?;
            let src = entry.path();
            let file_name = entry.file_name();
            let file_name_str = file_name.to_string_lossy();

            let is_skill = if src.is_dir() {
                src.join("SKILL.md").exists()
            } else {
                src.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            };

            if !is_skill {
                continue;
            }

            let dst = self.skills_dir.join(&file_name);

            // dst exists or is a dangling symlink
            if dst.symlink_metadata().is_ok() {
                if dst.is_symlink() {
                    let current = std::fs::read_link(&dst).ok();
                    if current.as_deref() == Some(src.as_path()) {
                        tracing::debug!(name = %file_name_str, "already up to date");
                        result.skipped += 1;
                        continue;
                    }
                    // Stale symlink — remove and recreate.
                    std::fs::remove_file(&dst)?;
                    std::os::unix::fs::symlink(&src, &dst)?;
                    tracing::debug!(name = %file_name_str, "updated symlink");
                    result.updated += 1;
                } else {
                    // Real file/dir — do not clobber.
                    let reason =
                        "a real file or directory exists at this path (not a symlink)".to_owned();
                    tracing::warn!(name = %file_name_str, %reason, "skipping");
                    result.errors.push((file_name_str.into_owned(), reason));
                }
            } else {
                std::os::unix::fs::symlink(&src, &dst)?;
                tracing::debug!(name = %file_name_str, "linked");
                result.linked += 1;
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use crate::registry::SkillRegistry;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    fn tmp_registry() -> (tempfile::TempDir, SkillRegistry) {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = SkillRegistry::new(dir.path());
        (dir, registry)
    }

    fn valid_skill_content(name: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: A test skill for {name}.\nmetadata:\n  version: \"0.1.0\"\n  trigger_phrases:\n    - {name}\n---\n# {name} body\n"
        )
    }

    fn make_bundle_skill(bundle_skills: &std::path::Path, name: &str) {
        let dir = bundle_skills.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), valid_skill_content(name)).unwrap();
    }

    #[test]
    fn sync_links_directory_skill() {
        let bundle = tempfile::tempdir().unwrap();
        let bundle_skills = bundle.path().join("skills");
        std::fs::create_dir_all(&bundle_skills).unwrap();
        make_bundle_skill(&bundle_skills, "alpha");

        let (_reg_dir, registry) = tmp_registry();
        let result = registry.sync_from(&bundle_skills).unwrap();

        assert_eq!(result.linked, 1);
        assert_eq!(result.updated, 0);
        assert_eq!(result.skipped, 0);
        assert!(registry.skills_dir.join("alpha").is_symlink());
    }

    #[test]
    fn sync_links_flat_file_skill() {
        let bundle = tempfile::tempdir().unwrap();
        let bundle_skills = bundle.path().join("skills");
        std::fs::create_dir_all(&bundle_skills).unwrap();
        std::fs::write(bundle_skills.join("flat.md"), valid_skill_content("flat")).unwrap();

        let (_reg_dir, registry) = tmp_registry();
        let result = registry.sync_from(&bundle_skills).unwrap();

        assert_eq!(result.linked, 1);
        assert!(registry.skills_dir.join("flat.md").is_symlink());
    }

    #[test]
    fn sync_skips_already_correct_symlink() {
        let bundle = tempfile::tempdir().unwrap();
        let bundle_skills = bundle.path().join("skills");
        std::fs::create_dir_all(&bundle_skills).unwrap();
        make_bundle_skill(&bundle_skills, "beta");

        let (_reg_dir, registry) = tmp_registry();
        registry.sync_from(&bundle_skills).unwrap();
        let result = registry.sync_from(&bundle_skills).unwrap();

        assert_eq!(result.linked, 0);
        assert_eq!(result.updated, 0);
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn sync_updates_stale_symlink() {
        let bundle1 = tempfile::tempdir().unwrap();
        let bundle2 = tempfile::tempdir().unwrap();

        for bundle in [&bundle1, &bundle2] {
            let skills = bundle.path().join("skills");
            std::fs::create_dir_all(&skills).unwrap();
            make_bundle_skill(&skills, "gamma");
        }

        let (_reg_dir, registry) = tmp_registry();
        registry.sync_from(&bundle1.path().join("skills")).unwrap();
        let result = registry.sync_from(&bundle2.path().join("skills")).unwrap();

        assert_eq!(result.updated, 1);
        assert_eq!(result.linked, 0);
    }

    #[test]
    fn sync_errors_on_real_directory_collision() {
        let bundle = tempfile::tempdir().unwrap();
        let bundle_skills = bundle.path().join("skills");
        std::fs::create_dir_all(&bundle_skills).unwrap();
        make_bundle_skill(&bundle_skills, "delta");

        let (_reg_dir, registry) = tmp_registry();
        // Plant a real directory at the target path before syncing.
        std::fs::create_dir_all(registry.skills_dir.join("delta")).unwrap();

        let result = registry.sync_from(&bundle_skills).unwrap();
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].0, "delta");
        assert_eq!(result.linked, 0);
    }

    #[test]
    fn sync_ignores_non_skill_entries() {
        let bundle = tempfile::tempdir().unwrap();
        let bundle_skills = bundle.path().join("skills");
        std::fs::create_dir_all(&bundle_skills).unwrap();
        // A directory with no SKILL.md.
        std::fs::create_dir_all(bundle_skills.join("not-a-skill")).unwrap();
        // A non-.md file.
        std::fs::write(bundle_skills.join("README.txt"), "hello").unwrap();
        // One real skill.
        make_bundle_skill(&bundle_skills, "real");

        let (_reg_dir, registry) = tmp_registry();
        let result = registry.sync_from(&bundle_skills).unwrap();

        assert_eq!(result.linked, 1);
    }
}

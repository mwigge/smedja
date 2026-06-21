//! [`SkillRegistry`] — scans, finds, installs, updates and removes skills.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::PluginsError;
use crate::parse::parse_skill;
use crate::types::Skill;

/// Manages Claude Code skill files stored under a skills directory.
///
/// Skills are stored as either:
/// - `<skills_dir>/<name>/SKILL.md` (directory-based), or
/// - `<skills_dir>/<name>.md` (flat file).
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    skills_dir: PathBuf,
}

impl SkillRegistry {
    /// Opens the registry rooted at `skills_dir`. Does not scan immediately.
    pub fn new(skills_dir: impl Into<PathBuf>) -> Self {
        Self {
            skills_dir: skills_dir.into(),
        }
    }

    /// Returns `~/.claude/skills` as the default registry path.
    ///
    /// # Panics
    ///
    /// Panics when the home directory cannot be determined (i.e. `$HOME` is
    /// unset). This is intentional: a tool that cannot locate its own config
    /// directory has no safe fallback.
    #[must_use]
    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .expect("HOME environment variable must be set");
        PathBuf::from(home).join(".claude").join("skills")
    }

    /// Scans `skills_dir` for skill files and returns all successfully parsed
    /// [`Skill`] values.
    ///
    /// Files that cannot be parsed are logged at `WARN` level and skipped;
    /// they do not cause the whole scan to fail.
    ///
    /// # Errors
    ///
    /// Returns [`PluginsError::Io`] when the directory cannot be read at all.
    pub fn scan(&self) -> Result<Vec<Skill>, PluginsError> {
        if !self.skills_dir.exists() {
            return Ok(Vec::new());
        }

        let mut skills = Vec::new();

        for entry in WalkDir::new(&self.skills_dir).min_depth(1).max_depth(2) {
            let entry = entry.map_err(|e| std::io::Error::other(e.to_string()))?;

            let path = entry.path();

            if !is_skill_file(path, &self.skills_dir) {
                continue;
            }

            match std::fs::read_to_string(path) {
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "could not read skill file");
                }
                Ok(content) => match parse_skill(&content, path) {
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "could not parse skill file");
                    }
                    Ok(skill) => skills.push(skill),
                },
            }
        }

        Ok(skills)
    }

    /// Finds a skill by exact name match (case-insensitive).
    ///
    /// Returns `Ok(None)` when no matching skill is found.
    ///
    /// # Errors
    ///
    /// Returns [`PluginsError::Io`] when the skills directory cannot be read.
    pub fn find(&self, name: &str) -> Result<Option<Skill>, PluginsError> {
        let name_lower = name.to_lowercase();
        let skills = self.scan()?;
        Ok(skills
            .into_iter()
            .find(|s| s.manifest.name.to_lowercase() == name_lower))
    }

    /// Installs a new skill by writing `content` to
    /// `skills_dir/<name>/SKILL.md`.
    ///
    /// # Errors
    ///
    /// - [`PluginsError::AlreadyExists`] when the skill directory already exists.
    /// - [`PluginsError::Io`] on filesystem errors.
    pub fn install(&self, name: &str, content: &str) -> Result<(), PluginsError> {
        let skill_dir = self.skills_dir.join(name);

        if skill_dir.exists() {
            return Err(PluginsError::AlreadyExists {
                name: name.to_owned(),
                path: skill_dir,
            });
        }

        std::fs::create_dir_all(&skill_dir)?;
        std::fs::write(skill_dir.join("SKILL.md"), content)?;
        Ok(())
    }

    /// Updates an existing skill by overwriting its `SKILL.md`.
    ///
    /// Only the directory-based layout (`<name>/SKILL.md`) is written on
    /// update; flat-file skills are not modified by this method.
    ///
    /// # Errors
    ///
    /// - [`PluginsError::NotFound`] when the skill directory does not exist.
    /// - [`PluginsError::Io`] on filesystem errors.
    pub fn update(&self, name: &str, content: &str) -> Result<(), PluginsError> {
        let skill_dir = self.skills_dir.join(name);

        if !skill_dir.exists() {
            return Err(PluginsError::NotFound {
                name: name.to_owned(),
            });
        }

        std::fs::write(skill_dir.join("SKILL.md"), content)?;
        Ok(())
    }

    /// Removes a skill directory (and all its contents) entirely.
    ///
    /// # Errors
    ///
    /// - [`PluginsError::NotFound`] when neither the directory nor a flat
    ///   `.md` file exists for the given name.
    /// - [`PluginsError::Io`] on filesystem errors.
    pub fn remove(&self, name: &str) -> Result<(), PluginsError> {
        let skill_dir = self.skills_dir.join(name);
        let flat_file = self.skills_dir.join(format!("{name}.md"));

        if skill_dir.exists() {
            std::fs::remove_dir_all(&skill_dir)?;
        } else if flat_file.exists() {
            std::fs::remove_file(&flat_file)?;
        } else {
            return Err(PluginsError::NotFound {
                name: name.to_owned(),
            });
        }

        Ok(())
    }
}

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

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Returns `true` when `path` is a skill file that should be parsed.
///
/// Accepted patterns:
/// - `<skills_dir>/<name>/SKILL.md`  (depth 2, filename `SKILL.md`)
/// - `<skills_dir>/<name>.md`        (depth 1, `.md` extension)
fn is_skill_file(path: &Path, skills_dir: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };

    // Directory-based: skills_dir/<name>/SKILL.md
    if file_name == "SKILL.md" {
        if let Some(parent) = path.parent() {
            return parent != skills_dir && parent.parent() == Some(skills_dir);
        }
    }

    // Flat file: skills_dir/<name>.md
    if std::path::Path::new(file_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        && path.parent() == Some(skills_dir)
    {
        return true;
    }

    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::SkillRegistry;
    use crate::error::PluginsError;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    fn tmp_registry() -> (TempDir, SkillRegistry) {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = SkillRegistry::new(dir.path());
        (dir, registry)
    }

    fn valid_skill_content(name: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: A test skill for {name}.\nmetadata:\n  version: \"0.1.0\"\n  trigger_phrases:\n    - {name}\n---\n# {name} body\n"
        )
    }

    fn install_skill(registry: &SkillRegistry, name: &str) -> PathBuf {
        let content = valid_skill_content(name);
        registry.install(name, &content).expect("install");
        registry.skills_dir.join(name).join("SKILL.md")
    }

    // -----------------------------------------------------------------------
    // scan
    // -----------------------------------------------------------------------

    #[test]
    fn scan_finds_valid_directory_based_skill() {
        let (_dir, registry) = tmp_registry();
        install_skill(&registry, "alpha");

        let skills = registry.scan().expect("scan");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].manifest.name, "alpha");
    }

    #[test]
    fn scan_returns_empty_when_directory_does_not_exist() {
        let registry = SkillRegistry::new("/tmp/nonexistent_smedja_skills_dir_12345");
        let skills = registry.scan().expect("scan");
        assert!(skills.is_empty());
    }

    #[test]
    fn scan_skips_unparseable_file_and_returns_rest() {
        let (_dir, registry) = tmp_registry();

        // Install a good skill.
        install_skill(&registry, "good");

        // Plant a bad SKILL.md manually.
        let bad_dir = registry.skills_dir.join("bad");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("SKILL.md"), "not valid frontmatter at all").unwrap();

        let skills = registry.scan().expect("scan should not fail entirely");
        assert_eq!(skills.len(), 1, "only the good skill should be returned");
        assert_eq!(skills[0].manifest.name, "good");
    }

    #[test]
    fn scan_discovers_flat_file_skill() {
        let (_dir, registry) = tmp_registry();

        // Write a flat file directly inside skills_dir.
        let flat_content = valid_skill_content("flat");
        std::fs::write(registry.skills_dir.join("flat.md"), &flat_content).unwrap();

        let skills = registry.scan().expect("scan");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].manifest.name, "flat");
    }

    // -----------------------------------------------------------------------
    // find
    // -----------------------------------------------------------------------

    #[test]
    fn find_returns_skill_by_exact_name() {
        let (_dir, registry) = tmp_registry();
        install_skill(&registry, "needle");
        install_skill(&registry, "haystack");

        let found = registry
            .find("needle")
            .expect("find")
            .expect("skill present");
        assert_eq!(found.manifest.name, "needle");
    }

    #[test]
    fn find_is_case_insensitive() {
        let (_dir, registry) = tmp_registry();
        install_skill(&registry, "MySkill");

        let found = registry
            .find("myskill")
            .expect("find")
            .expect("skill present");
        assert_eq!(found.manifest.name, "MySkill");
    }

    #[test]
    fn find_returns_none_for_missing_skill() {
        let (_dir, registry) = tmp_registry();
        install_skill(&registry, "present");

        let result = registry.find("absent").expect("find");
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // install
    // -----------------------------------------------------------------------

    #[test]
    fn install_creates_skill_md_file() {
        let (_dir, registry) = tmp_registry();
        let content = valid_skill_content("beta");

        registry.install("beta", &content).expect("install");

        let skill_path = registry.skills_dir.join("beta").join("SKILL.md");
        assert!(skill_path.exists(), "SKILL.md must exist after install");
        let on_disk = std::fs::read_to_string(&skill_path).unwrap();
        assert_eq!(on_disk, content);
    }

    #[test]
    fn install_fails_with_already_exists_on_second_call() {
        let (_dir, registry) = tmp_registry();
        let content = valid_skill_content("gamma");

        registry.install("gamma", &content).expect("first install");

        let err = registry
            .install("gamma", &content)
            .expect_err("second install must fail");
        assert!(
            matches!(err, PluginsError::AlreadyExists { ref name, .. } if name == "gamma"),
            "expected AlreadyExists, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // update
    // -----------------------------------------------------------------------

    #[test]
    fn update_overwrites_existing_skill() {
        let (_dir, registry) = tmp_registry();
        install_skill(&registry, "delta");

        let new_content = valid_skill_content("delta").replace("0.1.0", "0.2.0");
        registry.update("delta", &new_content).expect("update");

        let on_disk =
            std::fs::read_to_string(registry.skills_dir.join("delta").join("SKILL.md")).unwrap();
        assert!(on_disk.contains("0.2.0"));
    }

    #[test]
    fn update_fails_with_not_found_on_nonexistent_skill() {
        let (_dir, registry) = tmp_registry();

        let err = registry
            .update("ghost", "anything")
            .expect_err("update must fail");
        assert!(
            matches!(err, PluginsError::NotFound { ref name } if name == "ghost"),
            "expected NotFound, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // remove
    // -----------------------------------------------------------------------

    #[test]
    fn remove_deletes_skill_directory() {
        let (_dir, registry) = tmp_registry();
        install_skill(&registry, "epsilon");

        let skill_dir = registry.skills_dir.join("epsilon");
        assert!(skill_dir.exists());

        registry.remove("epsilon").expect("remove");
        assert!(!skill_dir.exists(), "directory must be gone after remove");
    }

    #[test]
    fn remove_fails_with_not_found_when_skill_absent() {
        let (_dir, registry) = tmp_registry();

        let err = registry.remove("phantom").expect_err("remove must fail");
        assert!(
            matches!(err, PluginsError::NotFound { ref name } if name == "phantom"),
            "expected NotFound, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // body content
    // -----------------------------------------------------------------------

    #[test]
    fn body_content_is_correctly_split_from_frontmatter() {
        let (_dir, registry) = tmp_registry();

        let content = "---\nname: zeta\ndescription: Body test.\n---\nHello body\nSecond line\n";
        registry.install("zeta", content).expect("install");

        let skill = registry.find("zeta").expect("find").expect("present");
        assert_eq!(skill.body.trim(), "Hello body\nSecond line");
    }

    // -----------------------------------------------------------------------
    // sync_from
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // default_path smoke test
    // -----------------------------------------------------------------------

    #[test]
    fn default_path_ends_with_claude_skills() {
        let path = SkillRegistry::default_path();
        let components: Vec<_> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert!(
            components
                .windows(2)
                .any(|w| w[0] == ".claude" && w[1] == "skills"),
            "default path must end with .claude/skills, got: {}",
            path.display()
        );
    }
}

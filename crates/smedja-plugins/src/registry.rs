//! [`SkillRegistry`] — scans, finds, installs, updates and removes skills.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::PluginsError;
use crate::parse::parse_skill;
use crate::types::Skill;

/// Manages skill files stored under a skills directory.
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

    /// Returns the home directory, preferring `$HOME` and falling back to
    /// `$USERPROFILE` on Windows.
    fn home_dir() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .expect("HOME environment variable must be set");
        PathBuf::from(home)
    }

    /// Returns `~/.config/smedja/skills` as the default registry path.
    ///
    /// If that directory does not exist but the legacy `~/.claude/skills`
    /// directory does, the legacy path is returned so existing skill libraries
    /// keep working.
    #[must_use]
    pub fn default_path() -> PathBuf {
        Self::default_path_in(Self::home_dir())
    }

    /// Returns the default registry path under `home`.
    ///
    /// Exposed for tests so they do not depend on the developer's actual
    /// home-directory layout.
    #[must_use]
    pub(crate) fn default_path_in(home: impl AsRef<Path>) -> PathBuf {
        let home = home.as_ref();
        let modern = home.join(".config").join("smedja").join("skills");
        let legacy = home.join(".claude").join("skills");
        if modern.exists() || !legacy.exists() {
            modern
        } else {
            legacy
        }
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
        validate_name(name)?;
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
        validate_name(name)?;
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
        validate_name(name)?;
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

/// Returns `true` when `name` is exactly one normal path component.
///
/// Rejects anything that could escape the registry directory when joined:
/// empty strings, `.`, `..`, absolute paths, and any name containing a path
/// separator (e.g. `a/b`, `../etc`). This is the guard that keeps
/// install/update/remove from writing or deleting outside `skills_dir`.
fn is_single_normal_component(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

/// Validates a skill `name`, returning [`PluginsError::InvalidName`] when it is
/// not a single normal path component.
fn validate_name(name: &str) -> Result<(), PluginsError> {
    if is_single_normal_component(name) {
        Ok(())
    } else {
        Err(PluginsError::InvalidName {
            name: name.to_owned(),
        })
    }
}

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
    // path-traversal guard
    // -----------------------------------------------------------------------

    /// Names that must be rejected because they are not a single normal
    /// component and could escape `skills_dir` when joined.
    const TRAVERSAL_NAMES: &[&str] = &["../etc", "a/b", "/abs", "..", ".", ""];

    #[test]
    fn install_rejects_traversal_names() {
        let (_dir, registry) = tmp_registry();
        for name in TRAVERSAL_NAMES {
            let err = registry
                .install(name, "content")
                .expect_err("install must reject traversal name");
            assert!(
                matches!(&err, PluginsError::InvalidName { name: n } if n == name),
                "expected InvalidName for {name:?}, got: {err}"
            );
        }
    }

    #[test]
    fn update_rejects_traversal_names() {
        let (_dir, registry) = tmp_registry();
        for name in TRAVERSAL_NAMES {
            let err = registry
                .update(name, "content")
                .expect_err("update must reject traversal name");
            assert!(
                matches!(&err, PluginsError::InvalidName { name: n } if n == name),
                "expected InvalidName for {name:?}, got: {err}"
            );
        }
    }

    #[test]
    fn remove_rejects_traversal_names() {
        let (_dir, registry) = tmp_registry();
        for name in TRAVERSAL_NAMES {
            let err = registry
                .remove(name)
                .expect_err("remove must reject traversal name");
            assert!(
                matches!(&err, PluginsError::InvalidName { name: n } if n == name),
                "expected InvalidName for {name:?}, got: {err}"
            );
        }
    }

    #[test]
    fn normal_name_still_works_through_install_update_remove() {
        let (_dir, registry) = tmp_registry();
        registry
            .install("myskill", &valid_skill_content("myskill"))
            .expect("install normal name");
        registry
            .update("myskill", &valid_skill_content("myskill"))
            .expect("update normal name");
        registry.remove("myskill").expect("remove normal name");
    }

    #[test]
    fn traversal_name_does_not_touch_files_outside_skills_dir() {
        // Layout: <root>/skills is the registry; <root>/outside.md is a sibling
        // file that a `../outside` traversal would target. It must survive.
        let root = tempfile::tempdir().expect("tempdir");
        let skills_dir = root.path().join("skills");
        std::fs::create_dir_all(&skills_dir).expect("create skills_dir");
        let outside = root.path().join("outside.md");
        std::fs::write(&outside, "do not delete me").expect("write outside file");

        let registry = SkillRegistry::new(&skills_dir);
        let traversal = "../outside";

        // install must not create/overwrite the outside file.
        assert!(registry.install(traversal, "clobber").is_err());
        // update must not overwrite the outside file.
        assert!(registry.update(traversal, "clobber").is_err());
        // remove must not delete the outside file.
        assert!(registry.remove(traversal).is_err());

        assert!(outside.exists(), "outside file must still exist");
        assert_eq!(
            std::fs::read_to_string(&outside).expect("read outside"),
            "do not delete me",
            "outside file must be untouched"
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
    // default_path smoke tests
    // -----------------------------------------------------------------------

    #[test]
    fn default_path_prefers_smedja_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let path = SkillRegistry::default_path_in(tmp.path());
        assert_eq!(
            path,
            tmp.path().join(".config").join("smedja").join("skills")
        );
    }

    #[test]
    fn default_path_falls_back_to_legacy_claude_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join(".claude").join("skills");
        std::fs::create_dir_all(&legacy).unwrap();
        let path = SkillRegistry::default_path_in(tmp.path());
        assert_eq!(path, legacy);
    }
}

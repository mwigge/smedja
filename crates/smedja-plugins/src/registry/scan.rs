//! Scanning and lookup of skill files on disk.

use std::path::Path;

use walkdir::WalkDir;

use super::SkillRegistry;
use crate::error::PluginsError;
use crate::parse::parse_skill;
use crate::types::Skill;

impl SkillRegistry {
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

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::registry::SkillRegistry;

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

    fn install_skill(registry: &SkillRegistry, name: &str) {
        let content = valid_skill_content(name);
        registry.install(name, &content).expect("install");
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
}

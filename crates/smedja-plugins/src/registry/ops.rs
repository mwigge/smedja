//! Mutating operations: install, update and remove skills.

use super::SkillRegistry;
use crate::error::PluginsError;

impl SkillRegistry {
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

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::error::PluginsError;
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
}

//! `smj skill` — manage Claude Code skill files.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::Subcommand;
use smedja_plugins::SkillRegistry;

#[derive(Subcommand)]
pub(crate) enum SkillCmd {
    /// List installed skills
    List,
    /// Install a skill from a SKILL.md file or directory
    Install {
        /// Path to a SKILL.md file or a directory containing one
        path: PathBuf,
    },
    /// Update an existing skill from a SKILL.md file or directory
    Update {
        /// Skill name to update
        name: String,
        /// Path to the new SKILL.md file or a directory containing one
        path: PathBuf,
    },
    /// Remove an installed skill
    Remove {
        /// Skill name to remove
        name: String,
    },
    /// Sync all skills from a bundle directory using symlinks
    Sync {
        /// Path to a directory of skills (e.g. agent-toolkit-bundle/skills)
        path: PathBuf,
    },
    /// Create .codex/skills and .cursor/skills symlinks pointing to ~/.claude/skills
    LinkIdes {
        /// Project directory to link into (default: current directory)
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
}

/// Dispatches a `smj skill` subcommand.
pub(crate) fn run(action: SkillCmd) -> Result<()> {
    let registry = SkillRegistry::new(SkillRegistry::default_path());
    match action {
        SkillCmd::List => cmd_skill_list(&registry)?,
        SkillCmd::Install { path } => cmd_skill_install(&registry, &path)?,
        SkillCmd::Update { name, path } => cmd_skill_update(&registry, &name, &path)?,
        SkillCmd::Remove { name } => cmd_skill_remove(&registry, &name)?,
        SkillCmd::Sync { path } => cmd_skill_sync(&registry, &path)?,
        SkillCmd::LinkIdes { dir } => {
            cmd_skill_link_ides(&SkillRegistry::default_path(), &dir)?;
        }
    }
    Ok(())
}

fn cmd_skill_list(registry: &SkillRegistry) -> Result<()> {
    let skills = registry.scan()?;
    if skills.is_empty() {
        println!(
            "No skills installed at {}",
            SkillRegistry::default_path().display()
        );
        return Ok(());
    }
    println!("{:<30} {:<10} DESCRIPTION", "NAME", "VERSION");
    println!("{}", "-".repeat(80));
    for skill in &skills {
        let version = skill.manifest.version.as_deref().unwrap_or("-");
        let desc = skill
            .manifest
            .description
            .lines()
            .next()
            .unwrap_or("")
            .trim();
        println!("{:<30} {:<10} {}", skill.manifest.name, version, desc);
    }
    println!("\n{} skill(s) installed", skills.len());
    Ok(())
}

fn cmd_skill_install(registry: &SkillRegistry, path: &Path) -> Result<()> {
    let (name, content) = read_skill_file(path)?;
    registry
        .install(&name, &content)
        .with_context(|| format!("failed to install skill `{name}`"))?;
    println!("Installed skill `{name}`");
    Ok(())
}

fn cmd_skill_update(registry: &SkillRegistry, name: &str, path: &Path) -> Result<()> {
    let (_parsed_name, content) = read_skill_file(path)?;
    registry
        .update(name, &content)
        .with_context(|| format!("failed to update skill `{name}`"))?;
    println!("Updated skill `{name}`");
    Ok(())
}

fn cmd_skill_remove(registry: &SkillRegistry, name: &str) -> Result<()> {
    registry
        .remove(name)
        .with_context(|| format!("failed to remove skill `{name}`"))?;
    println!("Removed skill `{name}`");
    Ok(())
}

fn cmd_skill_link_ides(skills_src: &Path, project_dir: &Path) -> Result<()> {
    for ide in &[".codex", ".cursor"] {
        let ide_dir = project_dir.join(ide);
        std::fs::create_dir_all(&ide_dir)
            .with_context(|| format!("cannot create {}", ide_dir.display()))?;
        let link = ide_dir.join("skills");
        if link.is_symlink() {
            if std::fs::read_link(&link).is_ok_and(|t| t == skills_src) {
                println!("  skip: {}", link.display());
                continue;
            }
            std::fs::remove_file(&link)
                .with_context(|| format!("cannot replace {}", link.display()))?;
        }
        std::os::unix::fs::symlink(skills_src, &link)
            .with_context(|| format!("cannot symlink {}", link.display()))?;
        println!("  linked: {}", link.display());
    }
    Ok(())
}

fn cmd_skill_sync(registry: &SkillRegistry, path: &Path) -> Result<()> {
    println!("Syncing from {} ...", path.display());
    let r = registry
        .sync_from(path)
        .with_context(|| format!("sync failed from {}", path.display()))?;
    for (name, reason) in &r.errors {
        println!("  error:   {name} — {reason}");
    }
    println!(
        "\n{} linked, {} updated, {} skipped, {} error(s)",
        r.linked,
        r.updated,
        r.skipped,
        r.errors.len()
    );
    Ok(())
}

/// Resolves a path to its SKILL.md content and the skill name from frontmatter.
fn read_skill_file(path: &Path) -> Result<(String, String)> {
    let skill_md = if path.is_dir() {
        path.join("SKILL.md")
    } else {
        path.to_owned()
    };
    let content = std::fs::read_to_string(&skill_md)
        .with_context(|| format!("cannot read {}", skill_md.display()))?;
    let skill = smedja_plugins::parse_skill(&content, &skill_md)
        .with_context(|| format!("invalid frontmatter in {}", skill_md.display()))?;
    Ok((skill.manifest.name, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_ides_creates_codex_and_cursor_symlinks() {
        let tmp = tempfile::tempdir().expect("tmp");
        let skills_src = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_src).unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        cmd_skill_link_ides(&skills_src, &project).expect("link_ides");

        let codex_link = project.join(".codex").join("skills");
        let cursor_link = project.join(".cursor").join("skills");
        assert!(codex_link.is_symlink(), ".codex/skills must be a symlink");
        assert!(cursor_link.is_symlink(), ".cursor/skills must be a symlink");
        assert_eq!(std::fs::read_link(&codex_link).unwrap(), skills_src);
        assert_eq!(std::fs::read_link(&cursor_link).unwrap(), skills_src);
    }

    #[test]
    fn link_ides_skips_existing_correct_symlinks() {
        let tmp = tempfile::tempdir().expect("tmp");
        let skills_src = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_src).unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        // Create first time.
        cmd_skill_link_ides(&skills_src, &project).expect("first");
        // Second call must not error.
        cmd_skill_link_ides(&skills_src, &project).expect("second");

        let codex_link = project.join(".codex").join("skills");
        assert_eq!(std::fs::read_link(&codex_link).unwrap(), skills_src);
    }
}

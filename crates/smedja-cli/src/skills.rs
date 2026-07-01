use super::*;

pub(crate) fn dispatch_skill(action: SkillCmd) -> Result<()> {
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

pub(crate) fn cmd_skill_list(registry: &SkillRegistry) -> Result<()> {
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

pub(crate) fn cmd_skill_install(registry: &SkillRegistry, path: &std::path::Path) -> Result<()> {
    let (name, content) = read_skill_file(path)?;
    registry
        .install(&name, &content)
        .with_context(|| format!("failed to install skill `{name}`"))?;
    println!("Installed skill `{name}`");
    Ok(())
}

pub(crate) fn cmd_skill_update(
    registry: &SkillRegistry,
    name: &str,
    path: &std::path::Path,
) -> Result<()> {
    let (_parsed_name, content) = read_skill_file(path)?;
    registry
        .update(name, &content)
        .with_context(|| format!("failed to update skill `{name}`"))?;
    println!("Updated skill `{name}`");
    Ok(())
}

pub(crate) fn cmd_skill_remove(registry: &SkillRegistry, name: &str) -> Result<()> {
    registry
        .remove(name)
        .with_context(|| format!("failed to remove skill `{name}`"))?;
    println!("Removed skill `{name}`");
    Ok(())
}

pub(crate) fn cmd_skill_link_ides(
    skills_src: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<()> {
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

pub(crate) fn cmd_skill_sync(registry: &SkillRegistry, path: &std::path::Path) -> Result<()> {
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

pub(crate) fn read_skill_file(path: &std::path::Path) -> Result<(String, String)> {
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

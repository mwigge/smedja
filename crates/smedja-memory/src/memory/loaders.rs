//! Workspace skill, role, and context loaders plus injection helpers.

use super::WorkingMemory;

/// Loads role-specific rules/skills for `role` from a workspace: the file
/// `<dir>/.smedja/roles/<role>.md` and every `*.md` under
/// `<dir>/.smedja/roles/<role>/`. Returns their contents (the single file first,
/// then the directory's files sorted by name), or an empty vec when none exist.
///
/// This binds a set of rules/skills to each agent role — the orchestrator
/// injects them whenever that role is active, alongside the workspace skills.
///
/// # Errors
///
/// Returns an `io::Error` if a present file cannot be read.
pub fn load_role_skills(dir: &std::path::Path, role: &str) -> Result<Vec<String>, std::io::Error> {
    let roles_dir = dir.join(".smedja").join("roles");
    let mut out = Vec::new();

    let single = roles_dir.join(format!("{role}.md"));
    if single.is_file() {
        out.push(std::fs::read_to_string(&single)?);
    }

    let role_specific_dir = roles_dir.join(role);
    if role_specific_dir.is_dir() {
        let mut files: Vec<(std::path::PathBuf, String)> = Vec::new();
        for entry in std::fs::read_dir(&role_specific_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let content = std::fs::read_to_string(&path)?;
                files.push((path, content));
            }
        }
        files.sort_by(|(a, _), (b, _)| a.file_name().cmp(&b.file_name()));
        out.extend(files.into_iter().map(|(_, c)| c));
    }

    Ok(out)
}

/// Loads workspace skill files from `<dir>/.smedja/skills/*.md`.
///
/// Returns an empty [`Vec`] when the directory is absent or no `.md` files
/// are present — this is not an error.
///
/// # Errors
///
/// Returns an error only if the directory exists but cannot be read.
pub fn load_workspace_skills(dir: &std::path::Path) -> Result<Vec<String>, std::io::Error> {
    let skills_dir = dir.join(".smedja").join("skills");
    if !skills_dir.exists() {
        return Ok(Vec::new());
    }
    let mut skills: Vec<(std::path::PathBuf, String)> = Vec::new();
    for entry in std::fs::read_dir(&skills_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let content = std::fs::read_to_string(&path)?;
            skills.push((path, content));
        }
    }
    skills.sort_by(|(a, _), (b, _)| a.file_name().cmp(&b.file_name()));
    Ok(skills.into_iter().map(|(_, c)| c).collect())
}

/// Reads project-specific context files from `.smedja/context/*.md` in `dir`.
///
/// Returns an empty `Vec` when the directory does not exist. Files are sorted
/// alphabetically so injection order is deterministic across runs.
///
/// # Errors
///
/// Returns an `io::Error` if the directory exists but cannot be read, or if any
/// `.md` file cannot be read.
pub fn load_context_files(dir: &std::path::Path) -> Result<Vec<String>, std::io::Error> {
    let ctx_dir = dir.join(".smedja").join("context");
    if !ctx_dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<(std::path::PathBuf, String)> = Vec::new();
    for entry in std::fs::read_dir(&ctx_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let content = std::fs::read_to_string(&path)?;
            files.push((path, content));
        }
    }
    files.sort_by(|(a, _), (b, _)| a.file_name().cmp(&b.file_name()));
    Ok(files.into_iter().map(|(_, c)| c).collect())
}

/// Injects workspace skills into `WorkingMemory` as a single system message
/// before `seal_prefix` is called.
///
/// Skips injection when no skills are found. Returns the number of skills injected.
///
/// # Errors
///
/// Returns an error if the skills directory exists but cannot be read.
pub fn inject_workspace_skills(
    memory: &mut WorkingMemory,
    workspace_dir: &std::path::Path,
) -> Result<usize, std::io::Error> {
    let skills = load_workspace_skills(workspace_dir)?;
    if skills.is_empty() {
        return Ok(0);
    }
    let count = skills.len();
    let combined = skills
        .iter()
        .enumerate()
        .map(|(i, body)| crate::skills::wrap_skill_body(&format!("skill-{i}"), body))
        .collect::<Vec<_>>()
        .join("\n\n");
    memory.push(crate::types::Message::system(format!(
        "[workspace skills]\n\n{combined}"
    )));
    Ok(count)
}

/// Reads `AGENTS.md` from the workspace root, if present.
///
/// Returns `None` when the file is absent — not an error.
///
/// # Errors
///
/// Returns an error only if the file exists but cannot be read.
pub fn detect_agents_md(
    workspace_root: &std::path::Path,
) -> Result<Option<String>, std::io::Error> {
    let path = workspace_root.join("AGENTS.md");
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)?;
    Ok(Some(content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    #[test]
    fn load_skills_empty_when_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let result = super::load_workspace_skills(tmp.path()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn load_role_skills_reads_file_and_dir_for_the_role_only() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = tmp.path().join(".smedja").join("roles");
        std::fs::create_dir_all(roles.join("review")).unwrap();
        std::fs::write(roles.join("review.md"), b"top review rule").unwrap();
        std::fs::write(roles.join("review").join("a_extra.md"), b"extra A").unwrap();
        std::fs::write(roles.join("plan.md"), b"a plan rule").unwrap();

        let review = super::load_role_skills(tmp.path(), "review").unwrap();
        assert_eq!(
            review,
            vec!["top review rule".to_owned(), "extra A".to_owned()]
        );

        // A role with no pack yields nothing; an unrelated role isn't mixed in.
        assert!(super::load_role_skills(tmp.path(), "research")
            .unwrap()
            .is_empty());
        assert_eq!(
            super::load_role_skills(tmp.path(), "plan").unwrap(),
            vec!["a plan rule".to_owned()]
        );
    }

    #[test]
    fn load_skills_reads_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".smedja").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(skills_dir.join("alpha.md"), "skill alpha").unwrap();
        std::fs::write(skills_dir.join("beta.md"), "skill beta").unwrap();
        let mut result = super::load_workspace_skills(tmp.path()).unwrap();
        result.sort();
        assert_eq!(result, vec!["skill alpha", "skill beta"]);
    }

    #[test]
    fn load_skills_ignores_non_md() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".smedja").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(skills_dir.join("skill.md"), "md content").unwrap();
        std::fs::write(skills_dir.join("readme.txt"), "txt content").unwrap();
        let result = super::load_workspace_skills(tmp.path()).unwrap();
        assert_eq!(result, vec!["md content"]);
    }

    #[test]
    fn detect_agents_md_absent_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let result = detect_agents_md(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn inject_workspace_skills_pushes_system_message() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".smedja").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(skills_dir.join("skill.md"), "do something").unwrap();
        let mut mem = WorkingMemory::new(4096);
        let n = inject_workspace_skills(&mut mem, tmp.path()).unwrap();
        assert_eq!(n, 1);
        assert_eq!(mem.len(), 1);
        assert!(mem.messages()[0].content.contains("workspace skills"));
    }

    #[test]
    fn inject_workspace_skills_empty_dir_no_push() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mem = WorkingMemory::new(4096);
        let n = inject_workspace_skills(&mut mem, tmp.path()).unwrap();
        assert_eq!(n, 0);
        assert!(mem.is_empty());
    }

    // --- smoke test equivalent (L66) ---

    #[test]
    fn smoke_l66_skill_injected_before_stable_prefix_watermark() {
        // Smoke L66: smj workspace skills add docs/conventions.md; start session;
        // skill content appears before stable_prefix watermark.
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".smedja").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("conventions.md"),
            "## Coding Conventions\nUse snake_case.",
        )
        .unwrap();

        let mut mem = WorkingMemory::new(4096);
        // Inject skills before sealing, as the session-start flow does.
        let n = super::inject_workspace_skills(&mut mem, tmp.path()).unwrap();
        assert_eq!(n, 1, "one skill file must be injected");
        // Seal the prefix to mark the stable boundary.
        mem.seal_prefix();
        // Push a user turn to simulate session activity.
        mem.push(Message::user("hello"));

        // The skill message must be at index 0 (before the watermark).
        let msgs = mem.messages();
        assert!(
            msgs[0].content.contains("Coding Conventions"),
            "skill content must appear in the first message (before stable_prefix)"
        );
        // stable_prefix == 1 means the skill message is the only frozen entry.
        assert_eq!(
            mem.stable_prefix(),
            1,
            "stable_prefix must be 1 (skill message sealed before user turns)"
        );
        // The mutable window must not contain the skill content.
        let mutable = mem.mutable_window();
        assert!(
            !mutable[0].content.contains("Coding Conventions"),
            "skill must not appear in the mutable window after sealing"
        );
    }

    #[test]
    fn load_context_files_empty_when_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let result = super::load_context_files(tmp.path()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn load_context_files_reads_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx_dir = tmp.path().join(".smedja").join("context");
        std::fs::create_dir_all(&ctx_dir).unwrap();
        std::fs::write(ctx_dir.join("a.md"), "context A").unwrap();
        std::fs::write(ctx_dir.join("b.md"), "context B").unwrap();
        let mut result = super::load_context_files(tmp.path()).unwrap();
        result.sort();
        assert_eq!(result, vec!["context A", "context B"]);
    }

    #[test]
    fn load_context_files_ignores_non_md() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx_dir = tmp.path().join(".smedja").join("context");
        std::fs::create_dir_all(&ctx_dir).unwrap();
        std::fs::write(ctx_dir.join("notes.md"), "md").unwrap();
        std::fs::write(ctx_dir.join("raw.txt"), "txt").unwrap();
        let result = super::load_context_files(tmp.path()).unwrap();
        assert_eq!(result, vec!["md"]);
    }

    #[test]
    fn workspace_skills_ordered_by_filename_not_content() {
        // z.md has content "AAA" — content-sort puts it first.
        // a.md has content "ZZZ" — filename-sort puts it first.
        // Correct: filename order (a.md before z.md) → ["ZZZ", "AAA"].
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".smedja").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(skills_dir.join("z.md"), "AAA").unwrap();
        std::fs::write(skills_dir.join("a.md"), "ZZZ").unwrap();
        let result = super::load_workspace_skills(tmp.path()).unwrap();
        assert_eq!(result, vec!["ZZZ", "AAA"]);
    }

    #[test]
    fn context_files_ordered_by_filename_not_content() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx_dir = tmp.path().join(".smedja").join("context");
        std::fs::create_dir_all(&ctx_dir).unwrap();
        std::fs::write(ctx_dir.join("z.md"), "AAA").unwrap();
        std::fs::write(ctx_dir.join("a.md"), "ZZZ").unwrap();
        let result = super::load_context_files(tmp.path()).unwrap();
        assert_eq!(result, vec!["ZZZ", "AAA"]);
    }

    #[test]
    fn role_skills_dir_ordered_by_filename_not_content() {
        let tmp = tempfile::tempdir().unwrap();
        let roles_dir = tmp.path().join(".smedja").join("roles");
        std::fs::create_dir_all(roles_dir.join("coder")).unwrap();
        std::fs::write(roles_dir.join("coder").join("z.md"), "AAA").unwrap();
        std::fs::write(roles_dir.join("coder").join("a.md"), "ZZZ").unwrap();
        let result = super::load_role_skills(tmp.path(), "coder").unwrap();
        assert_eq!(result, vec!["ZZZ", "AAA"]);
    }
}

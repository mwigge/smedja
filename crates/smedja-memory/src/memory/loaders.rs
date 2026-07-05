//! Filesystem loaders for workspace context: role-specific skill packs,
//! workspace skills, project context files, `AGENTS.md`, and skill injection
//! into [`WorkingMemory`](super::WorkingMemory).

use super::WorkingMemory;

/// Returns `true` when `name` is exactly one normal path component.
///
/// Rejects empty strings, `.`, `..`, absolute paths, and any name containing a
/// path separator. Used to keep a caller-supplied `role` from escaping the
/// roles directory when joined into a filesystem path.
fn is_single_normal_component(name: &str) -> bool {
    let mut components = std::path::Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

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
    // Path-traversal guard: `role` is joined into filesystem paths below, so it
    // must be a single normal path component. A crafted role such as
    // `../../etc/foo`, `a/b`, or an absolute path could otherwise read files
    // outside the roles directory. An invalid role has no pack by definition, so
    // we fail closed by returning an empty Vec (matching the "empty when none
    // exist" contract) rather than surfacing an error to callers.
    if !is_single_normal_component(role) {
        return Ok(Vec::new());
    }

    let roles_dir = dir.join(".smedja").join("roles");
    let mut out = Vec::new();

    let single = roles_dir.join(format!("{role}.md"));
    if single.is_file() {
        out.push(std::fs::read_to_string(&single)?);
    }

    let role_specific_dir = roles_dir.join(role);
    if role_specific_dir.is_dir() {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&role_specific_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                files.push(std::fs::read_to_string(&path)?);
            }
        }
        files.sort();
        out.extend(files);
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
    let mut skills = Vec::new();
    for entry in std::fs::read_dir(&skills_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let content = std::fs::read_to_string(&path)?;
            skills.push(content);
        }
    }
    // Sort for deterministic ordering (alphabetical by filename).
    skills.sort();
    Ok(skills)
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
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&ctx_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let content = std::fs::read_to_string(&path)?;
            files.push(content);
        }
    }
    files.sort();
    Ok(files)
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

/// Markers delimiting the smedja-managed section inside a workspace `AGENTS.md`.
///
/// The codex adapter (`smedja-adapter`'s `codex_cli.rs`) writes this section so
/// codex sees smedja's system block; [`strip_managed_agents_section`] removes it
/// when that same `AGENTS.md` is fed back into the system prompt, so smedja's own
/// injected block is never double-counted.
///
/// NOTE: these literals are duplicated in `smedja-adapter` (which this crate
/// depends on, so the dependency cannot be reversed to share them). Keep them in
/// sync with `AGENTS_MANAGED_BEGIN`/`AGENTS_MANAGED_END` there.
pub const AGENTS_MANAGED_BEGIN: &str =
    "<!-- BEGIN SMEDJA-MANAGED: auto-generated, do not edit below -->";
/// End marker for the smedja-managed `AGENTS.md` section. See
/// [`AGENTS_MANAGED_BEGIN`].
pub const AGENTS_MANAGED_END: &str = "<!-- END SMEDJA-MANAGED -->";

/// Removes the smedja-managed section (and its markers) from an `AGENTS.md`
/// body, returning only the user-authored content.
///
/// Returns the input unchanged when no managed section is present. Used before
/// injecting a repo's own `AGENTS.md` into the system prompt so smedja's own
/// block (which the codex adapter may have written into that file) is not fed
/// back to itself.
#[must_use]
pub fn strip_managed_agents_section(body: &str) -> String {
    match (
        body.find(AGENTS_MANAGED_BEGIN),
        body.find(AGENTS_MANAGED_END),
    ) {
        (Some(start), Some(end)) if end > start => {
            let end_full = end + AGENTS_MANAGED_END.len();
            let head = body[..start].trim_end();
            let tail = body[end_full..].trim_start();
            match (head.is_empty(), tail.is_empty()) {
                (true, true) => String::new(),
                (false, true) => head.to_owned(),
                (true, false) => tail.to_owned(),
                (false, false) => format!("{head}\n\n{tail}"),
            }
        }
        _ => body.to_owned(),
    }
}

#[cfg(test)]
mod agents_md_tests {
    use super::{strip_managed_agents_section, AGENTS_MANAGED_BEGIN, AGENTS_MANAGED_END};

    #[test]
    fn strip_returns_input_when_no_managed_section() {
        let body = "# My rules\n\nDo the thing.\n";
        assert_eq!(strip_managed_agents_section(body), body);
    }

    #[test]
    fn strip_removes_managed_section_keeps_user_content() {
        let body = format!(
            "# User rules\n\nBe careful.\n\n{AGENTS_MANAGED_BEGIN}\nSMEDJA BLOCK\n{AGENTS_MANAGED_END}\n"
        );
        let out = strip_managed_agents_section(&body);
        assert!(out.contains("# User rules"));
        assert!(out.contains("Be careful."));
        assert!(!out.contains("SMEDJA BLOCK"));
        assert!(!out.contains(AGENTS_MANAGED_BEGIN));
    }

    #[test]
    fn strip_of_managed_only_file_is_empty() {
        let body = format!("{AGENTS_MANAGED_BEGIN}\nSMEDJA BLOCK\n{AGENTS_MANAGED_END}\n");
        assert!(strip_managed_agents_section(&body).is_empty());
    }
}

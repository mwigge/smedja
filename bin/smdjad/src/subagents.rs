//! Maps bundle *agent* definitions onto smedja's routing model and, for runners
//! with a native agents directory (claude-cli's `.claude/agents/`), materialises
//! them so the same one-folder definitions reach the native runner too.
//!
//! This is behaviour-preserving for existing roles: an `agents/<name>.md` whose
//! name matches an existing [`AgentRole`] label refines that role's tool/model
//! policy; a name with no matching role is carried as a materialisable
//! definition but never invents a new internal route.

use std::path::Path;

use smedja_assayer::AgentRole;
use smedja_plugins::{Bundle, BundleItem, BundleKind};

/// Resolves the [`AgentRole`] an agent definition refines, by matching its name
/// (case-insensitively) against the existing role labels. Returns `None` for a
/// name that does not correspond to a built-in role — such a definition is still
/// materialisable but does not alter internal routing.
#[must_use]
pub fn role_for_agent(name: &str) -> Option<AgentRole> {
    let n = name.to_lowercase();
    [
        AgentRole::Impl,
        AgentRole::Plan,
        AgentRole::Research,
        AgentRole::Debug,
        AgentRole::Ask,
        AgentRole::Test,
        AgentRole::Review,
        AgentRole::Sre,
        AgentRole::Data,
        AgentRole::Iac,
        AgentRole::Orchestrator,
        AgentRole::Search,
    ]
    .into_iter()
    .find(|r| r.label() == n)
}

/// Returns the tool allow-list an agent definition requests, or an empty slice
/// when the definition permits all tools (or is not an agent item).
#[must_use]
pub fn agent_tools(item: &BundleItem) -> &[String] {
    item.agent.as_ref().map_or(&[], |a| a.tools.as_slice())
}

/// Reconstructs the on-disk `<name>.md` text for a native agents directory from
/// a bundle agent item: a frontmatter block (name/description/tools/model/
/// permissionMode) followed by the body.
#[must_use]
fn render_agent_md(item: &BundleItem) -> String {
    let mut front = String::from("---\n");
    front.push_str(&format!("name: {}\n", item.name));
    if !item.description.is_empty() {
        front.push_str(&format!("description: {}\n", item.description));
    }
    if let Some(agent) = &item.agent {
        if !agent.tools.is_empty() {
            front.push_str(&format!("tools: {}\n", agent.tools.join(", ")));
        }
        if let Some(model) = &agent.model {
            front.push_str(&format!("model: {model}\n"));
        }
        if let Some(mode) = &agent.permission_mode {
            front.push_str(&format!("permissionMode: {mode}\n"));
        }
    }
    front.push_str("---\n");
    format!("{front}{}", item.body)
}

/// Materialises every agent definition in `bundle` into `dest_dir` (e.g.
/// `<workspace>/.claude/agents/`), one `<name>.md` per agent. Returns the number
/// of files written.
///
/// This is the subagent half of the Phase-0 delivery: the same one-folder
/// definitions that feed smedja's internal routing are projected into the
/// runner's native agents directory so claude-cli sees them verbatim.
///
/// # Errors
///
/// Returns an `io::Error` if `dest_dir` cannot be created or a file cannot be
/// written.
pub fn materialize_agents(bundle: &Bundle, dest_dir: &Path) -> std::io::Result<usize> {
    let agents: Vec<&BundleItem> = bundle.of_kind(BundleKind::Agent).collect();
    if agents.is_empty() {
        return Ok(0);
    }
    std::fs::create_dir_all(dest_dir)?;
    let mut written = 0;
    for item in agents {
        let path = dest_dir.join(format!("{}.md", item.name));
        std::fs::write(&path, render_agent_md(item))?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_agent(ws: &Path, name: &str, front_extra: &str) {
        let agents = ws.join(".smedja/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join(format!("{name}.md")),
            format!("---\nname: {name}\ndescription: {name} agent.\n{front_extra}---\nbody for {name}\n"),
        )
        .unwrap();
    }

    #[test]
    fn role_for_agent_matches_existing_labels() {
        assert_eq!(role_for_agent("review"), Some(AgentRole::Review));
        assert_eq!(role_for_agent("Research"), Some(AgentRole::Research));
        assert_eq!(role_for_agent("not-a-role"), None);
    }

    #[test]
    fn agent_tools_reads_allow_list() {
        let ws = tempfile::tempdir().unwrap();
        seed_agent(ws.path(), "reviewer", "tools: read_file, grep_files\n");
        let bundle = Bundle::load(ws.path(), None);
        let item = bundle.find("reviewer").unwrap();
        assert_eq!(agent_tools(item), ["read_file", "grep_files"]);
    }

    #[test]
    fn materialize_writes_native_agent_files() {
        let ws = tempfile::tempdir().unwrap();
        seed_agent(
            ws.path(),
            "reviewer",
            "tools: read_file\nmodel: claude-sonnet-4-6\n",
        );
        seed_agent(ws.path(), "planner", "");
        let bundle = Bundle::load(ws.path(), None);

        let dest = ws.path().join(".claude/agents");
        let n = materialize_agents(&bundle, &dest).unwrap();
        assert_eq!(n, 2);

        let reviewer = std::fs::read_to_string(dest.join("reviewer.md")).unwrap();
        assert!(reviewer.contains("name: reviewer"));
        assert!(reviewer.contains("tools: read_file"));
        assert!(reviewer.contains("model: claude-sonnet-4-6"));
        assert!(reviewer.contains("body for reviewer"));
        assert!(dest.join("planner.md").exists());
    }

    #[test]
    fn materialize_no_agents_writes_nothing() {
        let ws = tempfile::tempdir().unwrap();
        let bundle = Bundle::load(ws.path(), None);
        let dest = ws.path().join(".claude/agents");
        assert_eq!(materialize_agents(&bundle, &dest).unwrap(), 0);
        assert!(
            !dest.exists(),
            "no agents dir created when there is nothing to write"
        );
    }

    #[test]
    fn round_trips_through_materialize_and_reload() {
        let ws = tempfile::tempdir().unwrap();
        seed_agent(
            ws.path(),
            "reviewer",
            "tools: read_file, bash\nmodel: m1\npermissionMode: read-only\n",
        );
        let bundle = Bundle::load(ws.path(), None);
        let dest = ws.path().join(".claude/agents");
        materialize_agents(&bundle, &dest).unwrap();

        // Reload the materialised file as a fresh agent item and assert fidelity.
        let content = std::fs::read_to_string(dest.join("reviewer.md")).unwrap();
        let item = smedja_plugins::parse_agent_item(&content, &dest.join("reviewer.md"));
        let agent = item.agent.unwrap();
        assert_eq!(agent.tools, ["read_file", "bash"]);
        assert_eq!(agent.model.as_deref(), Some("m1"));
        assert_eq!(agent.permission_mode.as_deref(), Some("read-only"));
    }
}

//! `.smedja/agents.toml` parser — workspace-local role overrides for the assayer.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::types::Role;
use crate::{Route, RoutingRule, Runner, Tier};

#[derive(Debug, Deserialize)]
struct RoleEntry {
    runner: Option<String>,
    tier: Option<String>,
    model: Option<String>,
    /// Tool whitelist for this role — parsed but not yet enforced; reserved for the
    /// `BashArity` permission gate (task-list item 6).
    #[serde(default)]
    #[allow(dead_code)] // reserved for BashArity gate — not yet wired up
    tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AgentsFile {
    #[serde(default)]
    roles: HashMap<String, RoleEntry>,
}

/// Loads `.smedja/agents.toml` from `workspace_dir` and returns routing rules.
///
/// Returns an empty vec if the file does not exist. The `tools` field is parsed
/// but not yet wired into `RoutingRule` (reserved for `BashArity` gate work).
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_rules(workspace_dir: &Path) -> Result<Vec<RoutingRule>, String> {
    let path = workspace_dir.join(".smedja").join("agents.toml");
    if !path.exists() {
        return Ok(vec![]);
    }
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("cannot read agents.toml: {e}"))?;
    let file: AgentsFile =
        toml::from_str(&content).map_err(|e| format!("invalid agents.toml: {e}"))?;

    let mut rules = Vec::with_capacity(file.roles.len());
    for (name, entry) in &file.roles {
        let Some(role) = parse_role(name) else {
            continue;
        };
        let runner = entry
            .runner
            .as_deref()
            .and_then(parse_runner)
            .unwrap_or(Runner::Claude);
        let tier = entry
            .tier
            .as_deref()
            .and_then(parse_tier)
            .unwrap_or(Tier::Fast);
        let route = Route {
            runner,
            tier,
            model: entry.model.clone(),
        };
        rules.push(RoutingRule::new(Some(role), None, route));
    }
    Ok(rules)
}

fn parse_role(s: &str) -> Option<Role> {
    match s {
        "impl" => Some(Role::Impl),
        "test" => Some(Role::Test),
        "review" => Some(Role::Review),
        "sre" => Some(Role::Sre),
        "orchestrator" => Some(Role::Orchestrator),
        _ => None,
    }
}

fn parse_runner(s: &str) -> Option<Runner> {
    match s {
        "claude" => Some(Runner::Claude),
        "local" => Some(Runner::Local),
        "codex" => Some(Runner::Codex),
        "copilot" => Some(Runner::Copilot),
        _ => None,
    }
}

fn parse_tier(s: &str) -> Option<Tier> {
    match s {
        "fast" => Some(Tier::Fast),
        "local" => Some(Tier::Local),
        "deep" => Some(Tier::Deep),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_toml(dir: &std::path::Path, content: &str) {
        let smedja = dir.join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(smedja.join("agents.toml"), content).unwrap();
    }

    #[test]
    fn missing_file_returns_empty_rules() {
        let dir = tempfile::tempdir().unwrap();
        let rules = load_rules(dir.path()).unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn review_role_parsed_as_deep() {
        use crate::{Assayer, Complexity};
        let dir = tempfile::tempdir().unwrap();
        write_toml(
            dir.path(),
            "[roles.review]\nrunner=\"claude\"\ntier=\"deep\"\n",
        );
        let rules = load_rules(dir.path()).unwrap();
        assert_eq!(rules.len(), 1);
        let mut default = Assayer::default_rules();
        default.prepend_rules(rules);
        let route = default.route(Role::Review, Complexity::Coding);
        assert_eq!(route.tier, Tier::Deep);
        assert_eq!(route.runner, Runner::Claude);
    }

    #[test]
    fn impl_role_local_override() {
        use crate::{Assayer, Complexity};
        let dir = tempfile::tempdir().unwrap();
        write_toml(
            dir.path(),
            "[roles.impl]\nrunner=\"local\"\ntier=\"local\"\n",
        );
        let rules = load_rules(dir.path()).unwrap();
        let mut default = Assayer::default_rules();
        default.prepend_rules(rules);
        let route = default.route(Role::Impl, Complexity::Simple);
        assert_eq!(route.runner, Runner::Local);
    }

    #[test]
    fn missing_agents_toml_applies_defaults() {
        use crate::{Assayer, Complexity};
        let dir = tempfile::tempdir().unwrap();
        let rules = load_rules(dir.path()).unwrap();
        let mut default = Assayer::default_rules();
        default.prepend_rules(rules);
        let route = default.route(Role::Review, Complexity::Coding);
        // Default: Review → Claude/Deep
        assert_eq!(route.runner, Runner::Claude);
        assert_eq!(route.tier, Tier::Deep);
    }

    #[test]
    fn model_override_propagated() {
        use crate::{Assayer, Complexity};
        let dir = tempfile::tempdir().unwrap();
        write_toml(
            dir.path(),
            "[roles.impl]\nrunner=\"local\"\ntier=\"local\"\nmodel=\"gemma-3-27b\"\n",
        );
        let rules = load_rules(dir.path()).unwrap();
        let mut default = Assayer::default_rules();
        default.prepend_rules(rules);
        let route = default.route(Role::Impl, Complexity::Simple);
        assert_eq!(route.model.as_deref(), Some("gemma-3-27b"));
    }

    #[test]
    fn unknown_role_name_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write_toml(
            dir.path(),
            "[roles.janitor]\nrunner=\"local\"\ntier=\"local\"\n",
        );
        let rules = load_rules(dir.path()).unwrap();
        // Unknown role names are silently skipped.
        assert!(rules.is_empty());
    }

    #[test]
    fn invalid_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        write_toml(dir.path(), "this is not valid toml = [[[");
        let result = load_rules(dir.path());
        assert!(result.is_err());
    }
}

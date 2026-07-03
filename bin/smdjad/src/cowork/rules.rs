//! Declarative `[[permission.rules]]` loaded from `.smedja/workspace.toml`.

use serde::Deserialize;

use super::policy::PermissionDecision;

/// A single declarative permission rule from `[[permission.rules]]` in
/// `.smedja/workspace.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionRule {
    /// Tool name or glob pattern (e.g. `"bash"`, `"write_*"`).
    pub tool: String,
    /// Glob matched against the `path` field of file-tool inputs.
    pub path_glob: Option<String>,
    /// Prefix/glob matched against the `command` field of bash inputs.
    pub command_pattern: Option<String>,
    /// Gate outcome when this rule matches.
    pub mode: RuleMode,
}

/// Gate outcome for a [`PermissionRule`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleMode {
    /// Always ask the user (same as cowork Ask mode).
    Ask,
    /// Let the call through without asking.
    Allow,
    /// Block the call before it reaches the gate.
    Deny,
}

/// Loads `[[permission.rules]]` from `.smedja/workspace.toml`, returning an
/// empty list if the file is absent or the section is missing.
#[must_use]
pub fn load_permission_rules(workspace: &std::path::Path) -> Vec<PermissionRule> {
    #[derive(Deserialize, Default)]
    struct WorkspaceToml {
        permission: Option<PermSection>,
    }
    #[derive(Deserialize, Default)]
    struct PermSection {
        rules: Option<Vec<PermissionRule>>,
    }
    let path = workspace.join(".smedja").join("workspace.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<WorkspaceToml>(&s).ok())
        .and_then(|c| c.permission?.rules)
        .unwrap_or_default()
}

/// Evaluates `rules` in order; returns the first matching rule's
/// [`PermissionDecision`], or `None` if no rule matches (fall through to
/// session mode).
#[must_use]
pub fn evaluate_permission_rules(
    rules: &[PermissionRule],
    tool: &str,
    args: &serde_json::Value,
) -> Option<PermissionDecision> {
    for rule in rules {
        if !perm_glob_match(&rule.tool, tool) {
            continue;
        }
        if let Some(ref glob) = rule.path_glob {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if !perm_glob_match(glob, path) {
                continue;
            }
        }
        if let Some(ref pat) = rule.command_pattern {
            let cmd = args
                .get("command")
                .or_else(|| args.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let prefix = pat.trim_end_matches('*');
            if !cmd.starts_with(prefix) {
                continue;
            }
        }
        return Some(match rule.mode {
            RuleMode::Ask => PermissionDecision::Ask,
            RuleMode::Allow => PermissionDecision::Allow,
            RuleMode::Deny => PermissionDecision::Deny,
        });
    }
    None
}

/// Minimal glob: `*` matches any sequence of characters, `?` matches one char.
fn perm_glob_match(pattern: &str, value: &str) -> bool {
    let mut p = pattern.as_bytes();
    let mut s = value.as_bytes();
    loop {
        match (p.first(), s.first()) {
            (None, None) => return true,
            (Some(&b'*'), _) => {
                p = &p[1..];
                if p.is_empty() {
                    return true;
                }
                for i in 0..=s.len() {
                    if perm_glob_match(
                        std::str::from_utf8(p).unwrap_or(""),
                        std::str::from_utf8(&s[i..]).unwrap_or(""),
                    ) {
                        return true;
                    }
                }
                return false;
            }
            (Some(&b'?'), Some(_)) => {
                p = &p[1..];
                s = &s[1..];
            }
            (Some(a), Some(b)) if a == b => {
                p = &p[1..];
                s = &s[1..];
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_rules_deny_blocks() {
        let rules = vec![PermissionRule {
            tool: "bash".into(),
            path_glob: None,
            command_pattern: None,
            mode: RuleMode::Deny,
        }];
        let result = evaluate_permission_rules(&rules, "bash", &serde_json::Value::Null);
        assert_eq!(result, Some(PermissionDecision::Deny));
    }

    #[test]
    fn permission_rules_allow_bypasses_gate() {
        let rules = vec![PermissionRule {
            tool: "read_file".into(),
            path_glob: Some("src/**".into()),
            command_pattern: None,
            mode: RuleMode::Allow,
        }];
        let args = json!({"path": "src/main.rs"});
        let result = evaluate_permission_rules(&rules, "read_file", &args);
        assert_eq!(result, Some(PermissionDecision::Allow));
    }

    #[test]
    fn permission_rules_fallthrough_when_no_match() {
        let rules = vec![PermissionRule {
            tool: "write_file".into(),
            path_glob: None,
            command_pattern: None,
            mode: RuleMode::Deny,
        }];
        let result = evaluate_permission_rules(&rules, "read_file", &serde_json::Value::Null);
        assert_eq!(
            result, None,
            "non-matching rule must not produce a decision"
        );
    }

    #[test]
    fn permission_rules_path_glob_non_match_skips_rule() {
        let rules = vec![PermissionRule {
            tool: "write_file".into(),
            path_glob: Some("src/**".into()),
            command_pattern: None,
            mode: RuleMode::Deny,
        }];
        // path is outside src/ — rule must not match
        let args = json!({"path": "tests/foo.rs"});
        let result = evaluate_permission_rules(&rules, "write_file", &args);
        assert_eq!(result, None, "path outside glob must not trigger rule");
    }
}

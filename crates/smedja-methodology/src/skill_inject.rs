/// Advisory raised when a diff signals that a methodology skill should have
/// been invoked but was not found in the current session's invocation list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillAdvisory {
    /// The skill that should have been invoked (e.g. `/security-review`).
    pub skill: &'static str,
    /// Human-readable reason explaining why the skill was expected.
    pub reason: &'static str,
}

impl SkillAdvisory {
    /// Human-readable one-line summary for panel display.
    #[must_use]
    pub fn summary(&self) -> String {
        format!("{} — {}", self.skill, self.reason)
    }
}

/// Heuristic signals: (`diff_pattern`, `expected_skill`, `reason`)
const SIGNALS: &[(&str, &str, &str)] = &[
    // New test functions → TDD workflow should be active.
    ("#[test]", "/tdd-workflow", "diff adds tests"),
    ("fn test_", "/tdd-workflow", "diff adds test functions"),
    // Auth / secrets handling → security review.
    (
        "Authorization",
        "/security-review",
        "diff touches auth headers",
    ),
    (
        "Bearer ",
        "/security-review",
        "diff references Bearer tokens",
    ),
    ("password", "/security-review", "diff references passwords"),
    ("secret", "/security-review", "diff references secrets"),
    ("api_key", "/security-review", "diff references API keys"),
    // New HTTP routes → API design review.
    (
        "axum::Router",
        "/api-designer",
        "diff introduces HTTP routes",
    ),
    (".route(\"", "/api-designer", "diff adds route definition"),
    // SQL — parameterised-query enforcement is a methodology rule, not a skill,
    // but the postgres-patterns skill is the reference.
    (
        "execute(\"",
        "/postgres-patterns",
        "diff uses raw SQL execute",
    ),
];

/// Checks a diff for signals indicating that a methodology skill should have
/// been invoked, then cross-references against `session_skills`.
///
/// Returns one [`SkillAdvisory`] per skill that the diff signals but that is
/// absent from `session_skills`. Duplicate advisories for the same skill are
/// deduplicated — at most one advisory per skill.
///
/// # Arguments
///
/// * `diff` — the unified diff text of the current turn's changes.
/// * `session_skills` — skills already invoked this session (e.g. `["/tdd-workflow"]`).
#[must_use]
pub fn check(diff: &str, session_skills: &[String]) -> Vec<SkillAdvisory> {
    let mut seen: Vec<&'static str> = Vec::new();
    let mut advisories: Vec<SkillAdvisory> = Vec::new();

    for (pattern, skill, reason) in SIGNALS {
        // Already emitted an advisory for this skill — skip.
        if seen.contains(skill) {
            continue;
        }
        // Diff does not contain this signal — skip.
        if !diff.contains(pattern) {
            continue;
        }
        // Skill was invoked this session — no advisory needed.
        if session_skills.iter().any(|s| s == skill) {
            continue;
        }
        seen.push(skill);
        advisories.push(SkillAdvisory { skill, reason });
    }

    advisories
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skills(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn empty_diff_no_advisories() {
        assert!(check("", &[]).is_empty());
    }

    #[test]
    fn no_signals_no_advisories() {
        let diff = "+fn add(a: i32, b: i32) -> i32 { a + b }";
        assert!(check(diff, &[]).is_empty());
    }

    #[test]
    fn test_signal_without_skill_raises_advisory() {
        let diff = "+#[test]\n+fn test_add() { assert_eq!(add(1,2),3); }";
        let advisories = check(diff, &[]);
        assert!(
            advisories.iter().any(|a| a.skill == "/tdd-workflow"),
            "expected /tdd-workflow advisory"
        );
    }

    #[test]
    fn test_signal_with_skill_invoked_no_advisory() {
        let diff = "+#[test]\n+fn test_add() {}";
        let advisories = check(diff, &skills(&["/tdd-workflow"]));
        assert!(
            !advisories.iter().any(|a| a.skill == "/tdd-workflow"),
            "no advisory when skill was invoked"
        );
    }

    #[test]
    fn auth_signal_raises_security_advisory() {
        let diff = r#"+headers.insert("Authorization", token);"#;
        let advisories = check(diff, &[]);
        assert!(
            advisories.iter().any(|a| a.skill == "/security-review"),
            "expected /security-review advisory"
        );
    }

    #[test]
    fn auth_signal_with_security_skill_no_advisory() {
        let diff = r#"+headers.insert("Authorization", token);"#;
        let advisories = check(diff, &skills(&["/security-review"]));
        assert!(advisories.is_empty());
    }

    #[test]
    fn route_signal_raises_api_designer_advisory() {
        let diff = r#"+router.route("/users", get(list_users));"#;
        let advisories = check(diff, &[]);
        assert!(
            advisories.iter().any(|a| a.skill == "/api-designer"),
            "expected /api-designer advisory"
        );
    }

    #[test]
    fn multiple_signals_same_skill_deduplicated() {
        // Both "#[test]" and "fn test_" appear — should get exactly one /tdd-workflow advisory.
        let diff = "+#[test]\n+fn test_foo() {}";
        let advisories = check(diff, &[]);
        let count = advisories
            .iter()
            .filter(|a| a.skill == "/tdd-workflow")
            .count();
        assert_eq!(count, 1, "skill advisory deduplicated");
    }

    #[test]
    fn multiple_different_skills_all_raised() {
        let diff = "+#[test]\n+headers.insert(\"Authorization\", t);\n+.route(\"/x\", get(h));";
        let advisories = check(diff, &[]);
        let skills_found: Vec<&str> = advisories.iter().map(|a| a.skill).collect();
        assert!(skills_found.contains(&"/tdd-workflow"));
        assert!(skills_found.contains(&"/security-review"));
        assert!(skills_found.contains(&"/api-designer"));
    }

    #[test]
    fn summary_contains_skill_and_reason() {
        let adv = SkillAdvisory {
            skill: "/tdd-workflow",
            reason: "diff adds tests",
        };
        let s = adv.summary();
        assert!(s.contains("/tdd-workflow"));
        assert!(s.contains("diff adds tests"));
    }
}

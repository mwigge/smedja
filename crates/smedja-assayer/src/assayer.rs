use crate::types::{Complexity, Role, Route, Runner, Tier};

/// A single routing rule: optional role and optional complexity matchers, plus the
/// `Route` to emit when both match. `None` in either position acts as a wildcard.
#[derive(Debug, Clone)]
pub struct RoutingRule {
    role: Option<Role>,
    complexity: Option<Complexity>,
    route: Route,
}

impl RoutingRule {
    /// Creates a new routing rule.
    ///
    /// `role` and `complexity` are optional; `None` acts as a wildcard that
    /// matches any value in that position.
    #[must_use]
    pub fn new(role: Option<Role>, complexity: Option<Complexity>, route: Route) -> Self {
        Self {
            role,
            complexity,
            route,
        }
    }

    /// Returns `true` when `role` and `complexity` match this rule.
    fn matches(&self, role: Role, complexity: Complexity) -> bool {
        let role_match = self.role.is_none_or(|r| r == role);
        let complexity_match = self.complexity.is_none_or(|c| c == complexity);
        role_match && complexity_match
    }
}

/// Routes a role × complexity pair to a `(Runner, Tier)` combination using an
/// ordered list of `RoutingRule`s. The first matching rule wins.
///
/// Use [`Assayer::default_rules`] to obtain an instance pre-loaded with the
/// standard routing table.
#[derive(Debug, Clone)]
pub struct Assayer {
    rules: Vec<RoutingRule>,
}

impl Assayer {
    /// Creates an `Assayer` pre-loaded with the default routing table.
    ///
    /// | Role         | Complexity | Runner | Tier  |
    /// |--------------|-----------|--------|-------|
    /// | Impl         | Simple    | Local  | Local |
    /// | Impl         | Coding    | Local  | Local |
    /// | Impl         | Complex   | Claude | Deep  |
    /// | Test         | *         | Local  | Local |
    /// | Review       | *         | Claude | Deep  |
    /// | Sre          | *         | Claude | Deep  |
    /// | Orchestrator | *         | Claude | Fast  |
    #[must_use]
    pub fn default_rules() -> Self {
        let local = || Route {
            runner: Runner::Local,
            tier: Tier::Local,
        };
        let claude_deep = || Route {
            runner: Runner::Claude,
            tier: Tier::Deep,
        };
        let claude_fast = || Route {
            runner: Runner::Claude,
            tier: Tier::Fast,
        };

        Self {
            rules: vec![
                // Impl × Simple → Local/Local
                RoutingRule::new(Some(Role::Impl), Some(Complexity::Simple), local()),
                // Impl × Coding → Local/Local
                RoutingRule::new(Some(Role::Impl), Some(Complexity::Coding), local()),
                // Impl × Complex → Claude/Deep
                RoutingRule::new(Some(Role::Impl), Some(Complexity::Complex), claude_deep()),
                // Test × * → Local/Local
                RoutingRule::new(Some(Role::Test), None, local()),
                // Review × * → Claude/Deep
                RoutingRule::new(Some(Role::Review), None, claude_deep()),
                // Sre × * → Claude/Deep
                RoutingRule::new(Some(Role::Sre), None, claude_deep()),
                // Orchestrator × * → Claude/Fast
                RoutingRule::new(Some(Role::Orchestrator), None, claude_fast()),
            ],
        }
    }

    /// Creates an `Assayer` from a caller-supplied ordered list of rules.
    #[must_use]
    pub fn from_rules(rules: Vec<RoutingRule>) -> Self {
        Self { rules }
    }

    /// Routes `role` × `complexity` to the first matching `Route`.
    ///
    /// Rules are evaluated in insertion order; the first match is returned.
    /// Falls back to `Runner::Local` / `Tier::Local` if no rule matches.
    #[must_use]
    pub fn route(&self, role: Role, complexity: Complexity) -> Route {
        self.rules
            .iter()
            .find(|r| r.matches(role, complexity))
            .map_or(
                Route {
                    runner: Runner::Local,
                    tier: Tier::Local,
                },
                |r| r.route.clone(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------ helpers

    fn local_local() -> Route {
        Route {
            runner: Runner::Local,
            tier: Tier::Local,
        }
    }

    fn claude_deep() -> Route {
        Route {
            runner: Runner::Claude,
            tier: Tier::Deep,
        }
    }

    fn claude_fast() -> Route {
        Route {
            runner: Runner::Claude,
            tier: Tier::Fast,
        }
    }

    // ------------------------------------------------------------------ tests

    #[test]
    fn impl_simple_routes_to_local() {
        let assayer = Assayer::default_rules();
        assert_eq!(assayer.route(Role::Impl, Complexity::Simple), local_local());
    }

    #[test]
    fn impl_coding_routes_to_local() {
        let assayer = Assayer::default_rules();
        assert_eq!(assayer.route(Role::Impl, Complexity::Coding), local_local());
    }

    #[test]
    fn impl_complex_routes_to_claude_deep() {
        let assayer = Assayer::default_rules();
        assert_eq!(
            assayer.route(Role::Impl, Complexity::Complex),
            claude_deep()
        );
    }

    #[test]
    fn review_always_claude_deep() {
        let assayer = Assayer::default_rules();
        assert_eq!(
            assayer.route(Role::Review, Complexity::Simple),
            claude_deep()
        );
        assert_eq!(
            assayer.route(Role::Review, Complexity::Coding),
            claude_deep()
        );
        assert_eq!(
            assayer.route(Role::Review, Complexity::Complex),
            claude_deep()
        );
    }

    #[test]
    fn orchestrator_routes_to_claude_fast() {
        let assayer = Assayer::default_rules();
        assert_eq!(
            assayer.route(Role::Orchestrator, Complexity::Simple),
            claude_fast()
        );
        assert_eq!(
            assayer.route(Role::Orchestrator, Complexity::Coding),
            claude_fast()
        );
        assert_eq!(
            assayer.route(Role::Orchestrator, Complexity::Complex),
            claude_fast()
        );
    }

    #[test]
    fn sre_routes_to_claude_deep() {
        let assayer = Assayer::default_rules();
        assert_eq!(assayer.route(Role::Sre, Complexity::Simple), claude_deep());
        assert_eq!(assayer.route(Role::Sre, Complexity::Coding), claude_deep());
        assert_eq!(assayer.route(Role::Sre, Complexity::Complex), claude_deep());
    }

    #[test]
    fn test_role_routes_to_local() {
        let assayer = Assayer::default_rules();
        assert_eq!(assayer.route(Role::Test, Complexity::Simple), local_local());
        assert_eq!(assayer.route(Role::Test, Complexity::Coding), local_local());
        assert_eq!(
            assayer.route(Role::Test, Complexity::Complex),
            local_local()
        );
    }

    #[test]
    fn wildcard_rule_matches_any_complexity() {
        // A rule with None complexity should match all three complexity levels.
        let rules = vec![RoutingRule::new(
            Some(Role::Impl),
            None,
            Route {
                runner: Runner::Codex,
                tier: Tier::Fast,
            },
        )];
        let assayer = Assayer::from_rules(rules);
        let expected = Route {
            runner: Runner::Codex,
            tier: Tier::Fast,
        };
        assert_eq!(assayer.route(Role::Impl, Complexity::Simple), expected);
        assert_eq!(assayer.route(Role::Impl, Complexity::Coding), expected);
        assert_eq!(assayer.route(Role::Impl, Complexity::Complex), expected);
    }

    #[test]
    fn first_matching_rule_wins() {
        // Specific rule (Impl + Complex → Copilot/Deep) placed before a wildcard
        // (Impl + None → Local/Local). The specific rule must win for Complex.
        let rules = vec![
            RoutingRule::new(
                Some(Role::Impl),
                Some(Complexity::Complex),
                Route {
                    runner: Runner::Copilot,
                    tier: Tier::Deep,
                },
            ),
            RoutingRule::new(
                Some(Role::Impl),
                None,
                Route {
                    runner: Runner::Local,
                    tier: Tier::Local,
                },
            ),
        ];
        let assayer = Assayer::from_rules(rules);

        assert_eq!(
            assayer.route(Role::Impl, Complexity::Complex),
            Route {
                runner: Runner::Copilot,
                tier: Tier::Deep,
            }
        );
        // Simple/Coding should fall through to the wildcard.
        assert_eq!(assayer.route(Role::Impl, Complexity::Simple), local_local());
        assert_eq!(assayer.route(Role::Impl, Complexity::Coding), local_local());
    }
}

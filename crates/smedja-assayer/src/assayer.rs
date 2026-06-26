use crate::types::{AgentRole, Complexity, Route, RoutingDecision, Runner, Tier};

/// A single routing rule: optional role and optional complexity matchers, plus the
/// `Route` to emit when both match. `None` in either position acts as a wildcard.
#[derive(Debug, Clone)]
pub struct RoutingRule {
    role: Option<AgentRole>,
    complexity: Option<Complexity>,
    route: Route,
}

impl RoutingRule {
    /// Creates a new routing rule.
    ///
    /// `role` and `complexity` are optional; `None` acts as a wildcard that
    /// matches any value in that position.
    #[must_use]
    pub fn new(role: Option<AgentRole>, complexity: Option<Complexity>, route: Route) -> Self {
        Self {
            role,
            complexity,
            route,
        }
    }

    /// Returns `true` when `role` and `complexity` match this rule.
    fn matches(&self, role: AgentRole, complexity: Complexity) -> bool {
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
            model: None,
            tools: vec![],
        };
        let claude_deep = || Route {
            runner: Runner::Claude,
            tier: Tier::Deep,
            model: None,
            tools: vec![],
        };
        let claude_fast = || Route {
            runner: Runner::Claude,
            tier: Tier::Fast,
            model: None,
            tools: vec![],
        };

        Self {
            rules: vec![
                // Impl × Simple → Local/Local
                RoutingRule::new(Some(AgentRole::Impl), Some(Complexity::Simple), local()),
                // Impl × Coding → Local/Local
                RoutingRule::new(Some(AgentRole::Impl), Some(Complexity::Coding), local()),
                // Impl × Complex → Claude/Deep
                RoutingRule::new(
                    Some(AgentRole::Impl),
                    Some(Complexity::Complex),
                    claude_deep(),
                ),
                // Test × * → Local/Local
                RoutingRule::new(Some(AgentRole::Test), None, local()),
                // Review × * → Claude/Deep
                RoutingRule::new(Some(AgentRole::Review), None, claude_deep()),
                // Sre × * → Claude/Deep
                RoutingRule::new(Some(AgentRole::Sre), None, claude_deep()),
                // Orchestrator × * → Claude/Fast
                RoutingRule::new(Some(AgentRole::Orchestrator), None, claude_fast()),
            ],
        }
    }

    /// Creates an `Assayer` from a caller-supplied ordered list of rules.
    #[must_use]
    pub fn from_rules(rules: Vec<RoutingRule>) -> Self {
        Self { rules }
    }

    /// Prepends `rules` so they take priority over the existing routing table.
    ///
    /// After this call the supplied rules are evaluated first; the original
    /// rules serve as fallbacks.
    pub fn prepend_rules(&mut self, mut rules: Vec<RoutingRule>) {
        rules.append(&mut self.rules);
        self.rules = rules;
    }

    /// Routes `role` × `complexity` to the first matching `Route`.
    ///
    /// Rules are evaluated in insertion order; the first match is returned.
    /// Falls back to `Runner::Local` / `Tier::Local` if no rule matches.
    #[must_use]
    pub fn route(&self, role: AgentRole, complexity: Complexity) -> Route {
        self.rules
            .iter()
            .find(|r| r.matches(role, complexity))
            .map_or(
                Route {
                    runner: Runner::Local,
                    tier: Tier::Local,
                    model: None,
                    tools: vec![],
                },
                |r| r.route.clone(),
            )
    }

    /// Routes `role` × `complexity` and returns a [`RoutingDecision`] that
    /// captures the destination, the complexity that was used, and a short
    /// rationale explaining the choice.
    ///
    /// This wraps [`Assayer::route`]; the resulting destination is identical.
    #[must_use]
    pub fn route_decision(&self, role: AgentRole, complexity: Complexity) -> RoutingDecision {
        let route = self.route(role, complexity);
        let rationale = format!(
            "role={} complexity={} tier={} via default rules",
            role_label(role),
            complexity_label(complexity),
            tier_label(route.tier),
        );
        RoutingDecision::new(route.runner, route.tier, route.model, complexity, rationale)
    }
}

/// Returns the lowercase label for an agent role, used in rationale strings.
fn role_label(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Impl => "impl",
        AgentRole::Test => "test",
        AgentRole::Review => "review",
        AgentRole::Sre => "sre",
        AgentRole::Orchestrator => "orchestrator",
    }
}

/// Returns the lowercase label for a complexity, used in rationale strings.
fn complexity_label(complexity: Complexity) -> &'static str {
    match complexity {
        Complexity::Simple => "simple",
        Complexity::Coding => "coding",
        Complexity::Complex => "complex",
    }
}

/// Returns the lowercase label for a tier, used in rationale strings.
fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Local => "local",
        Tier::Fast => "fast",
        Tier::Deep => "deep",
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
            model: None,
            tools: vec![],
        }
    }

    fn claude_deep() -> Route {
        Route {
            runner: Runner::Claude,
            tier: Tier::Deep,
            model: None,
            tools: vec![],
        }
    }

    fn claude_fast() -> Route {
        Route {
            runner: Runner::Claude,
            tier: Tier::Fast,
            model: None,
            tools: vec![],
        }
    }

    // ------------------------------------------------------------------ tests

    #[test]
    fn impl_simple_routes_to_local() {
        let assayer = Assayer::default_rules();
        assert_eq!(
            assayer.route(AgentRole::Impl, Complexity::Simple),
            local_local()
        );
    }

    #[test]
    fn impl_coding_routes_to_local() {
        let assayer = Assayer::default_rules();
        assert_eq!(
            assayer.route(AgentRole::Impl, Complexity::Coding),
            local_local()
        );
    }

    #[test]
    fn impl_complex_routes_to_claude_deep() {
        let assayer = Assayer::default_rules();
        assert_eq!(
            assayer.route(AgentRole::Impl, Complexity::Complex),
            claude_deep()
        );
    }

    #[test]
    fn review_always_claude_deep() {
        let assayer = Assayer::default_rules();
        assert_eq!(
            assayer.route(AgentRole::Review, Complexity::Simple),
            claude_deep()
        );
        assert_eq!(
            assayer.route(AgentRole::Review, Complexity::Coding),
            claude_deep()
        );
        assert_eq!(
            assayer.route(AgentRole::Review, Complexity::Complex),
            claude_deep()
        );
    }

    #[test]
    fn orchestrator_routes_to_claude_fast() {
        let assayer = Assayer::default_rules();
        assert_eq!(
            assayer.route(AgentRole::Orchestrator, Complexity::Simple),
            claude_fast()
        );
        assert_eq!(
            assayer.route(AgentRole::Orchestrator, Complexity::Coding),
            claude_fast()
        );
        assert_eq!(
            assayer.route(AgentRole::Orchestrator, Complexity::Complex),
            claude_fast()
        );
    }

    #[test]
    fn sre_routes_to_claude_deep() {
        let assayer = Assayer::default_rules();
        assert_eq!(
            assayer.route(AgentRole::Sre, Complexity::Simple),
            claude_deep()
        );
        assert_eq!(
            assayer.route(AgentRole::Sre, Complexity::Coding),
            claude_deep()
        );
        assert_eq!(
            assayer.route(AgentRole::Sre, Complexity::Complex),
            claude_deep()
        );
    }

    #[test]
    fn test_role_routes_to_local() {
        let assayer = Assayer::default_rules();
        assert_eq!(
            assayer.route(AgentRole::Test, Complexity::Simple),
            local_local()
        );
        assert_eq!(
            assayer.route(AgentRole::Test, Complexity::Coding),
            local_local()
        );
        assert_eq!(
            assayer.route(AgentRole::Test, Complexity::Complex),
            local_local()
        );
    }

    #[test]
    fn wildcard_rule_matches_any_complexity() {
        // A rule with None complexity should match all three complexity levels.
        let rules = vec![RoutingRule::new(
            Some(AgentRole::Impl),
            None,
            Route {
                runner: Runner::Codex,
                tier: Tier::Fast,
                model: None,
                tools: vec![],
            },
        )];
        let assayer = Assayer::from_rules(rules);
        let expected = Route {
            runner: Runner::Codex,
            tier: Tier::Fast,
            model: None,
            tools: vec![],
        };
        assert_eq!(assayer.route(AgentRole::Impl, Complexity::Simple), expected);
        assert_eq!(assayer.route(AgentRole::Impl, Complexity::Coding), expected);
        assert_eq!(
            assayer.route(AgentRole::Impl, Complexity::Complex),
            expected
        );
    }

    #[test]
    fn first_matching_rule_wins() {
        // Specific rule (Impl + Complex → Copilot/Deep) placed before a wildcard
        // (Impl + None → Local/Local). The specific rule must win for Complex.
        let rules = vec![
            RoutingRule::new(
                Some(AgentRole::Impl),
                Some(Complexity::Complex),
                Route {
                    runner: Runner::Copilot,
                    tier: Tier::Deep,
                    model: None,
                    tools: vec![],
                },
            ),
            RoutingRule::new(
                Some(AgentRole::Impl),
                None,
                Route {
                    runner: Runner::Local,
                    tier: Tier::Local,
                    model: None,
                    tools: vec![],
                },
            ),
        ];
        let assayer = Assayer::from_rules(rules);

        assert_eq!(
            assayer.route(AgentRole::Impl, Complexity::Complex),
            Route {
                runner: Runner::Copilot,
                tier: Tier::Deep,
                model: None,
                tools: vec![],
            }
        );
        // Simple/Coding should fall through to the wildcard.
        assert_eq!(
            assayer.route(AgentRole::Impl, Complexity::Simple),
            local_local()
        );
        assert_eq!(
            assayer.route(AgentRole::Impl, Complexity::Coding),
            local_local()
        );
    }

    // ------------------------------------------------------- routing decisions

    #[test]
    fn route_decision_matches_route_destination() {
        let assayer = Assayer::default_rules();
        let route = assayer.route(AgentRole::Review, Complexity::Coding);
        let decision = assayer.route_decision(AgentRole::Review, Complexity::Coding);

        assert_eq!(decision.runner(), route.runner);
        assert_eq!(decision.tier(), route.tier);
        assert_eq!(decision.model(), route.model.as_deref());
    }

    #[test]
    fn route_decision_records_complexity_used() {
        let assayer = Assayer::default_rules();

        let decision = assayer.route_decision(AgentRole::Impl, Complexity::Complex);
        assert_eq!(decision.complexity(), Complexity::Complex);
        assert!(decision.rationale().contains("complexity=complex"));

        // A different complexity is faithfully recorded on the decision.
        let simple = assayer.route_decision(AgentRole::Impl, Complexity::Simple);
        assert_eq!(simple.complexity(), Complexity::Simple);
        assert!(simple.rationale().contains("complexity=simple"));
    }

    #[test]
    fn route_decision_retains_rationale_for_runner_override() {
        // A caller-supplied rule overrides the runner; the decision must still
        // carry a non-empty rationale describing the chosen destination.
        let rules = vec![RoutingRule::new(
            Some(AgentRole::Impl),
            None,
            Route {
                runner: Runner::Codex,
                tier: Tier::Fast,
                model: Some("codex-mini".to_string()),
                tools: vec![],
            },
        )];
        let assayer = Assayer::from_rules(rules);

        let decision = assayer.route_decision(AgentRole::Impl, Complexity::Coding);

        assert_eq!(decision.runner(), Runner::Codex);
        assert_eq!(decision.tier(), Tier::Fast);
        assert_eq!(decision.model(), Some("codex-mini"));
        assert_eq!(decision.complexity(), Complexity::Coding);
        assert!(!decision.rationale().is_empty());
        assert!(decision.rationale().contains("role=impl"));
        assert!(decision.rationale().contains("tier=fast"));
    }
}

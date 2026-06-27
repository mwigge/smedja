use crate::types::{AgentRole, Complexity, Route, RoutingDecision, Runner, Tier};

/// Cost-aware tier ladder: the tier to use at implementation `step` (0-based),
/// descending from the strongest model early to cheaper models as work
/// progresses — the "opus → sonnet → haiku throughout implementation" policy.
///
/// step 0 → Deep (plan/first cut with the strong model), steps 1–2 → Fast,
/// step 3+ → Local. Capping a route at this tier lets an orchestrated loop start
/// deep and get cheaper without losing the ability to escalate per role.
#[must_use]
pub fn descending_tier(step: usize) -> Tier {
    match step {
        0 => Tier::Deep,
        1 | 2 => Tier::Fast,
        _ => Tier::Local,
    }
}

/// Caps `tier` so it is never *more* capable than `ceiling` — used to apply the
/// [`descending_tier`] ladder on top of a role's routed tier (a role can pin a
/// cheaper tier but the ladder won't force a role above its ceiling).
#[must_use]
pub fn cap_tier(tier: Tier, ceiling: Tier) -> Tier {
    if tier_rank(tier) > tier_rank(ceiling) {
        ceiling
    } else {
        tier
    }
}

/// Capability rank for tier ordering (Local < Fast < Deep).
fn tier_rank(tier: Tier) -> u8 {
    match tier {
        Tier::Local => 0,
        Tier::Fast => 1,
        Tier::Deep => 2,
    }
}

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
                // Plan × * → Claude/Deep (architecture/planning wants the strong model)
                RoutingRule::new(Some(AgentRole::Plan), None, claude_deep()),
                // Research × * → Claude/Deep (web/pdf/vision + synthesis)
                RoutingRule::new(Some(AgentRole::Research), None, claude_deep()),
                // Debug × * → Claude/Deep (tracing root causes benefits from depth)
                RoutingRule::new(Some(AgentRole::Debug), None, claude_deep()),
                // Ask × * → Local/Fast (read-only Q&A, latency over depth)
                RoutingRule::new(Some(AgentRole::Ask), None, claude_fast()),
                // Test × * → Local/Local
                RoutingRule::new(Some(AgentRole::Test), None, local()),
                // Review × * → Claude/Deep
                RoutingRule::new(Some(AgentRole::Review), None, claude_deep()),
                // Sre × * → Claude/Deep
                RoutingRule::new(Some(AgentRole::Sre), None, claude_deep()),
                // Orchestrator × * → Claude/Fast. (This is also the default
                // fallback for a no-mode turn, so it stays cheap; the
                // "orchestration on deep" split is realised in the Phase-4
                // delegation loop, which plans on deep explicitly.)
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
    role.label()
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
    fn research_carries_capability_tags() {
        assert_eq!(
            AgentRole::Research.capabilities(),
            &["web", "pdf", "vision"]
        );
        assert!(AgentRole::Impl.capabilities().is_empty());
        assert!(AgentRole::Plan.capabilities().is_empty());
    }

    #[test]
    fn descending_tier_ladder_steps_down_and_caps() {
        assert_eq!(super::descending_tier(0), Tier::Deep);
        assert_eq!(super::descending_tier(1), Tier::Fast);
        assert_eq!(super::descending_tier(2), Tier::Fast);
        assert_eq!(super::descending_tier(3), Tier::Local);
        assert_eq!(super::descending_tier(99), Tier::Local);
        // cap never raises above the ceiling, never lowers a cheaper tier.
        assert_eq!(super::cap_tier(Tier::Deep, Tier::Fast), Tier::Fast);
        assert_eq!(super::cap_tier(Tier::Local, Tier::Deep), Tier::Local);
        assert_eq!(super::cap_tier(Tier::Fast, Tier::Fast), Tier::Fast);
    }

    #[test]
    fn read_only_roles_are_classified() {
        for r in [
            AgentRole::Plan,
            AgentRole::Research,
            AgentRole::Review,
            AgentRole::Ask,
            AgentRole::Orchestrator,
        ] {
            assert!(r.is_read_only(), "{} should be read-only", r.label());
        }
        for r in [AgentRole::Impl, AgentRole::Debug, AgentRole::Test, AgentRole::Sre] {
            assert!(!r.is_read_only(), "{} should be able to mutate", r.label());
        }
    }

    #[test]
    fn new_roles_route_to_expected_client_and_tier() {
        let a = Assayer::default_rules();
        // Planning / research / debug / orchestration → claude/deep.
        assert_eq!(a.route(AgentRole::Plan, Complexity::Coding), claude_deep());
        assert_eq!(a.route(AgentRole::Research, Complexity::Simple), claude_deep());
        assert_eq!(a.route(AgentRole::Debug, Complexity::Complex), claude_deep());
        // Ask + Orchestrator (the cheap default fallback) → claude/fast.
        assert_eq!(a.route(AgentRole::Ask, Complexity::Simple), claude_fast());
        assert_eq!(a.route(AgentRole::Orchestrator, Complexity::Coding), claude_fast());
    }

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
        // The default fallback for a no-mode turn — kept cheap. Deep
        // orchestration is realised in the Phase-4 delegation loop.
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

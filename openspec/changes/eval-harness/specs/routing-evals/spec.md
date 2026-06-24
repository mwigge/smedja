## ADDED Requirements

### Requirement: Routing evals score the assayer deterministically by exact match

The harness SHALL evaluate routing quality by running labelled `(role, complexity)` inputs through `Assayer::route_decision` and comparing the resulting `(runner, tier)` destination to the case's expectation with the `ExactMatch` scorer. Routing evals MUST NOT make any model call and MUST be fully deterministic, so a single pass per case is authoritative.

#### Scenario: a correct routing expectation passes

- **WHEN** a routing case expects `(claude, deep)` for `(Review, Coding)` and `Assayer::default_rules()` routes it to `(claude, deep)`
- **THEN** the case verdict SHALL be pass
- **AND** no model call SHALL be made to reach that verdict

#### Scenario: a stale routing expectation fails

- **WHEN** a routing case expects a destination that differs from what the assayer returns for its input
- **THEN** the case verdict SHALL be fail
- **AND** the report SHALL identify the case by id and show expected versus actual destinations

### Requirement: The routing suite runs inside the default CI gate

The starter routing suite SHALL be exercised by a test that runs inside `cargo test --workspace`, so a change to the routing rules that the corpus does not expect fails CI immediately. The routing suite SHALL be runnable offline with no model access.

#### Scenario: routing suite passes against the default rules

- **WHEN** the test loads the `evals/routing` suite and runs it against `Assayer::default_rules()`
- **THEN** the suite SHALL report a 100% pass rate
- **AND** the test SHALL fail if any routing rule change makes a labelled case no longer match

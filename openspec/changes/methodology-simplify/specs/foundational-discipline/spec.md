## ADDED Requirements

### Requirement: TDD and clean-code discipline are always-on, steer-first

Test-driven development and clean-code discipline SHALL be foundational, not
selectable modes. On every code-writing turn the orchestrator SHALL inject a
discipline directive (write a failing test before implementation; no
`unwrap`/`expect`/`println!` in library code; small focused functions;
early-return over `else`) into the sealed system prefix, so the directive is
present in the agent's instructions every turn. The steering MUST be the primary
enforcement mechanism, with the diff backstop secondary.

#### Scenario: discipline directive present in every code-writing turn

- **WHEN** a code-writing turn is assembled under the default configuration
- **THEN** the sealed system prefix SHALL contain the TDD/clean discipline directive
- **AND** the directive SHALL be sealed into the cacheable prefix before `seal_prefix()` is called

#### Scenario: directive injected alongside workspace skills

- **WHEN** the orchestrator assembles pre-turn context
- **THEN** the discipline directive SHALL be injected on the same prefix that carries workspace skills
- **AND** it SHALL precede the stable-prefix seal so it does not change across the tool loop

### Requirement: The diff backstop is advisory and egregious-only, not a naive hard-block

The always-on TDD backstop SHALL NOT reject every change that adds an
implementation line without a co-located test. It SHALL raise only when a change
adds substantial new implementation with zero tests anywhere in the change, and
SHALL surface that result as an advisory warning rather than a blunt rejection.
The clean-code backstop (rejecting `unwrap`/`expect`/`println!` outside
`#[cfg(test)]`) MAY remain a hard backstop.

#### Scenario: refactor or helper edit is not blocked

- **WHEN** a change adds an implementation `fn` as part of a refactor, helper extraction, or doc edit with no new test in that change
- **THEN** the TDD backstop SHALL NOT block the write
- **AND** the discipline directive SHALL still have been present in the turn's system prefix

#### Scenario: substantial test-free implementation raises an advisory

- **WHEN** a change adds substantial new implementation with zero tests anywhere in the change
- **THEN** the TDD backstop SHALL raise an advisory verdict
- **AND** the verdict SHALL be advisory rather than a blunt always-reject

#### Scenario: clean backstop still blocks unwrap in library code

- **WHEN** a change adds a `.unwrap()` on a line outside a `#[cfg(test)]` block
- **THEN** the clean-code backstop SHALL block the write

### Requirement: The discipline is on by default with a per-workspace escape

The foundational discipline SHALL be enabled by default and SHALL be disablable
per workspace through a `[methodology]` block in `.smedja/config.toml` with
boolean `tdd` and `clean` fields, both defaulting to `true`. Resolution SHALL
mirror the security-config loader: a missing or unparseable config file SHALL
resolve to the all-true default and SHALL NOT block startup. Setting a flag to
`false` SHALL suppress both the steering directive clause and the backstop for
that discipline.

#### Scenario: absent config keeps the discipline on

- **WHEN** no `.smedja/config.toml` is present, or it has no `[methodology]` block
- **THEN** both `methodology.tdd` and `methodology.clean` SHALL resolve to `true`
- **AND** the discipline directive SHALL be injected on code-writing turns

#### Scenario: workspace escape disables a discipline

- **WHEN** `.smedja/config.toml` contains `[methodology]` with `tdd = false`
- **THEN** the TDD steering clause and the TDD backstop SHALL both be suppressed for that workspace
- **AND** the `clean` discipline SHALL remain on unless it too is set to `false`

#### Scenario: unparseable config never blocks

- **WHEN** `.smedja/config.toml` exists but cannot be parsed
- **THEN** the loader SHALL fall back to the all-true default
- **AND** startup SHALL NOT be aborted

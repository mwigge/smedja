## REMOVED Requirements

### Requirement: Selectable tdd and ponytail methodology modes

**Reason**: TDD and clean-code are now the foundational, always-on discipline
rather than selectable modes, so the `tdd`/`ponytail` selections are obsolete.
The `ponytail` gate was a byte-identical clone of the `clean` gate, so removing
it loses no enforcement.

**Migration**: A persisted session whose `mode` string is `"tdd"` or
`"ponytail"` degrades gracefully — `parse_mode` already returns `None` (ungated)
for unrecognised strings — and the foundational discipline now covers TDD/clean
regardless of the mode string.

### Requirement: The sre methodology mode

**Reason**: The `sre` mode was dormant — `run_gates` returned `Ok(())` for it and
never checked anything. A mode that silently passes is removed rather than kept.

**Migration**: The agent-routing meaning of `sre` (the `/agent sre` routing
concept) is unaffected; only its coupling to a methodology mode is severed.

## ADDED Requirements

### Requirement: No TUI command toggles the foundational discipline

The TUI SHALL NOT expose `/tdd` or `/ponytail` slash commands, and they SHALL
NOT appear in slash-command completion. A command that toggles a non-negotiable
discipline is an anti-pattern, and these names additionally collide with the
global Claude Code `/tdd` and `/ponytail` skills.

#### Scenario: /tdd is not a recognised command

- **WHEN** a user types `/tdd` in the TUI
- **THEN** the command SHALL NOT set any methodology mode
- **AND** `/tdd` SHALL NOT appear in slash-command completion

#### Scenario: /ponytail is not a recognised command

- **WHEN** a user types `/ponytail` in the TUI
- **THEN** the command SHALL NOT set any methodology mode
- **AND** `/ponytail` SHALL NOT appear in slash-command completion

### Requirement: Mode enum carries no Tdd, Ponytail, or Sre variants

`smedja_methodology::Mode` SHALL NOT define `Tdd`, `Ponytail`, or `Sre`
variants, and `parse_mode` SHALL resolve the strings `"tdd"`, `"ponytail"`, and
`"sre"` to `None` (ungated). The `Spec` and `Clean` variants are retained.

#### Scenario: removed mode strings resolve to ungated

- **WHEN** `parse_mode` is given `"tdd"`, `"ponytail"`, or `"sre"`
- **THEN** it SHALL return `None`
- **AND** no diff gate SHALL run for that session on the basis of those strings

#### Scenario: retained modes still resolve

- **WHEN** `parse_mode` is given `"clean"` or `"spec"`
- **THEN** it SHALL resolve to the corresponding retained `Mode` variant

### Requirement: Ponytail is an on-demand review skill, not a gate

The ponytail review lens (YAGNI / delete-over-add) SHALL be provided as a
workspace skill file loaded on demand through the existing skill-injection path,
not as a persistent methodology gate. The `ponytail` gate module SHALL be
removed.

#### Scenario: ponytail loads as a skill

- **WHEN** a workspace contains `.smedja/skills/ponytail.md`
- **THEN** its content SHALL be injectable through the workspace-skills path
- **AND** it SHALL NOT register any write-time diff gate

#### Scenario: no ponytail gate runs on writes

- **WHEN** a write tool call is intercepted
- **THEN** no ponytail diff gate SHALL run against the proposed diff

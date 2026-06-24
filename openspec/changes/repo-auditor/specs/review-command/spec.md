## MODIFIED Requirements

### Requirement: /review drives the read-only auditor with scope flags

The `/review` TUI slash command SHALL invoke the `audit.run` capability rather than building a single free-text `git diff HEAD` prompt. It SHALL accept scope flags — no argument for the working-tree diff, `<path>` for a path or whole-repo scope, `--branch <base>` for a branch range, and `--pr <ref>` for a pull request — and SHALL run the session in `"review"` (read-only) mode. After the audit completes it SHALL print a structured findings summary (counts per severity) and the path to the written markdown report.

#### Scenario: review of working-tree diff produces structured findings

- **WHEN** a user runs `/review` with uncommitted changes present
- **THEN** the command SHALL call `audit.run` with the working-tree diff scope under the read-only Review role
- **AND** it SHALL print a per-severity findings summary and the written report path
- **AND** it SHALL NOT submit a single free-text review turn

#### Scenario: review of a branch range

- **WHEN** a user runs `/review --branch main`
- **THEN** the command SHALL call `audit.run` with a branch-range scope of `main...HEAD`
- **AND** the findings summary and report path SHALL be printed

#### Scenario: empty working tree falls back to path scope

- **WHEN** a user runs `/review` and `git diff HEAD` is empty (all changes committed)
- **THEN** the command SHALL NOT hard-refuse
- **AND** it SHALL fall back to auditing the repository path scope

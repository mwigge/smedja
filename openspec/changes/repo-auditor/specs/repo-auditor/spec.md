## ADDED Requirements

### Requirement: Auditor runs the read-only Review role and never writes

The `audit.run` capability SHALL execute its exploration loop under the read-only Review role (`AgentRole::Review`), offering only the tools `graph_query`, `read_file`, and `list_files`. It MUST NOT invoke `write_file`, `edit_file`, or any write-arity bash command, and it MUST run the session in `"review"` mode so the existing `role_allows_write_bash` gate denies write-arity bash.

#### Scenario: write tool call is rejected

- **WHEN** the model emits a tool call for a tool outside `{graph_query, read_file, list_files}`
- **THEN** the loop SHALL reject the call without executing it
- **AND** it SHALL feed the rejection back as an error observation
- **AND** no file in the workspace SHALL be modified by the audit run

#### Scenario: review mode denies write bash

- **WHEN** the audit loop runs against a workspace
- **THEN** the session SHALL be in `"review"` mode
- **AND** `role_allows_write_bash` SHALL return false for that session

### Requirement: Audit supports four scopes

The auditor SHALL accept exactly one scope per run: working-tree diff (`git diff HEAD`), a path or the whole repository, a branch range (`git diff <base>...<head>`), or a pull request resolved to a branch range. Each scope MUST produce a non-empty seed context for the loop; an unresolvable pull-request reference MUST return an error rather than a partial audit.

#### Scenario: branch-range scope seeds from a diff

- **WHEN** `audit.run` is invoked with a branch base and head
- **THEN** the seed SHALL be the unified diff of `<base>...<head>`
- **AND** the loop SHALL audit that diff

#### Scenario: path scope seeds from graph and file listing

- **WHEN** `audit.run` is invoked with a path or whole-repo scope and no diff is available
- **THEN** the seed SHALL be assembled from a `graph_query` symbol listing plus a `list_files` tree
- **AND** the loop SHALL read files on demand within the iteration and token budget

#### Scenario: unresolvable pull request errors

- **WHEN** `audit.run` is invoked with a pull-request reference that cannot be resolved to a branch range
- **THEN** the RPC SHALL return an error
- **AND** no findings SHALL be persisted for that run

### Requirement: Findings are structured, de-duplicated, and persisted

The auditor SHALL parse the model output into typed `AuditFinding { severity, file, line, rule, rationale }` values, skipping malformed objects without failing the run, de-duplicate them on `(file, line, rule)` (and `(file, rule)` when line is absent), and persist each surviving finding as a `smedja-ingot` `AuditEvent` with `action_type = "audit_finding"`.

#### Scenario: malformed finding is skipped

- **WHEN** the model emits a findings array containing one malformed object and several valid objects
- **THEN** the malformed object SHALL be skipped
- **AND** the valid findings SHALL still be parsed, persisted, and reported

#### Scenario: duplicate findings collapse

- **WHEN** the loop surfaces two findings with the same file, line, and rule
- **THEN** only one SHALL be persisted and rendered
- **AND** the first occurrence's rationale SHALL be retained

#### Scenario: findings persist as audit events

- **WHEN** a finding survives de-duplication
- **THEN** it SHALL be written via `Ingot::insert_audit_event` with `action_type = "audit_finding"` and `actor = "review"`
- **AND** the run SHALL also persist `turn_start` and `turn_end` markers

### Requirement: Audit produces a deterministic markdown report

The auditor SHALL render surviving findings into a deterministic markdown report: a per-severity count header followed by sections ordered Critical, High, Medium, Low, Info, each finding rendered as a `` `file:line` — **rule** — rationale `` line. The report SHALL be written to a caller-supplied path when provided, otherwise returned inline; a `--format json` request SHALL instead return the full typed `AuditFinding` list with no field loss.

#### Scenario: smj audit <path> produces a report

- **WHEN** a user runs `smj audit <path>` against an initialised workspace
- **THEN** the daemon SHALL run the read-only audit loop over that path scope
- **AND** the CLI SHALL emit a markdown report containing the per-severity count header and severity-ordered sections
- **AND** the findings SHALL be persisted as audit events queryable via `smj audit query`

#### Scenario: report is deterministic across runs

- **WHEN** `render_report` is called twice on the same de-duplicated finding set
- **THEN** the two markdown outputs SHALL be byte-identical

#### Scenario: report written to a path

- **WHEN** `audit.run` is invoked with a report path
- **THEN** the markdown report SHALL be written to that path
- **AND** the RPC response SHALL carry the path and the per-severity counts

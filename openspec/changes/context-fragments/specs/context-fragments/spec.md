## ADDED Requirements

### Requirement: turn.submit expands inline context fragments

`turn.submit` SHALL expand inline context fragments in the submitted message `content` before the turn task is recorded. The recognised fragments are `@file <path>`, `@git`, `@branch`, and `@shell <cmd>`. A fragment MUST be recognised only when `@` begins a token (start-of-string or preceded by whitespace) and the kind is one of the four known kinds; any other `@<word>` token MUST be left verbatim. Each recognised fragment SHALL be replaced in place by a fenced content block, and surrounding prose MUST be preserved.

#### Scenario: known fragment expanded, prose preserved

- **WHEN** a user submits a message containing `@git` surrounded by prose
- **THEN** the recorded turn content SHALL contain the resolved git output in a fenced block at the fragment's position
- **AND** the surrounding prose SHALL be preserved unchanged

#### Scenario: unknown token left verbatim

- **WHEN** a user submits a message containing `email me at foo@bar.com` or `@unknownkind`
- **THEN** the `@`-bearing tokens SHALL be passed through unchanged
- **AND** no expansion SHALL be attempted for them

#### Scenario: no fragments passes through unchanged

- **WHEN** a user submits a message containing no recognised fragment
- **THEN** the recorded turn content SHALL equal the submitted text byte-for-byte

### Requirement: @file is constrained to the workspace boundary

The `@file <path>` fragment SHALL resolve its path through the workspace-boundary check (`assert_within_workspace`) against the session workspace. A path that resolves outside the workspace root MUST be rejected and the file MUST NOT be read. A path that resolves inside the workspace but is not a readable file MUST also yield an error marker rather than partial content.

#### Scenario: in-workspace file injected

- **WHEN** a user submits `@file src/lib.rs` and the path resolves inside the workspace
- **THEN** the fragment SHALL expand to a fenced block containing the file's contents

#### Scenario: path traversal denied

- **WHEN** a user submits `@file ../../etc/passwd` (or an absolute path outside the workspace)
- **THEN** the workspace-boundary check SHALL reject the path
- **AND** the fragment SHALL expand to a `path outside workspace` error marker
- **AND** the file SHALL NOT be read

#### Scenario: non-file path errors

- **WHEN** a user submits `@file <path>` that resolves inside the workspace but is a directory or is unreadable
- **THEN** the fragment SHALL expand to an error marker
- **AND** no partial or garbage content SHALL be injected

### Requirement: @shell execution is gated by cowork

The `@shell <cmd>` fragment SHALL run its command through the workspace shell runner (`exec_bash`) in the session workspace. When cowork is enabled, the command MUST be presented through the cowork approval gate before execution, and a denied command MUST expand to a denial marker rather than running.

#### Scenario: approved shell command output injected

- **WHEN** cowork is enabled, a user submits `@shell <cmd>`, and the command is approved
- **THEN** the command SHALL run in the session workspace
- **AND** the fragment SHALL expand to a fenced block containing the command output

#### Scenario: denied shell command not executed

- **WHEN** cowork is enabled, a user submits `@shell <cmd>`, and the command is denied
- **THEN** the command SHALL NOT be executed
- **AND** the fragment SHALL expand to a denial marker

### Requirement: @git and @branch inject read-only repository context

The `@git` fragment SHALL inject `git status --short` and `git diff HEAD` output for the session workspace, and the `@branch` fragment SHALL inject the current branch name (and upstream tracking info when present). Both run read-only commands through the workspace shell runner and MUST NOT require per-invocation cowork approval.

#### Scenario: git fragment injects status and diff

- **WHEN** a user submits `@git` in a session whose workspace is a git repository
- **THEN** the fragment SHALL expand to a fenced block containing the working-tree status and the diff against HEAD

#### Scenario: branch fragment injects current branch

- **WHEN** a user submits `@branch` in a session whose workspace is a git repository
- **THEN** the fragment SHALL expand to a fenced block naming the current branch

### Requirement: Fragment content is size-capped

Each fragment's injected content SHALL be capped by a per-fragment byte and line limit, and the aggregate injected content per message SHALL be capped by a per-message byte limit. Over-cap content MUST be truncated with a visible truncation marker. The per-fragment and per-message caps MUST be overridable via environment variables.

#### Scenario: over-cap fragment truncated

- **WHEN** a fragment resolves to content larger than the per-fragment cap
- **THEN** the injected content SHALL be truncated to the cap
- **AND** a visible truncation marker SHALL be appended inside the fenced block

#### Scenario: aggregate cap enforced

- **WHEN** the combined content of multiple fragments in one message exceeds the per-message cap
- **THEN** content SHALL be injected only until the aggregate cap is reached
- **AND** the truncated fragments SHALL carry a visible truncation marker

#### Scenario: env override changes the cap

- **WHEN** `SMEDJA_FRAGMENT_MAX_BYTES` is set
- **THEN** the per-fragment byte cap SHALL use the configured value instead of the default

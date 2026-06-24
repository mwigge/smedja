## ADDED Requirements

### Requirement: Command-keyed text filtering on the tool-result path

The daemon SHALL compress verbose `bash`/`run_command` text output in-process, keyed on the detected command, before the result is returned from `execute_tool` and pushed into working memory. Detection MUST key on the command tokens (one- and two-token keys, e.g. `cargo`, `git status`), and filtering MUST NOT alter the command string or spawn any additional process.

#### Scenario: known command compressed by the right strategy

- **WHEN** `bash` runs a known noisy command (e.g. `cargo build`) and returns verbose output
- **THEN** the result returned from `execute_tool` SHALL be the output compressed by the strategy registered for that command
- **AND** high-signal lines (e.g. `error[...]` / `warning:`) SHALL be preserved

#### Scenario: unknown command passed through unchanged

- **WHEN** `bash` runs a command with no registered filter
- **THEN** the output SHALL be returned without command-aware compression (pass-through)

### Requirement: Four filter strategies

The filter registry SHALL support four strategies — `smart-filter`, `group`, `truncate`, and `dedup` — and SHALL select exactly one per command entry. `smart-filter` MUST keep high-signal lines and drop progress/boilerplate; `group` MUST cluster related lines under headings with per-group counts; `truncate` MUST keep the first N lines and append an omitted-lines marker; `dedup` MUST collapse repeated lines into a single line with an occurrence count.

#### Scenario: dedup collapses repeated lines with a count

- **WHEN** a command's output contains a line repeated N times
- **THEN** the `dedup` strategy SHALL emit that line once
- **AND** the emitted line SHALL carry an occurrence count of N

#### Scenario: truncate keeps a head and marks omissions

- **WHEN** a command's output exceeds the truncate strategy's line limit
- **THEN** the result SHALL contain the first N lines
- **AND** SHALL append a marker naming the number of omitted lines and the `smedja_retrieve` recovery path

### Requirement: Exit codes and the tool contract are never altered

Command filtering SHALL operate on the textual result only. It MUST NOT change the command's exit status, success/failure classification, or any other part of the tool contract.

#### Scenario: exit code preserved across filtering

- **WHEN** a command exits with a given status and its output is filtered
- **THEN** the filtered result SHALL reflect the same success/failure outcome as the unfiltered result
- **AND** filtering SHALL affect only the textual content surfaced to the caller

### Requirement: Filtering composes with SmartCrusher without double-compression

The daemon SHALL route a tool result through exactly one compressor: JSON results through the SmartCrusher (`compress_tool_result`) and text command output through the command filter. The two MUST NOT both run on the same payload.

#### Scenario: JSON result takes the SmartCrusher branch

- **WHEN** a tool result parses as JSON
- **THEN** it SHALL be compressed by `compress_tool_result` (SmartCrusher)
- **AND** the command text filter SHALL NOT additionally run on that payload

### Requirement: Filtering bypass via environment variable

When `SMEDJA_NO_TOOL_COMPRESS=1` is set, command filtering SHALL be skipped and the output returned verbatim, matching the existing SmartCrusher bypass.

#### Scenario: bypass env var skips filtering

- **WHEN** `SMEDJA_NO_TOOL_COMPRESS=1` is set and a known noisy command runs
- **THEN** the command output SHALL be returned unchanged
- **AND** no command-aware compression SHALL be applied

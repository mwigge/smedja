## ADDED Requirements

### Requirement: Tool output is scanned for secrets on the return path

The executor SHALL pass tool-result content through the output scanner before the result is recorded. A match against a high-signal secret/credential pattern MUST produce an advisory `AuditEvent` with `action_type = "security_finding"` and the matching tool's `tool_name`. By default the tool result MUST be returned to the caller unmodified.

#### Scenario: a leaked credential is flagged but output is not altered

- **WHEN** a tool returns output containing a string matching a high-signal secret pattern
- **THEN** an advisory `security_finding` `AuditEvent` SHALL be recorded with `status = "warn"`
- **AND** the tool result returned to the caller SHALL be the original, unmodified content

#### Scenario: clean output produces no finding

- **WHEN** a tool returns output with no secret-pattern match
- **THEN** no `security_finding` event SHALL be recorded
- **AND** the result SHALL be returned unchanged

#### Scenario: scanner bypass honoured

- **WHEN** the output-scan bypass environment variable is set
- **THEN** tool output SHALL be returned unchanged and no scan finding SHALL be recorded

### Requirement: Output redaction is opt-in only

The scanner SHALL redact a matched secret from tool output only when `[security].enforce = true` and the match severity is at or above `enforce_min_severity`. With enforcement disabled (the default), output MUST never be redacted.

#### Scenario: default config does not redact

- **WHEN** enforcement is not configured and a tool result matches a secret pattern
- **THEN** the finding SHALL be recorded with `status = "warn"`
- **AND** the unredacted content SHALL be returned to the caller

#### Scenario: opt-in redaction replaces the matched secret

- **WHEN** `[security].enforce = true` at or above the match severity and a tool result matches a secret pattern
- **THEN** the matched secret SHALL be replaced with a redaction placeholder in the returned content
- **AND** the finding SHALL be recorded with `status = "blocked"`

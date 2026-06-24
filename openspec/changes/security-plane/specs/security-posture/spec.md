## ADDED Requirements

### Requirement: Startup posture scan emits advisory findings

On daemon startup the security plane SHALL scan the workspace root for risky hooks/configs and known IOC file markers, and SHALL classify any pending shell-command risk through the existing `smedja_ingot` command classifier. Each detection MUST be recorded as an advisory `AuditEvent` with `action_type = "security_finding"`, a severity in `error_kind`, and `status = "warn"`. The scan MUST NOT abort startup.

#### Scenario: risky config produces an advisory finding without blocking startup

- **WHEN** the daemon starts in a workspace containing a flagged config or IOC marker file
- **THEN** an `AuditEvent` with `action_type = "security_finding"` and `status = "warn"` SHALL be recorded for the detection
- **AND** the daemon SHALL complete startup normally and serve requests
- **AND** no action SHALL be blocked as a result of the finding

#### Scenario: scan error is non-fatal

- **WHEN** the posture scan encounters an unreadable path or I/O error
- **THEN** the scanner SHALL log a warning and continue
- **AND** daemon startup SHALL still complete

### Requirement: Command-risk reuse without a second blocklist

The posture scan SHALL classify command risk by calling the existing `smedja_ingot` command classifier (`classify_command`) and MUST NOT define a separate command blocklist.

#### Scenario: a Blocked command is reported as a finding, not refused by default

- **WHEN** a command classified as `Blocked` by `smedja_ingot::classify_command` is observed during a scan
- **THEN** a `security_finding` advisory `AuditEvent` SHALL be recorded with the highest severity
- **AND** with enforcement disabled (the default) the command SHALL NOT be refused by the security plane

### Requirement: Enforcement is opt-in and off by default

The security plane SHALL be advisory by default. Enforcement (promoting a finding to a block) SHALL occur only when the `[security]` config sets `enforce = true`, and even then only for findings at or above `enforce_min_severity` (which SHALL default to the highest severity). When the `[security]` block is absent, the plane MUST behave as if `enforce = false`.

#### Scenario: default config never blocks

- **WHEN** no `[security].enforce` value is configured and a high-severity posture finding is produced
- **THEN** the finding SHALL be recorded with `status = "warn"`
- **AND** startup and all subsequent actions SHALL proceed unblocked

#### Scenario: opt-in enforcement blocks only above threshold

- **WHEN** `[security].enforce = true` with the default `enforce_min_severity` and a highest-severity posture finding is produced
- **THEN** the finding SHALL be recorded with `status = "blocked"`
- **AND** the corresponding action SHALL be refused
- **AND** a lower-severity finding under the same config SHALL still be recorded with `status = "warn"` and SHALL NOT be blocked

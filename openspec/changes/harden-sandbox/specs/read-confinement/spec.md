## ADDED Requirements

### Requirement: Sandboxed reads confined to a system-dir allow-list

When a sandbox backend is active, a sandboxed command's filesystem reads SHALL be confined to a configurable allow-list of system directories plus the confined writable root. The user's home directory and secret directories (`~/.ssh`, `~/.aws`, `~/.config`, `~/.gnupg`) MUST NOT be readable unless an operator explicitly widens the allow-list to include them.

#### Scenario: secret directory is unreadable under the tightened allow-list

- **WHEN** a sandboxed command attempts to read a file under `~/.ssh` or `~/.aws/credentials`
- **THEN** the read SHALL be denied by the active backend (Landlock tightened allow-list, Seatbelt `(deny file-read*)`, or Docker structural isolation)
- **AND** the secret content SHALL NOT appear in the command's output

#### Scenario: system directories remain readable so the shell loads

- **WHEN** a sandboxed command reads an allow-listed system path (for example a shared library under `/lib` or `/usr`, or `/bin/sh`)
- **THEN** the read SHALL succeed
- **AND** the shell and its required shared libraries SHALL load and execute

### Requirement: Read allow-list is configurable with a documented failure mode

The read allow-list SHALL default to a bounded set of system directories that does NOT include the user's home directory, and SHALL be widenable via `SMEDJA_SANDBOX_READ_PATHS` (colon-separated paths appended to the defaults). Default paths that do not exist on the host SHALL be skipped without error. The failure mode of a command needing an unlisted path MUST be documented together with the override.

#### Scenario: operator widens the allow-list for an unlisted toolchain path

- **WHEN** `SMEDJA_SANDBOX_READ_PATHS` includes a path such as a toolchain directory under the home directory
- **THEN** a sandboxed command SHALL be able to read beneath that path
- **AND** the paths in `SMEDJA_SANDBOX_READ_PATHS` SHALL be added to, not replace, the platform defaults

#### Scenario: a program reading an unlisted path fails observably

- **WHEN** a sandboxed command reads a path that is neither in the defaults nor in `SMEDJA_SANDBOX_READ_PATHS`
- **THEN** the command SHALL fail with a permission or not-found error rather than silently succeeding
- **AND** the documentation SHALL name `SMEDJA_SANDBOX_READ_PATHS` as the remedy

### Requirement: Read confinement composes with the existing mode contract and telemetry

Read confinement SHALL be applied whenever the sandbox is enabled (`SMEDJA_SANDBOX_MODE` is not `off`) and a backend is available, without introducing a separate opt-in. The existing `auto|required|off` fallback semantics SHALL be unchanged, and the `smedja.sandbox.exec` span SHALL record whether read confinement was applied.

#### Scenario: read confinement on whenever the sandbox is on

- **WHEN** the sandbox is enabled and an OS-native or Docker backend is selected
- **THEN** read confinement SHALL be enforced for the command
- **AND** the `smedja.sandbox.exec` span SHALL carry `read_confined = true`

#### Scenario: sandbox off leaves reads unconfined

- **WHEN** `SMEDJA_SANDBOX_MODE=off`
- **THEN** the command SHALL run on the host with no read confinement
- **AND** the span SHALL carry `read_confined = false`

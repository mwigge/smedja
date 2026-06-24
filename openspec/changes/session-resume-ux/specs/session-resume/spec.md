## ADDED Requirements

### Requirement: smedja-tui attaches to an existing session via --session

`smedja-tui` SHALL accept a `--session <id>` launch flag. When the flag is present, the TUI MUST attach to the existing session identified by `<id>` instead of creating a new one, and it MUST validate the id via `session.get` before drawing any UI. When the flag is absent, the TUI SHALL create a new session as it does today.

#### Scenario: attach to a valid existing session

- **WHEN** `smedja-tui --session <id>` is launched and `session.get` confirms `<id>` exists
- **THEN** the TUI SHALL use `<id>` as its active session
- **AND** it SHALL NOT call `session.create`

#### Scenario: unknown session id fails fast

- **WHEN** `smedja-tui --session <id>` is launched and `session.get` reports `<id>` does not exist
- **THEN** the TUI SHALL print a `session not found` message and exit non-zero
- **AND** it SHALL NOT enter the terminal event loop

#### Scenario: no flag creates a new session

- **WHEN** `smedja-tui` is launched without `--session`
- **THEN** the TUI SHALL call `session.create`
- **AND** behaviour SHALL be unchanged from before this capability

### Requirement: Resumed sessions replay their history into the view

On attaching to an existing session, `smedja-tui` SHALL replay the session's prior turns into the view by calling `session.history` and seeding both the block store and the message panel, so the resumed conversation is visible and scrollable. The next live turn number MUST continue from the highest replayed turn.

#### Scenario: prior turns are rendered on resume

- **WHEN** a session with prior turns is resumed
- **THEN** the TUI SHALL build one block per turn from `session.history` and seed the block store and message panel with them
- **AND** the next submitted turn's number SHALL be one greater than the highest replayed turn number

#### Scenario: empty history resumes cleanly

- **WHEN** a session with no stored turns is resumed
- **THEN** replay SHALL be a no-op and the TUI SHALL attach without error

#### Scenario: unparseable turn record degrades to text

- **WHEN** a replayed turn contains a record with an unrecognised or missing role
- **THEN** the TUI SHALL render that record as plain text rather than dropping it or panicking

### Requirement: In-TUI picker lists and resumes sessions

`smedja-tui` SHALL provide a `/resume` command that lists resumable sessions from `session.list` and resumes a selected session in place without restarting the binary. The command MUST also accept a session id directly to resume without opening the picker.

#### Scenario: picker lists resumable sessions

- **WHEN** the user enters `/resume` with no argument
- **THEN** the TUI SHALL call `session.list` and display one selectable entry per session showing its short id, title, mode, and last-updated time

#### Scenario: selecting a session resumes it in place

- **WHEN** the user confirms a highlighted session in the picker
- **THEN** the TUI SHALL switch its active session to the selected id, clear the live display, and replay the selected session's history
- **AND** it SHALL NOT restart the process

#### Scenario: resume is rejected mid-turn

- **WHEN** the user enters `/resume` while a turn is awaiting a response
- **THEN** the TUI SHALL refuse to resume and show a status message
- **AND** the active session SHALL remain unchanged

### Requirement: Resume can rewind to a chosen turn via rollback

`smedja-tui` SHALL allow resume to rewind a session to a chosen turn. When a turn target is supplied (`/resume <id> <turn>` or `--session <id>` with `--turn <n>`), the TUI MUST call `session.rollback` with the session id and turn number before replaying, mirroring the `smj session rollback` contract. When no turn target is supplied, resume MUST NOT call `session.rollback`.

#### Scenario: turn target rewinds before replay

- **WHEN** the user resumes a session with a turn target
- **THEN** the TUI SHALL call `session.rollback` with `{ session_id, turn_n }` before replaying history
- **AND** it SHALL replay only the history up to the rewound turn

#### Scenario: plain resume is non-destructive

- **WHEN** the user resumes a session without a turn target
- **THEN** the TUI SHALL read history via `session.history` only
- **AND** it SHALL NOT call `session.rollback`

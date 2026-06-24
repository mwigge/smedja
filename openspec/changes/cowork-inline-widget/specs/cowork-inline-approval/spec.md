## ADDED Requirements

### Requirement: Inline widget resolves approvals via keyboard shortcuts

When one or more cowork approvals are pending, the TUI SHALL present an inline widget that intercepts keyboard input and SHALL resolve the first pending approval through `y` (approve), `n` (deny), and `m` (modify) shortcuts, without requiring a typed slash command. The widget MUST hold modal focus: while approvals are pending, decision keys MUST NOT leak to the input line, slash-completion popup, or history search.

#### Scenario: approve via the y shortcut

- **WHEN** an approval is pending and the user presses `y`
- **THEN** the TUI SHALL call `cowork.approve` with the pending item's `id` and the session id
- **AND** no other key handler (input, slash popup, history search) SHALL receive the keypress

#### Scenario: deny via the n shortcut

- **WHEN** an approval is pending and the user presses `n`
- **THEN** the TUI SHALL call `cowork.deny` with the pending item's `id` and a reason
- **AND** the decision SHALL be applied to the first pending item

#### Scenario: modify enters an instruction sub-mode

- **WHEN** an approval is pending and the user presses `m`
- **THEN** the widget SHALL enter modify mode and capture typed characters as a modify instruction
- **AND** pressing `Esc` SHALL cancel modify mode and return to the decision footer

### Requirement: Widget decisions honour the daemon resolved flag

The TUI SHALL remove a pending approval from the inline widget only when the daemon's `cowork.approve`/`cowork.deny`/`cowork.modify` response reports `resolved: true`. When the response reports `resolved: false` or the RPC call fails, the item MUST be retained in the pending list rather than silently discarded.

#### Scenario: resolved decision removes the item

- **WHEN** the user approves a pending item and the daemon responds with `resolved: true`
- **THEN** the item SHALL be removed from the inline widget's pending list

#### Scenario: unresolved decision retains the item

- **WHEN** the user approves a pending item and the daemon responds with `resolved: false`
- **THEN** the item SHALL NOT be removed from the pending list
- **AND** the TUI SHALL push a system message indicating the item was not found

#### Scenario: transport error retains the item

- **WHEN** a decision RPC returns a transport error
- **THEN** the item SHALL be retained
- **AND** the TUI SHALL push a system message reporting the error

### Requirement: Widget decisions emit transcript confirmation

The TUI SHALL push a system message into the transcript for each inline-widget decision so the resolved action is recorded. An approval SHALL report the tool name, a denial SHALL report the tool name, and a modify submission SHALL report the submitted instruction.

#### Scenario: approval confirmation

- **WHEN** an approval is resolved successfully via the widget
- **THEN** a system message naming the approved tool SHALL appear in the transcript

#### Scenario: modify confirmation echoes the instruction

- **WHEN** the user submits a modify instruction and the daemon resolves it
- **THEN** the item SHALL be removed
- **AND** a system message SHALL echo the submitted instruction

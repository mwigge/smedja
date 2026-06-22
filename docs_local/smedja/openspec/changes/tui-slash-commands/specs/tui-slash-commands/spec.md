## ADDED Requirements

### Requirement: Slash Popup Accepts Argument Commands
The TUI SHALL let users continue typing arguments after accepting a slash-command completion.

#### Scenario: Space accepts completion for arguments
- **GIVEN** the slash completion popup is visible
- **WHEN** the user presses Space
- **THEN** the selected completion is inserted with a trailing space
- **AND** the popup closes

#### Scenario: Tab accepts completion for arguments
- **GIVEN** the slash completion popup is visible
- **WHEN** the user presses Tab
- **THEN** the selected completion is inserted with a trailing space
- **AND** the popup closes

#### Scenario: Enter submits selected completion
- **GIVEN** the slash completion popup is visible
- **WHEN** the user presses Enter
- **THEN** the selected completion is inserted without a trailing space
- **AND** the command is submitted immediately

### Requirement: Slash Command Arguments
The TUI SHALL parse slash command names separately from their argument string.

#### Scenario: Tier command updates tier
- **WHEN** the user submits `/tier fast`, `/tier deep`, or `/tier local`
- **THEN** the TUI updates the current tier to the requested value

#### Scenario: Agent command updates mode
- **WHEN** the user submits `/agent impl`
- **THEN** the TUI updates the current mode to `impl`

#### Scenario: Health command performs daemon ping
- **WHEN** the user submits `/health`
- **THEN** the TUI calls `session.get`
- **AND** the messages panel shows the health result

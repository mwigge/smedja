## ADDED Requirements

### Requirement: Claude CLI Stream JSON Provider
Smedja SHALL invoke the local Claude CLI with stream-json output when the CLI is selected.

#### Scenario: First turn streams structured events
- **WHEN** a turn is submitted with no provider-native session id
- **THEN** the provider runs `claude --print --output-format stream-json --include-partial-messages --bare --dangerously-skip-permissions`
- **AND** text, tool call, tool result, usage, and session id events are parsed into adapter deltas

#### Scenario: Later turn resumes provider session
- **GIVEN** a previous Claude stream emitted a provider-native session id
- **WHEN** a later turn is submitted for the same smedja session
- **THEN** the provider includes `--resume <session-id>` in the Claude CLI invocation

### Requirement: Provider Tool Event Rendering
Smedja SHALL preserve provider-native tool events as compact turn output.

#### Scenario: Tool use appears in streamed output
- **WHEN** the provider emits a tool call delta
- **THEN** the daemon publishes a tool-called event
- **AND** the final turn response includes a collapsed tool call line

#### Scenario: Tool result appears in streamed output
- **WHEN** the provider emits a tool result delta
- **THEN** the final turn response includes a collapsed tool result line with result size

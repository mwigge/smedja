# Tasks: claude-turn-protocol

- [x] T1 — Extend Delta enum
File: `smedja-adapter/src/types.rs`
Add `ToolCall`, `ToolResult`, `SessionId` variants.
Update all match arms in adapter crates and daemon.

- [x] T2 — ClaudeStreamProvider
File: `smedja-adapter/src/claude_cli.rs`
Replace SubprocessProvider delegation with direct tokio::process spawn.
Args: `--print --output-format stream-json --include-partial-messages --bare --dangerously-skip-permissions [--resume <id>] "<msg>"`
Implement `parse_line` → Option<Delta>.

- [x] T3 — Session resume state
File: `smdjad/src/main.rs` (or new `session_state.rs`)
Add `claude_session_id: Option<String>` to in-memory session map.
On `Delta::SessionId(id)` from first turn, store it.
On subsequent turns, pass `--resume id` to ClaudeStreamProvider.

- [x] T4 — TUI tool call rendering
File: `smedja-tui/src/main_panel.rs`
Handle `Delta::ToolCall` and `Delta::ToolResult` events from turn stream.
Render as collapsed lines: `▶ ToolName(arg)` / `✓ ToolName → N chars`.

- [x] T5 — Tests
- Unit tests for `parse_line` with fixture JSON
- Integration test with mock claude binary (shell script emitting stream-json fixture)
- Provider priority test: session_id stored + second turn gets --resume flag

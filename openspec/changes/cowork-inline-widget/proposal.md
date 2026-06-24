## Why

The cowork approval gate is fully wired daemon-side: `cowork.set/approve/deny/modify/pending` are registered in `bin/smdjad/src/main.rs` (~603–655) and dispatched through `bin/smdjad/src/handlers/audit.rs`, which talks to the per-session `CoworkGate` (`bin/smdjad/src/cowork.rs`). Each of `cowork.approve/deny/modify` returns `{ "id": <id>, "resolved": <bool> }`, where `resolved` is `false` when the approval ID was not found (already resolved, expired, or unknown).

An inline widget already exists in the TUI — `bin/smedja-tui/src/cowork_widget.rs` renders the first pending item with `[y]`/`[n]`/`[m]` shortcuts, and `handle_key` (`bin/smedja-tui/src/main.rs` ~1240–1310) handles `y`/`n`/`m` plus the modify text sub-mode. But the widget path is not on a par with the typed slash-command path (`/approve`, `/deny` in `dispatch_slash`, ~864–925):

- **The widget path ignores `resolved`.** On `y`/`n`/`m` it fires the RPC with `let _ = client.call(...)` and unconditionally `state.pending_cowork.remove(0)`. If the RPC errors or the gate returns `resolved: false`, the user sees the item vanish as if it succeeded, with no message. The typed path inspects `resolved` and reports `approved:`/`denied:`/`item not found:`.
- **No confirmation feedback in the widget path.** The typed path calls `push_system_message`; the widget path is silent, so an approved/denied/modified decision leaves no trace in the transcript.
- **The modify flow drops the daemon's response.** On `Enter` in modify mode the widget sends `cowork.modify` with `let _ = client.call(...)` and removes the item without checking `resolved` or echoing the submitted instruction.
- **The widget key-dispatch branch has no tests.** `cowork_widget.rs` has render tests, but the `handle_key` cowork branch (the y/n/m/modify decision routing and `resolved` handling) is untested.

This change closes those gaps so the inline keyboard widget is the primary, fully-feedback approval surface and the typed `/approve`/`/deny` commands become the headless fallback rather than the path with better behaviour.

## What Changes

- **Route widget decisions through a shared resolver** that inspects the RPC `resolved` flag instead of `let _ = client.call(...)`. A pending item is removed from `state.pending_cowork` only when the daemon reports `resolved: true`; on `resolved: false` or an RPC error the item is kept and an explanatory system message is pushed.
- **Emit confirmation feedback for widget decisions**: `approved: <tool>`, `denied: <tool>`, and `modify sent: <instruction>` (or `item not found` / error text) via `push_system_message`, matching the typed-command path.
- **Surface the modify result**: on `Enter` in modify mode, send `cowork.modify`, inspect `resolved`, echo the submitted instruction in the transcript, and keep the item if the daemon did not resolve it.
- **Add key-dispatch tests** for the cowork branch of `handle_key` covering approve/deny/modify routing and the `resolved: false` retain-item behaviour, plus a render test that the widget shows the active decision shortcuts.
- **Out of scope**: changing the daemon `CoworkGate` contract, the `cowork.*` RPC shapes, the 200ms `cowork.pending` polling cadence, or removing the typed `/approve`/`/deny`/`/approvals` commands (they remain as the headless fallback).

## Capabilities

### New Capabilities

- `cowork-inline-approval`: the TUI resolves pending cowork approvals through the inline keyboard widget (`y` approve / `n` deny / `m` modify), honouring the daemon `resolved` flag and emitting transcript confirmation, so approvals no longer require typed slash commands.

## Impact

- `bin/smedja-tui/src/main.rs`: the cowork branch of `handle_key` (~1240–1310) routes `y`/`n`/`m` and the modify `Enter` through a shared resolver that checks `resolved` before removing the item and pushes confirmation messages; new tests for that branch.
- `bin/smedja-tui/src/cowork_widget.rs`: unchanged render contract; an added render assertion that decision shortcuts are present (no behavioural change to the widget struct).
- `bin/smdjad/src/handlers/audit.rs`, `bin/smdjad/src/cowork.rs`, `bin/smdjad/src/main.rs`: read-only — the existing `{ id, resolved }` contract is consumed, not changed.

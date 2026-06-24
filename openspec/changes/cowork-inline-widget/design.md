## Context

The inline cowork widget already renders and already captures keys. The relevant surface:

- `bin/smedja-tui/src/cowork_widget.rs`: `CoworkWidget { items: &[CoworkItem], modify_mode: bool, modify_input: &str }` renders the first pending item with a footer of `[y] approve  [n] deny  [m] modify`, or an `instruction:` input line when `modify_mode` is true. `overlay_rect(parent)` centres the overlay. `CoworkItem { id, tool, step_n, args_display, reasoning }`.
- `bin/smedja-tui/src/main.rs`:
  - `AppState` fields: `pending_cowork: Vec<CoworkItem>`, `cowork_modify_mode: bool`, `cowork_modify_input: String`, `last_cowork_poll: Option<Instant>`.
  - The render path (~2131–2142) draws the widget when `!state.pending_cowork.is_empty()`.
  - The poll loop (~2668–2707) calls `cowork.pending` every 200ms while `pending_task_id.is_some()` and appends genuinely-new IDs.
  - `handle_key` (~1240–1310) is the cowork branch: when `cowork_modify_mode`, `Esc` cancels, `Enter` submits `cowork.modify`, `Backspace`/`Char(c)` edit the buffer; otherwise `y`/`Y` → `cowork.approve`, `n`/`N` → `cowork.deny` (reason `"denied"`), `m`/`M` enters modify mode.
  - The typed fallback (`dispatch_slash`, ~852–925): `/approvals` → `format_approvals_list`, `/approve <id>` and `/deny <id> [reason]` inspect the RPC `resolved` flag and `push_system_message("approved: …" / "denied: …" / "item not found: …")`.
- Daemon contract (`bin/smdjad/src/handlers/audit.rs`): `approve`, `deny`, `modify` each return `{ "id": <id>, "resolved": <bool> }`; `resolved` is `false` when the ID is unknown to the `CoworkGate`. `pending` returns an array of `{ id, tool, step_n, args, reasoning }`. The gate (`bin/smdjad/src/cowork.rs`) resolves the suspended `intercept` future via a oneshot; `resolve` returns `false` when the ID is absent.

The defect: the widget branch fires `let _ = client.call(...)` and unconditionally `state.pending_cowork.remove(0)`, discarding both RPC errors and `resolved: false`. The typed path does the correct thing. The two paths should behave identically on the decision contract.

## Goals / Non-Goals

Goals:
- Make widget decisions (`y`/`n`/`m`) honour the daemon `resolved` flag: remove the item only on `resolved: true`; keep it and explain otherwise.
- Give widget decisions the same transcript feedback the typed path already emits.
- Surface the modify submission (instruction echo + resolved check) rather than silently dropping it.
- Cover the widget key-dispatch branch with tests.

Non-Goals:
- Changing the `cowork.*` RPC shapes or the `CoworkGate` contract (daemon-side is correct).
- Changing the 200ms poll cadence or the `pending_task_id.is_some()` poll guard.
- Removing the typed `/approve`/`/deny`/`/approvals` commands — they stay as the headless / scripted fallback.
- Multi-item batch approval or reordering the queue (the widget shows one item at a time with a `+N queued` count, unchanged).

## Decisions

**Decision: the widget cowork branch keeps modal focus and consumes all keys.**
When `!state.pending_cowork.is_empty()`, the cowork branch runs first in `handle_key` and `return Ok(())` after handling, so no key leaks to the slash popup, history search, or input. This is the existing behaviour and is preserved; modify mode is a nested modal that consumes text keys until `Esc`/`Enter`.
- Rationale: an approval gate is blocking by nature; the user must decide before typing anything else.

**Decision: key bindings stay `y`/`n`/`m` (case-insensitive), `Esc`/`Enter` inside modify.**
`y`/`Y` approve, `n`/`N` deny, `m`/`M` enter modify mode; in modify mode `Enter` submits, `Esc` cancels back to the decision footer, `Backspace`/`Char` edit the instruction. No new bindings are introduced — only the post-RPC handling changes.
- Rationale: the bindings already match the widget footer rendered by `cowork_widget.rs`; the gap is in result handling, not key mapping.

**Decision: route every widget decision through one resolver helper that returns whether to drop the item.**
Introduce a small async helper in `main.rs`, e.g. `resolve_cowork(client, session_id, method, params) -> bool`, that calls the RPC, reads the `resolved` boolean (defaulting to `false`), and on transport error returns `false`. The caller removes `pending_cowork[0]` only when the helper returns `true`, mirroring `dispatch_slash`'s `/approve` logic. This removes the duplicated `let _ = client.call(...); remove(0)` shape and unifies the two paths on the same contract.
- Rationale: single source of truth for "did the daemon accept this decision"; eliminates the silent-drop bug; keeps the typed and widget paths consistent.
- Alternative considered: inline the `resolved` check in each `match` arm. Rejected — three near-identical blocks invite drift; the helper is one reason to change.

**Decision: emit confirmation feedback identical in spirit to the typed path.**
On `resolved: true`: `push_system_message("approved: <tool>")`, `"denied: <tool>"`, or `"modify sent: <instruction>"`. On `resolved: false`: `"item not found: <tool>"`. On RPC error: `"cowork.<method> error: <e>"`. The tool name (and instruction for modify) comes from the `CoworkItem` being resolved, captured before removal.
- Rationale: the decision currently leaves no transcript trace; users need to see what they approved/denied, especially when the item is retained on failure.

**Decision: the modify flow echoes the instruction and respects `resolved`.**
On `Enter` in modify mode: take the `cowork_modify_input` buffer, send `cowork.modify` via the resolver, and only `remove(0)` when `resolved: true`. Push `"modify sent: <instruction>"` on success; keep the item and report otherwise. Clear `cowork_modify_mode`/`cowork_modify_input` after submission regardless (the buffer is consumed), matching the existing `std::mem::take` behaviour.
- Rationale: the daemon turns a modify decision into a re-prompt of the agent; the user should see the instruction they sent recorded in the transcript.

**Decision: how it composes with the existing cowork RPCs.**
No RPC is added or changed. The widget continues to source items from `cowork.pending` polling and to resolve them through `cowork.approve`/`cowork.deny`/`cowork.modify`. The only change is consuming the `{ id, resolved }` response the handlers already return. The daemon-side `CoworkGate` blocking semantics (oneshot resolve, fail-closed on timeout) are untouched.

## Risks / Trade-offs

- [Risk] Keeping an item on `resolved: false` could leave a stale item visible if the gate already resolved it server-side → Mitigation: the 200ms `cowork.pending` poll reconciles the list; a server-resolved item stops appearing in `pending` and the dedup-by-ID logic will not re-add it; the retained client item is only the local echo and will be superseded. Document that `resolved: false` means "no live gate accepted this", which is the actionable signal.
- [Risk] Added system messages could be noisy for rapid approvals → Mitigation: one concise line per decision, matching the typed path users already accept; no per-poll spam.
- [Risk] The resolver helper borrows `client` mutably inside a branch that also reads `state` → Mitigation: capture the needed `CoworkItem` fields (id, tool, instruction) into owned `String`s before the await, then push the message after, as the existing arms already do with `item.id.clone()`.

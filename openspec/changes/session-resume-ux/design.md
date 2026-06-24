## Context

The daemon already exposes everything resume needs; the gap is entirely in `smedja-tui`. Relevant surfaces:

- `session.list` (`bin/smdjad/src/handlers/session.rs::list`) → `[{ id, title, mode, created_at, updated_at }]` — exactly the columns a picker renders.
- `session.get` (`session.rs::get`) → `{ id, title, mode, created_at, updated_at, status, task_id }`, or an error when the id is unknown — the validation path for `--session`.
- `session.history` (`session.rs::history`) → `{ session_id, turns: [{ turn_n, created_at, messages }], audit: [...] }`, where each turn's `messages` is the parsed checkpoint blob (a JSON array of role/content records) ordered by `turn_n` ascending.
- `session.rollback` (`bin/smdjad/src/handlers/checkpoint.rs::rollback`) → takes `{ session_id, turn_n }`, atomically loads the target checkpoint and prunes later ones, returns `{ session_id, turn_n, messages_json, created_at }`.

The TUI side today:

- `Cli` (`bin/smedja-tui/src/main.rs` ~38–52): `--sock`, `--mode`/`-m`, `--tier`/`-t`. No `--session`.
- `main()` (~2255 onward) connects, unconditionally calls `session.create`, reads back `runner`/`model`/`tier`, then builds `AppState { session_id, turn_n: 0, ... }`.
- Live turns build `blocks::TurnBlock` (`bin/smedja-tui/src/blocks.rs`) and `MainPanel::push_line` (`bin/smedja-tui/src/main_panel.rs`) appends rendered lines. Nothing ever seeds these from history.
- The slash dispatch table (`dispatch_slash`) and the slash-completion popup (`SLASH_COMPLETIONS`, `runner_picker_mode`) already provide a working pattern for an interactive in-TUI picker (see `/switch`).

## Goals / Non-Goals

Goals:
- `smedja-tui --session <id>` attaches to an existing session and replays its history into the view.
- An in-TUI `/resume` picker lists resumable sessions and resumes the selected one without restarting.
- Resume can optionally rewind to a chosen `turn_n` via `session.rollback`, replaying history only up to that turn.
- Reuse the existing daemon RPCs unchanged; reuse the existing picker/popup pattern.

Non-Goals:
- No new daemon RPCs and no schema changes — `session.list/get/history/rollback` suffice.
- No `session.blocks` RPC (still stubbed by `smj session blocks`); replay reconstructs blocks client-side from `session.history`.
- No persistence of `WorkingMemory` across daemon restarts (owned by `wire-memory`).
- No change to live streaming, the thinking indicator, or token accounting.

## Decisions

**Decision: `--session <id>` selects attach vs. create.**
Add `#[arg(long)] session: Option<String>` to `Cli`. When `Some(id)`, `main()` calls `session.get` to validate the id; on success it uses that `session_id` and skips `session.create`; on error it prints a fail-fast message (`session not found: <id>`) and exits non-zero. When `None`, behaviour is unchanged (`session.create`).
- Rationale: a single flag, no subcommands; create remains the default so existing invocations are untouched.
- Alternative considered: a positional argument. Rejected — a named flag reads clearly alongside `--mode`/`--tier` and avoids ambiguity with a future prompt argument.

**Decision: history replay seeds `BlockStore` + `MainPanel` before the event loop.**
After attaching, call `session.history`, iterate `turns` in ascending `turn_n`, build a `TurnBlock` per turn from its `messages` array (a new `TurnBlock::from_history_turn(turn_n, messages)` constructor in `blocks.rs`), push each into the `BlockStore`, and push the rendered lines into `MainPanel`. Set `AppState.turn_n` to the highest replayed `turn_n` so the next live turn continues the sequence.
- Rationale: replayed turns must render identically to live ones; building real `TurnBlock`s (rather than dumping raw text) keeps the block browser, copy, and diff features working over history.
- Alternative considered: seed only `MainPanel` lines. Rejected — the block browser would be empty for resumed turns, breaking `b`/`c`/`D` over history.

**Decision: `/resume` opens an in-TUI picker reusing the slash-popup pattern.**
`/resume` with no argument calls `session.list`, populates the slash-completion popup with one entry per session (`<short-id>  <title>  <mode>  <updated_at>`), and sets a `session_picker_mode` flag analogous to `runner_picker_mode`. Enter on a highlighted row resumes that session in place: it swaps `AppState.session_id`, clears the live display via the existing `/clear` watermark mechanism, and replays the chosen session's history. `/resume <id>` resumes directly without opening the picker.
- Rationale: the popup, cursor movement, and Enter-confirm wiring already exist for `/switch`; a parallel mode keeps the code path consistent and small.
- Alternative considered: a separate full-screen overlay widget. Rejected — disproportionate for a P3; the existing popup already lists and selects.

**Decision: rollback-to-turn resume is an optional turn target on the resume path.**
`/resume <id> <turn>` (and a `--session <id>` paired with an optional `--turn <n>`) calls `session.rollback` with `{ session_id, turn_n }` before replaying, so history is rewound to the chosen checkpoint and later turns are pruned — mirroring `smj session rollback`. Without a turn target, resume is non-destructive (history is read via `session.history`, nothing is pruned).
- Rationale: rollback is already destructive-and-atomic in the daemon; the TUI only supplies the same two parameters the CLI does, keeping one contract.
- Alternative considered: a separate `/rollback` command in the TUI. Rejected — folding the turn target into resume keeps a single mental model ("resume this session, optionally rewound") and avoids a second destructive command surface.

## Risks / Trade-offs

- [Risk] A malformed or legacy checkpoint blob could fail to parse into messages → Mitigation: `session.history` already returns `Value::Array(empty)` for unparseable blobs (`session.rs::history`); `from_history_turn` treats a non-array / unknown-role record as plain text rather than panicking, so replay degrades gracefully.
- [Risk] Resuming in place mid-session could collide with an in-flight turn → Mitigation: `/resume` is rejected (with a status message) while `pending_task_id` is `Some`, so resume only runs when no turn is awaiting a response.
- [Risk] Rollback is destructive (prunes later checkpoints) and could surprise a user who only wanted to view history → Mitigation: rollback runs only when a turn target is explicitly supplied; plain resume never calls `session.rollback`. The picker labels the turn-target form distinctly.
- [Risk] Large histories could be slow to replay into the panel → Mitigation: replay is a bounded one-time seed over already-stored checkpoints, off the live event loop; the `MainPanel` scrollback already handles long content.
- [Risk] `--session` with a valid id but a daemon that has since lost the session → Mitigation: the `session.get` validation runs before any UI is drawn, so the failure is a clean fail-fast exit rather than a half-initialised dashboard.

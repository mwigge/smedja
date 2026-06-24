## Why

Session rollback and resume already exist at the daemon layer but are only half-exposed in the UX. The `session.rollback` RPC (`bin/smdjad/src/handlers/checkpoint.rs::rollback`), the `session.history` RPC (`bin/smdjad/src/handlers/session.rs::history`), and `session.list` (`session.rs::list`) are all wired and registered (`bin/smdjad/src/main.rs`), and the `smj session rollback <id> <turn>` CLI subcommand drives `session.rollback` (`bin/smj/src/main.rs::cmd_session_rollback`).

The TUI, however, never lets a user return to a prior conversation:

- `smedja-tui` always calls `session.create` on startup (`bin/smedja-tui/src/main.rs` ~2287). There is no way to attach to an existing `session_id`.
- The TUI `Cli` struct (`bin/smedja-tui/src/main.rs` ~38–52) exposes `--sock`, `--mode`/`-m`, and `--tier`/`-t`, but **no `--session`** flag.
- There is no in-TUI picker to browse and select a resumable session, even though `session.list` returns exactly the rows a picker needs (`id`, `title`, `mode`, `created_at`, `updated_at`).
- Prior turns are never replayed into the view: `MainPanel::push_line` and the `BlockStore` are only ever populated by live turns, never seeded from `session.history`.

The result is that a session can be rolled back from the CLI but the conversation it belongs to cannot be reopened in the dashboard — the primary surface users interact with. This change makes resume a first-class TUI flow.

## What Changes

- **Add `--session <id>` launch flag to `smedja-tui`**: when present, the TUI attaches to the existing session instead of calling `session.create`, validating it via `session.get` and failing fast with a clear message when the id is unknown.
- **Replay history into the view on resume**: after attaching, the TUI calls `session.history` and seeds the `MainPanel` and `BlockStore` with the prior turns (user/assistant/tool messages) so the resumed conversation is visible and scrollable, with `turn_n` continuing from the last checkpoint.
- **In-TUI session picker**: a `/resume` slash command (and the picker it opens) lists resumable sessions from `session.list` — id (short), title, mode, and last-updated — and resumes the highlighted one in place without restarting the binary.
- **Rollback-to-turn resume**: the picker and the `--session` flow accept an optional turn target so a user can resume a session rewound to a chosen checkpoint, mirroring the `smj session rollback` contract (`session.rollback` with `session_id` + `turn_n`) and replaying history only up to that turn.

Out of scope (referenced only): persisting `WorkingMemory` across restarts (owned by `wire-memory`); the `session.blocks` RPC that `smj session blocks` still stubs (`bin/smj/src/main.rs` ~619); any change to how live turns are streamed or rendered.

## Capabilities

### New Capabilities

- `session-resume`: `smedja-tui` can attach to an existing session via `--session <id>` or an in-TUI `/resume` picker, replay its history into the view, and optionally resume the session rewound to a chosen turn via `session.rollback`.

## Impact

- `bin/smedja-tui/src/main.rs`: add `--session` to `Cli`; branch startup between `session.create` and attach-to-existing (`session.get` validation); add the `/resume` slash command, picker state, and key handling; seed `MainPanel`/`BlockStore` from `session.history`; continue `turn_n` from the last checkpoint.
- `bin/smedja-tui/src/blocks.rs`: a constructor to build a `TurnBlock` from a stored history turn so replayed turns render identically to live ones.
- `bin/smedja-tui/src/main_panel.rs`: a seed path to push replayed history lines before the live event loop begins.
- No daemon changes: `session.list`, `session.get`, `session.history`, and `session.rollback` already provide the required surface.
- README / help text: document `smedja-tui --session <id>` and the `/resume` command.

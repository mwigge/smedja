## 1. Build TurnBlock from stored history

- [x] 1.1 Add a failing test in `bin/smedja-tui/src/blocks.rs` (`from_history_turn_renders_user_and_assistant`) asserting a `TurnBlock` built from a `messages` array containing a user and an assistant record renders header + both contents + footer
- [x] 1.2 Add a failing test (`from_history_turn_treats_unknown_role_as_text`) asserting a record with an unrecognised/missing role is rendered as plain text rather than dropped or panicking
- [x] 1.3 Implement `TurnBlock::from_history_turn(turn_n: u32, messages: &serde_json::Value)` in `blocks.rs`: iterate the array, map role/content into `push_text`/`push_tool_call`/`set_tool_outcome`, mark the block complete; return a ready-to-store block
- [x] 1.4 Run `cargo test -p smedja-tui blocks` — green

## 2. Add --session launch flag and attach-vs-create branch

- [x] 2.1 Add a failing unit test for the startup decision helper (`resume_when_session_flag_present` / `create_when_session_flag_absent`) asserting that a `Some(id)` flag routes to attach and `None` routes to create
- [x] 2.2 Add `#[arg(long)] session: Option<String>` to the `Cli` struct in `bin/smedja-tui/src/main.rs`
- [x] 2.3 Extract a `resolve_session(client, cli.session)` helper: when `Some(id)`, call `session.get` and return the validated id + runner/model/mode/tier; when `None`, call `session.create` (current behaviour)
- [x] 2.4 On `session.get` failure for a supplied id, print a fail-fast message (`session not found: <id>`) and exit non-zero before any terminal setup
- [x] 2.5 Run `cargo test -p smedja-tui` — green

## 3. Replay history into the view on resume

- [x] 3.1 Add a failing test (`replay_seeds_blocks_and_continues_turn_n`) over a fake `session.history` payload asserting the `BlockStore` gains one block per turn and `turn_n` equals the highest replayed turn
- [x] 3.2 Implement `replay_history(state, history_value)`: iterate `turns` ascending, build a `TurnBlock` per turn via `from_history_turn`, push into `BlockStore`, push rendered lines into `MainPanel`, and set `state.turn_n` to the max `turn_n`
- [x] 3.3 In `main()`, when attaching to an existing session, call `session.history` and pass the result to `replay_history` before entering the event loop
- [x] 3.4 Treat a missing/empty `turns` array as a no-op replay (fresh-looking but attached session)
- [x] 3.5 Run `cargo test -p smedja-tui` — green

## 4. In-TUI /resume picker

- [x] 4.1 Add a failing test (`resume_list_formats_session_rows`) asserting `session.list` rows render as `<short-id>  <title>  <mode>  <updated_at>` lines for the picker
- [x] 4.2 Add `/resume` to `SLASH_COMPLETIONS` and `HELP_TEXT`, and add a `session_picker_mode` flag to `AppState` (parallel to `runner_picker_mode`)
- [x] 4.3 In `dispatch_slash`, handle `/resume` with no args: call `session.list`, populate the popup entries, set `session_picker_mode`; handle `/resume <id>` (and `/resume <id> <turn>`) by resuming directly without opening the popup
- [x] 4.4 In `handle_key`, when `session_picker_mode` and Enter is pressed, resume the highlighted session: swap `state.session_id`, clear the display via the existing `/clear` watermark, then replay history
- [x] 4.5 Reject `/resume` with a status message while `pending_task_id` is `Some` (no resume mid-turn)
- [x] 4.6 Run `cargo test -p smedja-tui` — green

## 5. Rollback-to-turn resume

- [x] 5.1 Add a failing test (`resume_with_turn_calls_rollback_then_replays`) asserting that a turn target triggers a `session.rollback` call with `{ session_id, turn_n }` before replay, and no turn target does not
- [x] 5.2 In the resume path, when a turn target is supplied (`/resume <id> <turn>` or `--session <id> --turn <n>`), call `session.rollback` first, then replay the rewound history
- [x] 5.3 Add `--turn <n>` to `Cli` (only meaningful with `--session`); document that it rewinds destructively, mirroring `smj session rollback`
- [x] 5.4 Keep plain resume (no turn target) non-destructive — read via `session.history` only, never `session.rollback`
- [x] 5.5 Run `cargo test -p smedja-tui` — green

## 6. Docs

- [x] 6.1 Update `HELP_TEXT` in `bin/smedja-tui/src/main.rs` with `/resume [id [turn]]`
- [x] 6.2 Document `smedja-tui --session <id> [--turn <n>]` and `/resume` in the README TUI section

## 7. Verify

- [x] 7.1 Run `cargo test --workspace` — all green (no failures introduced)
- [x] 7.2 Run `cargo clippy -p smedja-tui -- -D warnings` — clean for the touched code
- [ ] 7.3 Manual check: `smedja-tui --session <known-id>` replays history; `--session <bad-id>` fails fast; `/resume` lists and resumes; `/resume <id> <turn>` rewinds — deferred: requires a live daemon + interactive terminal (not runnable in CI)
- [x] 7.4 Run `openspec validate session-resume-ux --strict` — clean

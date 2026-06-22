# Tasks: tui-slash-commands

- [x] T1 — Fix Space/Tab key in slash popup
File: `smedja-tui/src/main.rs`, input event handler block
Change Space key while `slash_popup_visible` to: insert completion + space, close popup.
Add Tab key same behaviour.
Enter key: insert completion (no space), submit.

- [x] T2 — Command argument dispatcher
File: `smedja-tui/src/main.rs`, `dispatch_slash()` fn
Parse `cmd + args` from input.
Implement `/tier <level>` → sets `state.tier`.
Implement `/agent <role>` → sets `state.mode`.
Implement `/health` → RPC health ping.
Others: `/spec`, `/tdd`, `/ponytail` — stubs OK.

- [x] T3 — Tests
Inline unit tests for `dispatch_slash`:
- `/tier fast` → state.tier == "fast"
- `/tier deep` → state.tier == "deep"
- `/agent impl` → state.mode == "impl"
- `/health` → no panic

Integration: keypress simulation (crossterm mock or manual state machine test).

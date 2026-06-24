## Why

The Go predecessor (milliways) let users splice live context directly into a prompt with inline fragments: `@file <path>` injected a file's contents, `@git` / `@branch` injected git status/diff/branch info, and `@shell <cmd>` injected a shell command's output. This kept the conversational loop tight — the user never had to copy-paste a file or paste terminal output by hand.

smedja has no equivalent. The TUI submits the raw input string verbatim to `turn.submit`; `bin/smedja-tui/src/main.rs::submit` builds `{ "session_id", "content": text }` with no expansion. The only context-assembling behaviour is the special-cased `/review` slash command (`dispatch_slash` ~L926), which shells out to `git diff HEAD` *client-side* and wraps the output in a prompt. There is no general fragment mechanism, and the building blocks the predecessor relied on — a workspace-boundary check (`bin/smdjad/src/executor/fs_tools.rs::assert_within_workspace`) and a sandboxed shell runner (`bin/smdjad/src/main.rs::exec_bash`) — live daemon-side, not in the TUI.

This change restores inline context fragments by expanding them daemon-side in `turn.submit`, reusing the existing workspace-boundary and shell-execution primitives so `@file` cannot escape the workspace and `@shell` honours the cowork approval gate.

## What Changes

- **Define the fragment grammar**: `@file <path>`, `@git`, `@branch`, `@shell <cmd>` recognised as inline tokens within the submitted message text. Each expands in place to a fenced block carrying the resolved content; surrounding prose is preserved.
- **Expand daemon-side in `turn.submit`**: a new fragment-expansion pass runs on `content` in `bin/smdjad/src/handlers/turn.rs::submit` before the turn task is recorded, so the stored/executed prompt is the expanded text. The TUI continues to send the raw string.
- **`@file` path-safety via the workspace boundary**: every `@file <path>` is resolved through `assert_within_workspace`; a path that escapes the workspace root is rejected and the fragment expands to an error marker rather than file contents (no traversal, no read outside the workspace).
- **`@git` / `@branch`**: `@git` injects `git status --short` plus `git diff HEAD`; `@branch` injects the current branch and upstream tracking info, both run in the session workspace.
- **`@shell <cmd>`**: the command runs through `exec_bash` in the workspace and, when cowork is enabled, passes through the `CoworkGate` approval flow before execution; the captured stdout/stderr expands in place.
- **Token-size guards**: each fragment's injected content is capped (per-fragment byte/line limit and a per-message aggregate cap); over-cap content is truncated with a visible marker so a single `@file` or `@shell` cannot blow the context window.

## Capabilities

### New Capabilities

- `context-fragments`: `turn.submit` expands inline context fragments (`@file`, `@git`, `@branch`, `@shell`) in the submitted message into fenced content blocks before the turn runs, with `@file` constrained to the workspace boundary, `@shell` gated by cowork, and per-fragment and per-message size caps.

## Impact

- `bin/smdjad/src/handlers/turn.rs`: `submit` runs the fragment-expansion pass over `content` before creating the turn task.
- `bin/smdjad/src/`: new fragment-expansion module (grammar parse + per-fragment resolvers) reusing `executor::fs_tools::assert_within_workspace`, `exec_bash`, and `cowork::CoworkGate`.
- `bin/smedja-tui/src/main.rs`: documentation/help text lists the fragment syntax; `submit` is unchanged (raw text still sent).
- README: the inline-context-fragment feature becomes accurate (was milliways-only).

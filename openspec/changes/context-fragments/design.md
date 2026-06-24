## Context

The TUI submit path is thin. `bin/smedja-tui/src/main.rs::submit` (~L336) trims the input, records it locally, and calls `turn.submit` with `{ "session_id", "content": text }` — no expansion. The TUI knows only its `session_id` and the daemon socket; it does **not** hold the session's workspace root. The one context-assembling behaviour today is `/review` in `dispatch_slash` (~L926), which runs `git diff HEAD` via `std::process::Command` in the TUI's own cwd and wraps the result in a prompt before calling `submit`. That is a one-off, not a general mechanism, and it carries no workspace-boundary safety because it does not read arbitrary user-named paths.

The primitives a fragment expander needs already exist on the daemon side:

- `bin/smdjad/src/executor/fs_tools.rs::assert_within_workspace(workspace, path_str)` — canonicalises `workspace.join(path_str)` and rejects anything that escapes the canonical workspace root, returning the byte-identical error `{"error": "path outside workspace"}`. This is the exact guard `@file` must reuse.
- `bin/smdjad/src/main.rs::exec_bash(cmd, workspace)` — runs `sh -c <cmd>` with `current_dir(workspace)` via `tokio::process::Command`, capturing stdout (on success) or stderr (on failure). This is the runner `@shell`, `@git`, and `@branch` use.
- `bin/smdjad/src/cowork::CoworkGate` — the human-in-the-loop approval gate (`intercept`/`approve`/`deny`/`modify`) already wrapping tool execution.

`turn.submit` is handled by `bin/smdjad/src/handlers/turn.rs::submit`, which reads `content`, records a `Task`, and publishes `TurnEvent::Started`. The expansion pass slots in here, before the task is recorded, so the stored prompt is the expanded text.

## Goals / Non-Goals

Goals:
- Recognise inline `@file <path>`, `@git`, `@branch`, `@shell <cmd>` fragments in a submitted message and expand each in place into a fenced content block.
- Keep `@file` strictly inside the workspace via the existing `assert_within_workspace` boundary check.
- Run `@shell` (and the git fragments) through `exec_bash` in the session workspace, honouring the cowork approval gate for `@shell`.
- Cap injected content per fragment and per message so a fragment cannot exhaust the context window.
- Leave the TUI submit payload unchanged (raw text still sent); expansion is purely daemon-side.

Non-Goals:
- Glob / directory expansion for `@file` (single path only; a directory path is rejected as not-a-file).
- Remote URL fetching or `@http`-style fragments.
- Streaming or lazy fragment resolution — fragments resolve synchronously during `turn.submit`.
- Replacing the `/review` slash command (it keeps its current client-side behaviour; it may later be re-expressed as `@git`, but that is out of scope here).
- Fragment expansion inside tool-result content or model output — expansion applies only to the user-submitted message.

## Decisions

**Decision: fragment grammar is `@<kind>` optionally followed by one argument token.**
- `@file <path>` — one path argument (the next whitespace-delimited token).
- `@git` — no argument.
- `@branch` — no argument.
- `@shell <cmd>` — the command is the remainder of the line after `@shell ` (so it may contain spaces); the fragment terminates at end-of-line.
A fragment is recognised only when `@` starts a token (preceded by start-of-string or whitespace), so email addresses and `foo@bar` inside prose are not mis-parsed. Each recognised fragment is replaced in place by a fenced block; unrecognised `@<word>` tokens are left verbatim.
- Rationale: mirrors the milliways surface while staying unambiguous to parse line-by-line; the one-argument rule keeps `@file` paths and `@shell` commands distinct (path = single token, command = rest of line).
- Alternative considered: requiring fragments on their own line. Rejected — milliways allowed inline use mid-sentence; preserving prose around the fragment is more ergonomic.

**Decision: expansion runs daemon-side in `turn.submit`, not client-side in the TUI.**
The TUI does not know the workspace root and has no access to `assert_within_workspace`, `exec_bash`, or the cowork gate; all three live in `smdjad`. Expanding client-side would duplicate the boundary check (a security-sensitive primitive) and the shell runner, and would run `@shell`/`@file` against the TUI's cwd rather than the session workspace. Expanding in `turn.submit` reuses the canonical primitives, resolves paths and commands against the *session* workspace, and routes `@shell` through the existing approval gate.
- Rationale: single trusted implementation of the workspace boundary and shell sandbox; correct workspace context; one path for cowork enforcement.
- Alternative considered: client-side expansion (like `/review`). Rejected — it would re-implement the security boundary in the less-trusted client and against the wrong directory. `/review` is acceptable client-side only because it reads no user-named path.

**Decision: `@file` path-safety is delegated entirely to `assert_within_workspace`.**
Each `@file <path>` resolves via `assert_within_workspace(workspace, path)`. On `Err` (path escapes the root — e.g. `../../etc/passwd`, an absolute path outside the workspace, or a symlink resolving outside), the fragment expands to a visible error marker (`[smedja: @file rejected: path outside workspace]`) and the file is never read. On `Ok`, the canonical path is read with `tokio::fs`.
- Rationale: the boundary check is already audited and byte-for-byte stable; `@file` must not introduce a second, weaker path policy.
- Note: a path that resolves inside the workspace but is a directory or unreadable expands to an error marker too, never partial/garbage content.

**Decision: `@shell` runs through `exec_bash` and the cowork gate; `@git`/`@branch` run through `exec_bash` without a gate.**
`@shell <cmd>` is arbitrary user-authored command execution, so when cowork is enabled it is presented through `CoworkGate::intercept` (tool `shell`, scrubbed args = the command) and only runs on approval; a denial expands to a `[smedja: @shell denied]` marker. `@git` and `@branch` run fixed, read-only commands (`git status --short` + `git diff HEAD`; `git rev-parse --abbrev-ref HEAD` + upstream lookup) and therefore do not require per-invocation approval.
- Rationale: `@shell` is the same trust class as the `bash` tool, so it inherits the same human-in-the-loop gate; the git fragments are read-only and bounded, matching the already-client-side `/review` precedent.
- Alternative considered: gating `@git`/`@branch` too. Rejected as friction with no added safety — the commands are fixed and read-only.

**Decision: per-fragment and per-message size caps with truncation markers.**
Each fragment's resolved content is capped (default 64 KiB and 2 000 lines per fragment); the sum of all injected fragment content in one message is capped (default 256 KiB). Over-cap content is truncated and a `[smedja: truncated N bytes]` marker is appended inside the fenced block. Caps are overridable via env (`SMEDJA_FRAGMENT_MAX_BYTES`, `SMEDJA_FRAGMENT_MAX_TOTAL_BYTES`).
- Rationale: a single `@file` on a large file or a chatty `@shell` could otherwise exceed the model window and dwarf the user's actual question; truncation is visible so the user knows content was dropped.
- Interaction with `wire-memory`: expansion happens before the prompt enters `WorkingMemory`, so the budgeting/strata logic sees the already-capped, expanded text — the caps are a first-line guard, not a replacement for context budgeting.

## Risks / Trade-offs

- [Risk] `@shell` is arbitrary code execution → Mitigation: routed through the same `CoworkGate` as the `bash` tool when cowork is enabled; runs in the session workspace via `exec_bash`; output is size-capped.
- [Risk] `@file` path traversal could leak files outside the workspace → Mitigation: every path goes through `assert_within_workspace`; rejection yields an error marker and no read. A dedicated traversal-denied scenario covers this.
- [Risk] A large `@file`/`@shell` could blow the context window → Mitigation: per-fragment and per-message byte/line caps with visible truncation markers.
- [Risk] Over-eager parsing could mangle `foo@bar` / email addresses in prose → Mitigation: a fragment is recognised only when `@` begins a token (start-of-string or whitespace) and `<kind>` is one of the four known kinds; everything else is left verbatim.
- [Risk] Expansion latency (shell/git/file I/O) blocks `turn.submit` → Mitigation: fragments resolve with async I/O (`tokio::fs`, `tokio::process`); the existing `@shell` cowork timeout bounds the wait, and resolution happens once at submit time, not per provider round-trip.

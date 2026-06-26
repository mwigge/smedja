# smedja-tui Reference

`smedja-tui` is a ratatui-based agent dashboard. It runs inside any terminal (including the smedja GPU terminal). It connects to a running `smdjad` daemon via a Unix domain socket and streams turn events in real time.

---

## Launch

```sh
smedja-tui [OPTIONS]
```

| Option | Env | Purpose |
|--------|-----|---------|
| `--sock <path>` | `SMEDJA_SOCK` | Override the daemon socket path (default: `$XDG_RUNTIME_DIR/smdjad.sock`) |
| `--mode <m>` | | Agent mode: `impl`, `review`, `test`, `sre`, `explain` |
| `--tier <t>` | | Tier override: `local`, `fast`, `deep` |
| `--session <id>` | | Attach to an existing session and replay its history |
| `--turn <n>` | | Rewind the resumed session to turn `n` before replaying (destructive) |

---

## Layout

```
┌─────────────────────────────────────────────────┐
│  [session rail]  │  main panel                  │  [right rails]
│  (Ctrl-W)        │  turn blocks, system messages │  context / LSP /
│                  │                               │  obs / cockpit
│──────────────────┴───────────────────────────────┤
│  input bar                          {N}c ≈{M}tok │
│──────────────────────────────────────────────────│
│  [I] [tier] [mode] [sess-id] [runner]  HH:MM:SS │  status bar
└──────────────────────────────────────────────────┘
```

The right rails are independent — multiple can be open at once (each takes a column). The left session rail and right rails reduce the main panel width.

---

## Modes

The TUI has two keyboard modes:

- **Input mode** `[I]` — the input bar is focused. Characters type into the bar. Press `Esc` to enter scroll mode.
- **Scroll / normal mode** `[N]` — the main panel is focused. Navigation keys scroll the view. Press `i` or `a` to return to input mode.

The current mode is shown in the status bar's `[I]` / `[N]` badge.

---

## Slash Commands

Type a `/` in the input bar to open the autocomplete popup. Commands are filtered as you type.

### Session

| Command | Description |
|---------|-------------|
| `/session` | Show the current session ID, mode, tier, and turn count |
| `/resume` | Open an interactive session picker; press `Enter` to reattach |
| `/resume <id>` | Resume the named session directly |
| `/resume <id> <turn>` | Rewind to turn `n` then replay (destructive — prunes later turns) |
| `/clear` | Clear the message display; the session conversation is preserved |
| `/briefing` | Show the session briefing (system prompt summary) |
| `/quit` | Exit smedja-tui |

### Model and Runner

| Command | Description |
|---------|-------------|
| `/model` | List available models with GPU fit annotations (local runner) or runner's model list |
| `/model <name>` | Hot-swap to the named model; local runner: calls `local.swap`, not a relabel |
| `/switch` | Open an interactive runner picker |
| `/switch <runner>` | Switch to the named runner immediately |
| `/takeover <runner>` | Fork the current session to a new runner (keeps history) |
| `/tier <t>` | Set the routing tier: `local`, `fast`, or `deep` |
| `/login` | Authenticate with the current runner (OAuth / API key flow) |

### Loop Engine

| Command | Description |
|---------|-------------|
| `/loop status` | Show the status of the current active loop |
| `/loop list` | List all loops for the current workspace |
| `/loop create <goal>` | Start a new loop for the given goal |
| `/loop cancel` | Cancel the running loop |

### Agent and Cowork

| Command | Description |
|---------|-------------|
| `/agent` | List available named agents |
| `/agent <id>` | Run the named agent |
| `/approve` | List pending cowork items |
| `/approve <id>` | Approve the named cowork item |

### Code and Review

| Command | Description |
|---------|-------------|
| `/review` | Send the current `git diff HEAD` to the review role |
| `/test` | Run the project test suite (auto-detects `cargo`, `npm`, `go`, `python`) |
| `/test cargo` | Force cargo test (useful in monorepos) |
| `/test npm` | Force npm test |
| `/test go` | Force go test |
| `/test py` | Force pytest |
| `/lsp` | Show LSP server status and current diagnostic summary |

### Governance

| Command | Description |
|---------|-------------|
| `/gov list` | List all govctl artifacts (work items, RFCs, ADRs) with id/kind/status/title |
| `/gov show <id>` | Show full detail for an artifact (e.g. `WI-001`, `RFC-001`, `ADR-001`) |
| `/gov create work-item <title>` | Create a new work item TOML in `gov/work-items/` |
| `/gov create rfc <title>` | Create a new RFC TOML in `gov/rfc/` |
| `/gov create adr <title>` | Create a new ADR TOML in `gov/adr/` |
| `/gov transition <id> <status>` | Update the status field of an artifact |

IDs auto-increment within their kind (WI-001, WI-002…; RFC-001…; ADR-001…). Valid transition statuses: `planned`, `in_progress`, `done`, `cancelled`, `draft`, `accepted`, `rejected`, `superseded`.

### OpenSpec

| Command | Description |
|---------|-------------|
| `/spec` | Browse OpenSpec changes in `openspec/changes/` |
| `/drawio <slug>` | Generate a draw.io mxGraph XML diagram for the named change |
| `/pptx <slug>` | Generate a python-pptx script for the named change |

### Information

| Command | Description |
|---------|-------------|
| `/help` | Show the full help text (commands + keybindings) |
| `/health` | Check daemon connectivity (calls `session.list`) |
| `/metrics` | Show token usage and cost rollup by runner |
| `/quota` | Show daily token usage vs. `SMEDJA_DAILY_TOKEN_LIMIT` |
| `/version` | Print the TUI and daemon versions; check for a newer release |
| `/upgrade` | Download and install the latest release in-place |

---

## Inline Context Fragments

Fragments are expanded into your message text before the turn reaches the daemon. They let you inject file contents, git state, or shell output without copying and pasting.

| Fragment | What it injects |
|----------|----------------|
| `@file <path>` | The contents of a workspace file. The path is validated to stay inside the workspace root. |
| `@git` | Output of `git status --short` and `git diff HEAD` (current working tree changes). |
| `@branch` | The current branch name and its upstream ref. |
| `@shell <cmd>` | The stdout of a shell command. When cowork mode is on, the command pauses for approval before running. |

---

## Key Bindings

### Input Mode

The input bar is a line editor with Emacs-style movement and a kill ring.

| Key | Action |
|-----|--------|
| `Enter` | Submit the message |
| `↑` / `Ctrl-P` | Browse prompt history backwards |
| `↓` / `Ctrl-N` | Browse prompt history forwards |
| `Ctrl-R` | Toggle reverse history search — type to filter, `Enter` to accept |
| `Ctrl-G` | Open `$VISUAL` / `$EDITOR` / `vi` for multi-line composition; contents loaded back on exit |
| `Ctrl-B` | Move cursor left one character |
| `Ctrl-K` | Kill from cursor to end of line (push text to kill ring) |
| `Ctrl-U` | Kill from start of line to cursor (push text to kill ring) |
| `Ctrl-Y` | Yank the most recent kill ring entry at the cursor |
| `Esc` | Enter scroll / normal mode |

The kill ring holds up to 16 entries (VecDeque, oldest dropped on overflow). `Ctrl-Y` always yanks the most recent entry.

### Scroll / Normal Mode

| Key | Action |
|-----|--------|
| `i` / `a` | Return to input mode |
| `j` / `k` | Scroll the main panel down / up one line |
| `G` | Scroll to the bottom |
| `gg` | Scroll to the top (press `g` twice) |
| `v` | Start visual line-selection mode (anchor at current line) |
| `y` | Yank the current selection to the in-memory clipboard |
| `t` | Copy the W3C `traceparent` from the most recent turn to the clipboard |
| `T` | Expand / collapse the thinking block badge (when the model emits thinking tokens) |
| `[` / `]` | Move the session rail cursor up / down |
| `Enter` | Resume the highlighted session (when session rail is open and focused) |
| `Esc` | Exit visual selection / return to input mode |

### Panel Toggles (both modes)

`Ctrl-A` works in both input and scroll mode. The rest work in scroll mode only.

| Key | Panel | Default |
|-----|-------|---------|
| `Ctrl-A` | Role cockpit — active role, tier, runner, in-flight turn status | Off |
| `Ctrl-F` | Context fill rail — live token slot breakdown | Off |
| `Ctrl-L` | LSP diagnostic panel | On |
| `Ctrl-O` | Observability panel — latency, tokens, cost, OTel status | On |
| `Ctrl-T` | Metrics overlay — per-runner token/cost/error rollup | Off |
| `Ctrl-W` | Session browser left-rail — 5 s refresh from `session.list` | Off |

---

## Panels

### Main Panel

The primary display area. Shows:

- **User messages** — your submitted text
- **System messages** — TUI notifications (queued, error, status changes)
- **Turn blocks** — structured `TurnBlock` output per assistant turn, each with:
  - Header: `turn N · runner · model · input→output tok · latency`
  - Tool call entries: `▸ tool_name  args  → result`
  - Inline diff for `edit_file` calls: `+` / `-` lines
  - Footer: `✓ complete - trace: <traceparent-short>`
- **Thinking blocks** — dim collapsible block while streaming; `╌ thinking (N chars) [T to expand] ╌` badge on completion

**Panel search**: press `/` (the literal slash, not a slash command) while in scroll mode to enter search — type to highlight matching lines.

### Role Cockpit (`Ctrl-A`)

A right-rail panel showing the active agent / role state during a turn. One row shows:

```
mode: impl   tier: deep   runner: claude
agent: impl-role   status: ●  in-flight
```

- `mode` / `tier` / `runner` from the current session
- `agent` from `TurnEvent.CorrelationCtx.agent_name` (populated on `started` NDJSON events)
- Status: `● in-flight` (spinning) / `◌ waiting` / `─ idle`
- Panel height: 6 rows

### Context Fill Rail (`Ctrl-F`, scroll mode only)

Shows live context window utilisation:

```
context
fill: ████████░░ 44%
locked prefix: 12 k
working:       22 k
budget:        50 k
```

Colour thresholds: green < 60%, yellow 60–80%, red > 80%.

### LSP Diagnostic Panel (`Ctrl-L`)

Shows the current LSP server status and diagnostic counts from `lsp.status` / `lsp.diagnostics` RPC polls (5 s interval):

```
LSP  rust-analyzer  running
E:2  W:5  H:0
```

### Observability Panel (`Ctrl-O`)

Shows:
- Session cost and token totals for the current session
- Turn latency samples (p50/p95/p99 over the last 50 turns)
- OTel status (whether `SMEDJA_OTLP_ENDPOINT` is configured)
- Daily token total vs. quota

### Metrics Overlay (`Ctrl-T`)

Per-runner rollup snapshot (fetched once on open, refreshed every ~3 s):

```
runner       tokens in   tokens out   cost ($)   errors
claude       1,200,000      340,000      4.12          0
local          800,000      220,000      0.00          1
```

Plus the token-economy savings section (SmartCrusher + stable-prefix savings).

### Session Browser (`Ctrl-W`)

A 28-column left-rail panel listing recent sessions (id, title, mode, last-updated). Refreshes every 5 s from `session.list`. Navigate with `[` / `]`; press `Enter` to resume.

---

## Status Bar

The status bar renders a row of modules at the bottom of the screen:

```
[I]  [deep]  [impl]  [a1b2c3d4]  [claude]  14:22:31
mode  tier    mode    session       runner    time
```

- `[I]` / `[N]` — current mode (input / scroll)
- Tier badge — colour-coded: cyan=local, blue=fast, magenta=deep
- Mode badge — the agent mode for this session
- Session badge — first 8 characters of the session ID
- Runner badge — current runner name
- Time — UTC wall clock

The status bar is TOML-configurable. Format: Starship-compatible module format.

---

## Cowork Mode

Enable with `/cowork on` (or `smj session start --cowork`). While active, every tool call from the agent pauses for approval before execution.

Approval prompts appear in the main panel as text lines. Press:
- `y` — approve and execute the tool call
- `n` — deny (the reason is fed back to the agent as a tool error)
- `m` — enter modify mode and rewrite the arguments before execution

Each decision is recorded in `smedja-ingot` as an audit event.

---

## OSC-9 Desktop Notifications

When a turn completes, the TUI emits `\e]9;turn complete\x07` to stdout. Terminals that support OSC-9 (Windows Terminal, iTerm2, and compatible emulators) display a desktop notification. No configuration required.

---

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `SMEDJA_SOCK` | `$XDG_RUNTIME_DIR/smdjad.sock` | Override daemon socket path |
| `SMEDJA_DAILY_TOKEN_LIMIT` | *(unset)* | Daily token budget; `/quota` shows a usage bar |
| `SMEDJA_OTLP_ENDPOINT` | *(unset)* | OTLP collector endpoint; enables trace footer when set |
| `NO_COLOR` | *(unset)* | Disable all colour output when set to any value |
| `VISUAL` / `EDITOR` | *(system)* | Editor opened by `Ctrl-G` for multi-line composition |

# smedja presentation & agent-journey checklist

Third checklist, focused on **how output looks and reads** — the layer the user
called "severely lacking visually/UX-wise." Companion to:
- `checklist-tui.md` — app behaviour (wrap/scroll/selection/ownership)
- the terminal-core plan — VT/grid/render correctness

Benchmarks (what good looks like): **toad** (batrachianai/toad), **opencode**
(opencode-ai), claude-cli, codex, ecc.tools; terminal feel from alacritty/kitty;
status-line ideas from starship.

## What the references do well (and we don't yet)

- **toad**: streaming Markdown with syntax-highlighted code fences, tables,
  quotes, lists; unified/side-by-side **diffs** with highlighting; never garbles
  rich output; full mouse + paste.
- **opencode** (Bubble Tea): chat page + overlays — logs (Ctrl+L), sessions
  (Ctrl+A), model picker (Ctrl+O), command palette (Ctrl+K); permission dialogs;
  spinners; external editor (Ctrl+E).
- **ecc.tools / claude-cli**: tool output as **clean cards** — severity/status
  chips, concise human summaries, file paths — "clarity over decoration."

## Observed gaps in smedja (with code pointers)

Legend: ✅ done · ⚠️ partial · ❌ missing · 🔧 fixed in this pass

### P — Tool-call & result presentation  ← worst offender
- 🔧 Tool calls now render as a **card** — `<glyph> <label>  <summary>` (e.g.
  `⌘ bash  find . -type f`), built by `tool_call_card`/`tool_glyph_label`
  (`smedja-tui/src/main.rs`) + `MainPanel::push_styled_line`. The input is a
  human summary (`summarize_tool_input`, `smdjad/src/common.rs`), not raw JSON.
- 🔧 Tool results now render as `↳ <ok|error> · <first-line preview>`
  (`summarize_tool_result`) — no `tool_use_id`, no bare byte count; dim, own line.
- 🔧 **Duplication fixed** at the source: `common.rs` published every tool call
  BOTH as a structured `ToolCalled` event AND as an `AssistantDelta` of the same
  `▶ …` text → doubled render. Dropped the duplicate delta.
- 🔧 Verbose labels: `tool_glyph_label` maps known tools → short glyph+name
  (`ToolSearch` → `⌕ search`), capping unknown names. (collapsible details: TODO)

### M — Markdown / rich text rendering
- 🔧 Streamed assistant text now classified per completed line via
  `MainPanel::finalize_last_line` (the delta path calls it on each newline), so
  streamed **code fences are syntect-highlighted** and **diff lines coloured** —
  the same pipeline `push_line` already had, no longer bypassed by `push_delta`.
- ⚠️ Tables, blockquotes, lists, inline `code`/bold/italic — still TODO (line
  classification covers fences + diffs + math today).

### D — Diffs
- 🔧 `` ```diff ``/`` ```patch `` blocks and standalone diffs render as cards:
  coloured left gutter (`▎`), dim bold file/meta headers, accented `@@` hunk
  headers (`diff_line_spans`/`is_diff_marker` in `main_panel.rs`). Gutter is
  display-only so copied text stays clean. (side-by-side view: TODO)

### J — Agent journey / status
- 🔧 Status line restyled starship-style: segmented, brand-coloured runner chip
  + tier/mode/session, dim dot separators, INSERT/SCROLL pip (`status_bar_line`).
  No Nerd-Font dependency (colour segments, not powerline glyphs).
- 🔧 Discoverability: dim right-aligned hint on the status row
  (`/help · ^W sessions · ^O obs · ^L lsp`, `status_hint_line`).
- ⏭️ Command palette intentionally NOT added: the input uses an emacs keymap
  (Ctrl-A/E/K/U/P/N/B/F…) so a Ctrl-chord palette would clash, and `/` already
  is the command surface (`/help`, slash completion). Per
  [[feedback-smedja-tui-modeless-input]] no new modal that can trap typing.
- ⚠️ Per-tool progress / turn-boundary author chips (You/Claude) — still TODO.

### F — Functional blockers found alongside
- 🔧 **Early "timeout"**: `STREAM_TIMEOUT_SECS = 90` was an *absolute* deadline
  from stream start, so any turn >90s total (a whole review) died with "stream
  timed out", misreported as the 900s cap. Now an **idle** timeout (resets per
  event); message says "stream stalled: no events for 90s"
  (`stream_server.rs:292`).
- ❌ **Agent Bash sandbox fails** ("read-only filesystem") — the nested
  claude-cli could not run any command, so the review degraded to retries. Needs
  root-cause: is smdjad running the provider subprocess under a read-only
  sandbox, or is cwd/TMPDIR unwritable? (separate investigation)

## Priority order (highest user-visible impact first)
1. **P** — tool-call/result **cards** + de-dup + compact labels. Biggest win;
   directly fixes "hard to read" and "ToolSearch too long".
2. **M** — streaming Markdown + syntax highlighting for assistant text.
3. **F** — agent Bash sandbox root-cause (so reviews actually run).
4. **D** — diff cards. 5. **J** — overlays/command palette + starship-style status.

## Acceptance (the gate)
- Unit test: a recorded claude-cli NDJSON turn (text + tool_use + tool_result +
  retries) rendered into `MainPanel` produces **one** card per tool call, a
  readable result summary, and highlighted code — asserted on the buffer via
  `TestBackend`. No `toolu_<id>` or raw-JSON leakage in the output.
- Manual: run "review this repo" in a fresh smedja; compare side-by-side with
  opencode/toad screenshots — code highlighted, tool cards clean, no duplication,
  no premature timeout.

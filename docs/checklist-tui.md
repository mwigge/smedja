# smedja-tui acceptance checklist (app + chrome layer)

This is an **addition** to the GPU-terminal correctness checklist (VT/grid/render
in `st-pty`/`st-render`). That checklist made the terminal *core* correct. It does
**not** cover the two layers where the user-visible bugs actually keep landing:

1. **smedja-tui** — the ratatui agent-chat app that runs *inside* the terminal.
2. **terminal chrome** — overlays the terminal draws on top of the grid
   (top bar, agent-block overlay).

## Why we kept missing these

- **Wrong layer instrumented.** Golden VT conformance + `smoke-term.sh` test the
  terminal core, which was already mostly correct. The app layer and the chrome
  overlays had **zero** acceptance criteria or automated tests, so omissions were
  invisible until a screenshot.
- **Symptom-driven, not contract-driven.** Each fix targeted the one artifact in
  the screenshot; we never enumerated "what a usable agent-chat TUI must do" as
  testable contracts, so the next adjacent gap always surfaced next.
- **No ownership contract.** The terminal draws chrome *and* the app draws its own
  UI. With no rule for who-owns-which-cell, they collided (top-row garble) and
  duplicated (two status lines).
- **The decisive diagnostic wasn't standard.** Capturing real app output and
  replaying it through the exact grid (`vtdump`) localized the garble in one shot
  — *after* many wrong hypotheses. It must be a standing step, not a last resort.

Legend: ✅ done · ⚠️ partial · ❌ missing · 🔧 fixed in this pass

## A — Message rendering (smedja-tui `main_panel.rs`)
- ❌→🔧 Long lines **wrap** at panel width (no horizontal clipping)
- ⚠️→🔧 Streaming deltas accumulate and stay visible (auto-follow bottom)
- ✅ Code fences / diff lines styled; wrapping must preserve styling
- ⚠️ Wide chars / emoji align (panel measures **display width**, not char count)

## B — Scroll
- 🔧 Auto-follow newest content while streaming; release on manual scroll-up;
  re-follow when scrolled back to bottom
- ⚠️ PgUp/PgDn/Home/End/wheel scroll by visual rows; `gg`/`G` jump
- ✅ Scroll clamps on resize

## C — Selection & clipboard
- ❌→🔧 **Mouse drag** selects text in messages; release copies to clipboard
- ✅ Keyboard visual mode (`v`/`y`) copies selection
- ✅ System clipboard via wl-copy/xclip/xsel; ⚠️ add OSC-52 fallback
- ⚠️ Document: Shift+drag falls through to the terminal's native selection when
  the app holds mouse capture

## D — Turn lifecycle UX
- ⚠️ Turn timeout surfaced with elapsed time + the configurable cap
  (`SMEDJA_TURN_TIMEOUT_S`, default 900 s); today it only says "try a shorter
  prompt"
- ❌ In-flight turn cancellable (Ctrl-C / Esc) without killing the app
- ✅ `thinking` / `tool_call` events visible

## E — Terminal ↔ app ownership contract (`term/bin/smedja`, `st-render`)
- ✅🔧 Terminal chrome (top bar, agent-block overlay) MUST NOT draw over a
  full-screen **alt-screen** app (smedja-tui / vim / less own every cell)
- ✅🔧 Single status bar; no duplicate/overlapping status lines
- ✅🔧 PTY winsize rows/cols exactly match the rendered grid (no phantom-row drift)

## F — Acceptance harness (the gate — this is what was missing)
- ❌ **Recorded-stream test**: feed a captured NDJSON turn (long paragraph + code
  + tool calls) into `MainPanel`; assert it wraps, the newest line is visible,
  and selection extracts the right text. Unit-testable, no GPU.
- ❌ **Layer-localization smoke** (`scripts/smoke-tui.sh`): spawn smedja-tui under
  a PTY, capture its output, replay through `vtdump`, assert the grid is clean
  (chrome excluded). This is the diagnostic that finally found the garble —
  standardize it.
- ⚠️ Per-release manual GPU pass: run smedja-tui (+ vim/less) in a fresh smedja,
  confirm no overlap/garble, wrapping, scroll-follow, mouse-copy.

## Definition of done
Every A–E item ✅ and the F harness green + wired into `scripts/test-all-layers.sh`,
so app-layer and chrome-layer regressions fail a test instead of a screenshot.

# Changelog

All notable changes to smedja are documented here.

Format: `## [version] — YYYY-MM-DD` / `### Added|Fixed|Changed|Removed|Roadmap`.

---

## [0.10.1] — 2026-06-22

### Fixed

- `smedja` GPU terminal: `FontSystem` now initialises with an empty `fontdb::Database` (< 5 ms); system fonts are loaded lazily on first glyph rasterisation, eliminating the ~10 s startup freeze.
- `smedja` GPU terminal: Glyphs are pre-warmed via `ensure_cell_glyphs()` before each render pass, replacing the previous behaviour where cells missing from the atlas were silently dropped. Atlas misses now emit a `warn!` trace line instead of discarding content.
- `smdjad`: socket path auto-derives from `$XDG_RUNTIME_DIR/smdjad.sock` (fallback `/tmp/smdjad.sock`). The `--sock` flag shown in earlier docs does not exist; the path is controlled via environment variable only.
- `smedja-tui`: connect banner now shows socket path, session ID, provider, and tier on startup (was an empty screen).
- `smedja-tui`: `turn.subscribe` replaces the old `task.get` poll loop; the TUI waits for `done=true` from the daemon before rendering the reply.
- `smedja-tui`: tier is read from the session response and reflected in the status bar.
- `st-agent` → `smdjad` bridge: `TurnStart` events now propagate `tier` and `model` into `SharedPaneState`; the GPU terminal status bar renders the active tier correctly.
- VTE (`st-pty`): cursor home (`CSI H`), delete line (`CSI M`), index (`ESC D`), reverse index (`ESC M`), OSC 0/2 (window title), alternate screen enter/exit, cursor hide/show, save/restore (`ESC 7`/`8`), 24-bit foreground colour, and 256-colour background are now handled and have conformance tests.

### Added

- `smedja-adapter`: `SmartCrusher` transform strips JSON nulls, zero-value arrays, and repeated keys from tool results before serialisation.
- `smedja-memory`: `inject_conciseness` appends a `<conciseness>` directive to the system prompt when context exceeds 60% of the window.
- `smedja-memory`: `seal_prefix()` marks a stable-prefix boundary below which turns are never reordered or discarded during compaction.
- `smedja-vault`: SQLite-backed vector store with cosine-similarity full-scan retrieval; `Vault::query` returns top-K results.
- `smedja-tui`: `/cowork on|off|status` slash command sends `cowork.set` to the daemon; approval prompts appear as text lines in the agent block.
- `smdjad`: `cowork.set`, `cowork.approve`, `cowork.deny`, and `cowork.modify` RPC methods implemented and tested (11 tests).
- Smoke test suite: 6 user-journey tests covering `daemon_unavailable`, session creation, unknown method, streaming deltas, and `turn.submit` + subscribe round-trip.
- VTE conformance tests: 12 `vte_*` tests in `st-pty` covering the sequences listed above.

### Changed

- README install section replaced: no release binaries exist yet; instructions now describe build-from-source workflow.
- README Context Budget Control: `CacheAligner` (BuildPrompt integration) correctly marked as roadmap; `SmartCrusher` and verbosity steering marked as working.
- README vault: `smedja_vault_search` tool noted as in-progress (returns empty results pending daemon wiring).
- README cowork gate: inline keyboard approval widget noted as roadmap.
- README GPU terminal: current rendering state described accurately.
- DELIVERY.md: feature matrix, test counts, and test pyramid updated to match codebase reality.

### Roadmap (not yet implemented)

- Release binary distribution and install script.
- `smedja_vault_search` daemon wiring (vault crate is ready; tool dispatch stub returns empty).
- CacheAligner / BuildPrompt stable-prefix hints to provider.
- Inline cowork approval widget in `smedja-tui` (keyboard `y`/`n`/`m`).
- Background image GPU blit in `st-render`.
- Glyph Protocol PUA registration.
- MCP OAuth redirect listener and token exchange.
- `smj session rollback` command.

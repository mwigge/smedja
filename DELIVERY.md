# Delivery

## Definition of Done

A task checkbox may only be set to `[x]` when **all** of the following are true:

1. A failing test exists that covers the behaviour (red phase).
2. The implementation makes that test pass (green phase).
3. `scripts/smoke-test.sh` exits 0.

If `scripts/smoke-test.sh` does not pass, do not check the box.

## Current Feature Status

| Feature | Dimension | Status |
|---------|-----------|--------|
| GPU terminal (`smedja`) | text rendering | working — system fonts load via `new_with_system_fonts`; glyphs rasterised lazily by `ensure_cell_glyphs()`; atlas miss emits warn but does not crash |
| GPU terminal (`smedja`) | startup time | non-blocking — `FontSystem` init < 5 ms; system font scan deferred to first glyph |
| GPU terminal (`smedja`) | background image blit | roadmap — pixels loaded into `background.image_pixels` but GPU blit not implemented |
| GPU terminal (`smedja`) | Glyph Protocol (PUA) | roadmap — protocol specified; PUA glyph registration not wired |
| smedja-tui | connects to daemon | yes |
| smedja-tui | turn submission + response | working — `turn.submit` queues a task; `turn.subscribe` polls until `done=true` |
| smedja-tui | connect banner | working — shows socket path, session ID, provider, tier on startup |
| smedja-tui | status bar tier | working — reads tier from session response; falls back to `"default"` |
| smedja-tui | cowork gate (slash command) | working — `/cowork on|off|status` sends `cowork.set`; `/cowork status` reads live from `session.get`; approval prompts shown as text |
| smedja-tui | cowork gate (inline widget) | roadmap — keyboard `y`/`n`/`m` approval widget not implemented |
| agent bridge (st-agent → smdjad) | wired | working — `spawn_agent_bridge` subscribes to pane events; tier and model propagated from `TurnStart` |
| smedja-vault | storage + cosine retrieval | working — SQLite BLOB store with full-scan cosine-similarity query |
| smedja-vault | `smedja_vault_search` tool (daemon) | in progress — tool registered in daemon; returns empty results pending wiring |
| smedja-vault | cold retrieval via SmartCrusher pipeline | roadmap |
| smedja-adapter | SmartCrusher | working — strips nulls, empty arrays, repeated keys; tested |
| smedja-memory | stable-prefix / CacheAligner | partial — `seal_prefix()` and mutable-window compaction work; adapter BuildPrompt integration roadmap |
| smedja-memory | verbosity steering | working — `inject_conciseness` appends directive at > 60% context fill; tested |
| OTel | gen_ai.* semantic conventions | partial — spans emitted for turns and tool calls; missing conversation.id, TTFT, tool call IDs |
| VTE | escape sequences handled | ~35 — cursor home, delete line, index, reverse index, OSC 0/2, alt screen, hide/show cursor, save/restore, 24-bit and 256-colour now handled |
| VTE | conformance tests | 12 vte_ tests in `st-pty` (cursor home, delete line, index, OSC, alt screen, colour) |
| TUI | functional tests | 22 tests in `smedja-tui` lib + 6 smoke tests |
| GPU terminal | headless tests | 1 smoke test (`st-render` non-blocking init) |
| MCP OAuth | redirect listener + token exchange | in progress — `start_pkce` stub returns `Cancelled`; HTTP listener not implemented |
| smedja-tui | daily quota panel (`/quota`) | working — reads `daily_tokens_used` + `daily_tokens_limit` from obs snapshot; bar shown in obs panel; limit set via `SMEDJA_DAILY_TOKEN_LIMIT` |
| smedja-tui | `/test` project detection | working — auto-detects Cargo / npm / Go / pytest manifests; monorepo disambiguation message; `pass /test cargo\|npm\|go\|py` to override |
| smedja-tui | LSP empty state | working — shows install hint when no servers registered |
| smedja-tui | obs panel independent poll | working — 3 s cadence independent of metrics overlay open state |
| smdjad | MCP stdio allowlist | working — shell metachar rejection + binary-on-PATH check in `McpStdioClient::spawn` |
| smdjad | `@shell` cowork gate warning | working — `tracing::warn!` when called without a cowork gate |
| smdjad | DeltaStore TTL (60 s) | working — delta buffers evicted 60 s after terminal event |
| smdjad | DB prune + vacuum (daily) | working — `prune_old_sessions(30)` + `VACUUM` in background task |
| smdjad | lifecycle mpsc channel | working — `TurnEvent::Started` sent on bounded mpsc(256) to worker, never dropped |
| smdjad | signal handler safety | working — SIGTERM/SIGHUP installed before `select!`, errors propagated with `?` |
| smedja-lsp | `LspManager::shutdown()` | working — aborts `run_all` JoinHandle, kills child LSP processes |

## Test Layers

```bash
# Unit and integration tests for all crates (754 passing, 7 ignored)
cargo test --workspace

# TUI smoke integration tests (6 passing)
cargo test -p smedja-tui --test smoke

# All layers via the gate script
./scripts/smoke-test.sh
```

## How to Check a Task Done

1. Write a test that fails without the feature (`cargo test` goes red).
2. Implement the feature until `cargo test` goes green.
3. Run `./scripts/smoke-test.sh` from the repo root — it must exit 0.
4. Set the checkbox to `[x]` in the relevant `tasks.md`.

Do not skip step 3. A passing unit test without a passing smoke run is not done.

## Test Pyramid

| Layer | What | Count | How to Run | CI Default |
|-------|------|-------|-----------|-----------|
| 1 — Unit (st-pty) | CellGrid resize, scroll, VTE sequences | 39 | `cargo test -p st-pty` | Yes |
| 2 — VTE conformance | Cursor home, delete line, index, OSC, alt screen, colour | 12 | `cargo test -p st-pty vte_` | Yes |
| 3 — GPU smoke (st-render) | Non-blocking FontSystem init, renderer state | 1 | `cargo test -p st-render` | Yes |
| 4 — TUI functional | TestBackend render + state checks | 22 | `cargo test -p smedja-tui` | Yes |
| 5 — User journey | MockDaemon + client protocol | 6 | `cargo test -p smedja-tui --test smoke` | Yes |
| 6 — Workspace unit | All other crates (adapter, ingot, memory, assayer, …) | 674 | `cargo test --workspace` | Yes |

### Definition of Done

A task is complete only when:
1. Test written (red phase — test fails or is `#[ignore]` pending feature)
2. Implementation makes it pass (green phase)
3. `scripts/smoke-test.sh` passes
4. Checkbox set to `[x]` in tasks.md

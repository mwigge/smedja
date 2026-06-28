# Changelog

All notable changes to smedja are documented here.

Format: `## [version] — YYYY-MM-DD` / `### Added|Fixed|Changed|Removed|Roadmap`.

---

## [0.20.5] — 2026-06-28

### Fixed

- **install.sh quickstart message** — Linux installs with systemd now show `quickstart: smedja  (smdjad starts automatically via systemd --user)` instead of the misleading `smdjad & smedja`; on upgrade, the running daemon is restarted automatically via `systemctl --user restart`.

---

## [0.20.4] — 2026-06-28

### Fixed

- **Stream stall false positive** — raised `STREAM_TIMEOUT_SECS` from 90 s to 600 s in `stream_server.rs`; eliminates the `[ERROR] stream stalled: no events for 90s` error that fired mid-response when using slow providers (codex-cli + claude-haiku).

---

## [0.16.3] — 2026-06-26

### Added

- **Ctrl-F context rail** — context rail remapped from `Ctrl-R` to `Ctrl-F` (scroll mode); `Ctrl-R` is now exclusively reverse history search (input mode), eliminating the double-binding ambiguity.
- **Ctrl-G external editor** — press `Ctrl-G` in input mode to open `$VISUAL` / `$EDITOR` / `vi` for multi-line message composition. The TUI suspends, the editor runs, and the file contents are loaded back into the input buffer on exit.
- **ThinkingDelta TurnEvent** — new `TurnEvent::ThinkingDelta { content, turn_id, correlation }` variant in `smedja-bellows`; stream_server forwards it as `{"type":"thinking","text":"..."}` NDJSON. The TUI accumulates thinking tokens into `current_thinking`, shows a live 50-char preview alongside the spinner, and on completion emits a `╌ thinking (N chars) [T to expand] ╌` badge. Press `T` (shift-T) in scroll mode to expand/collapse the full thinking block.
- **OSC-9 turn-complete notification** — emits `\e]9;turn complete\x07` on every `"done"` NDJSON event; picked up by Windows Terminal, iTerm2, and any OSC-9-capable terminal as a desktop notification.
- **Prompt feedback indicator** — right-aligned `{N}c ≈{M}tok` character and estimated token count displayed in the input line while composing. Disappears when input is empty.
- **Emit/canvas split** — single-line system messages are now also routed to the action log (the "emit" rail), styled as `sys` entries in cyan, implementing the SuperConsole dual-display pattern.
- **`/loop` TUI command** — `/loop status`, `/loop list`, `/loop create <goal>`, `/loop cancel` manage loop runs via `loop.*` RPC methods.
- **Session browser left-rail** — `Ctrl-W` toggles a 28-col session list panel on the left. The list refreshes every 5 s from `session.list`. Navigate with `[` / `]` in scroll mode.
- **Mouse scroll support** — `EnableMouseCapture` is now active; `MouseEventKind::ScrollDown/Up` scroll the main panel. GPU terminal click-selection is suspended while the TUI is focused.
- **`/gov` govctl harness** — reads TOML artifacts from `gov/work-items/`, `gov/rfc/`, `gov/adr/`. `/gov list` shows all artifacts with id/kind/status/title. `/gov show <id>` prints full detail.

### Fixed

- **`lsp_panel.rs` clippy** — removed dead-code `if/else` producing identical "lsp" blocks; `block` now hardcodes the title directly.
- **`main.rs` clippy** — six `map_or(false, ...)` → `is_ok_and(...)`, one `map_or(true, ...)` → `is_none_or(...)`, redundant `else if` branches with identical bodies collapsed, `len() > 0` → `!is_empty()`, `filter_map(|b| Some(...))` → `map`.

## [0.16.2] — 2026-06-26

### Fixed

- **`spawn_worker` contention** — removed `Arc<Mutex<JoinSet<()>>>` from the turn-dispatch hot path. `spawn_worker` now owns its `JoinSet` exclusively (no mutex, no Arc), eliminating lock contention that serialised every concurrent turn spawn. The function returns `JoinHandle<JoinSet<()>>`; at shutdown the channel is closed by dropping a retained `work_tx` clone, the handle is awaited to recover any remaining in-flight turns, and both the turn set and the loop `task_set` are drained together under the existing 30 s deadline. Loop tasks (`loop.run`) continue to use the separate `task_set` in `HandlerState` unchanged.

## [0.16.1] — 2026-06-26

### Fixed

- **Test coverage: MCP stdio allowlist** — three unit tests now assert that `McpStdioClient::spawn` returns an error for commands containing shell metacharacters (`;`, `|`, `` ` ``, `$`, `>`, `&`), for relative binary names not on PATH, and for absolute paths that do not exist. A regression in the injection guard can no longer ship silently.
- **Test coverage: DB prune + vacuum** — three unit tests on an in-memory SQLite fixture verify `prune_old_sessions` cascades through `tasks` and `audit_events`, preserves recent complete sessions, and never touches active sessions regardless of age. `vacuum()` is also exercised.
- **Test coverage: `lsp_snapshot_from_rpc`** — four unit tests cover all four severity strings (`error`/`warning`/`info`/`hint`), unknown severity defaulting to `Error`, all three server states (`ready`/`degraded: <reason>`/`starting`), and empty inputs.
- **Test coverage: `detect_project_types`** — extracted as a pure function; three unit tests cover Cargo-only, multi-manifest, and empty-directory workspaces.
- **Test coverage: DeltaStore TTL** — a `start_paused = true` tokio test advances the clock past `DELTA_TTL_SECS` and asserts the buffer entry is evicted. Requires `tokio/test-util` added to smdjad dev-dependencies.
- **Test coverage: poll backoff** — two unit tests verify the clamped shift expression never overflows for any retry count 0–60 and caps at 1 000 ms for retry ≥ 5.
- **Bug: poll backoff shift overflow** — `100u64 << poll_retry_count.saturating_sub(1)` could overflow in debug builds at `poll_retry_count ≥ 64`; fixed to `let shift = count.saturating_sub(1).min(10); 100u64 << shift` — overflows impossible, cap unchanged.

## [0.16.0] — 2026-06-25

### Added

- **Daily token quota panel** — `SMEDJA_DAILY_TOKEN_LIMIT` env var exposes a `quota.limit` RPC; the TUI obs panel shows a live daily-usage bar (`used / limit`) populated from `obs_snapshot.daily_tokens_used` + `daily_tokens_limit` polled every 3 s independently of the metrics overlay.
- **LSP empty state** — the LSP panel now renders an install hint (`no servers found — install rust-analyzer, gopls, or pyright`) when no language servers are registered, instead of a blank pane.
- **`/test` project-type detection** — TUI `/test` auto-detects Cargo / npm / Go / pytest manifests in the session workspace; if multiple are found a disambiguation message is emitted; `pass /test cargo|npm|go|py` to override.
- **`/cowork status` live** — reads `cowork_mode` from the `session.get` RPC response (was previously missing from the payload) so `/cowork status` reflects the real daemon state.
- **`/quota` live** — shows real `daily_tokens_used` and `daily_tokens_limit` from the obs snapshot instead of placeholder zeros.
- **DB prune + vacuum** — `prune_old_sessions(30)` deletes terminal sessions older than 30 days (cascades through `cost_ledger`, `audit_events`, `tasks`), then `PRAGMA wal_checkpoint(TRUNCATE); VACUUM;` runs once daily in a background task, keeping the SQLite file bounded.
- **DeltaStore TTL** — streaming delta buffers auto-evict 60 s after a turn reaches a terminal state, so late subscribers can still replay but memory is reclaimed in steady state.
- **Dedicated mpsc lifecycle channel** — `turn.submit` also sends on a bounded `mpsc::channel(256)` so `TurnEvent::Started` can never be dropped even under broadcast burst; `spawn_worker` reads exclusively from this channel.
- **`route!` macro** — eliminates the four-line handler-registration boilerplate; all 50+ routes now use `route!(router, "method.name", state, handlers::module::fn)`.
- **`lsp_snapshot_from_rpc()`** — converts `lsp.status` + `lsp.diagnostics` RPC responses to `LspSnapshot`; severity decoded from strings (`"error"/"warning"/"info"/"hint"`), state decoded from `"starting"/"ready"/"degraded: <reason>"`.
- **`LspManager::shutdown()`** — aborts the `run_all` JoinHandle, killing child LSP processes (guarded by `kill_on_drop(true)`).
- **Signal handler safety** — `SIGTERM`/`SIGHUP` handlers installed before `tokio::select!` and errors propagated with `?` rather than `.unwrap()`, preventing panics under FD exhaustion.
- **`exec_bash` 30 s timeout** — shell fragments time out at 30 s, preventing hung turns.
- **DB file permissions 0600** — SQLite file opened with `0o600` permissions on creation.
- **PID file in XDG_RUNTIME_DIR** — PID file placed in `$XDG_RUNTIME_DIR` or `~/.cache`, never in `/tmp`.
- **Obs poll independent of metrics overlay** — `session.cost` and `obs_snapshot` fields polled on a dedicated 3 s cadence regardless of whether the metrics panel is open.
- **MCP refresh background** — MCP server list refresh moved to a `tokio::spawn` so it never blocks the router.
- **Dispatcher capacity 1024** — broadcast channel capacity raised from 256 to 1024.

### Fixed

- **MCP stdio allowlist** — `McpStdioClient::spawn` now rejects commands containing shell metacharacters (`; & | \` $ > < ( ) { } \n \r`) and requires the binary to be on PATH (or an absolute path that exists), closing a command-injection vector.
- **`@shell` cowork gate warning** — `resolve_shell()` emits `tracing::warn!` when called without a cowork gate, making unapproved shell execution auditable.
- **Duplicate route detection** — `Router::register` now fires `debug_assert!(prev.is_none(), "duplicate route: {method_str}")` so double-registrations fail fast in debug builds.
- **`metrics.summary` field names** — TUI metrics poll was reading `resp["rows"]` and `tokens_total`; corrected to `resp["buckets"]` with `input_tok`/`output_tok` fields matching the actual RPC schema.
- **LSP severity type mismatch** — `lsp_snapshot_from_rpc` was calling `Severity::from_lsp(d["severity"].as_u64())` but the daemon emits string severities; fixed to match on `d["severity"].as_str()`.
- **LSP degraded state format** — TUI matched `"degraded"` literally; daemon sends `"degraded: <reason>"`; fixed with `strip_prefix("degraded: ")`.
- **Mutex poison recovery** — `loop_runner.rs` test harness uses `unwrap_or_else(|e| e.into_inner())` to survive poisoned locks.
- **`session.cowork_mode` in `session.get` response** — the field was absent from the handler's JSON output; added so `/cowork status` can read it.

## [0.14.0] — 2026-06-25

### Added

- Token economy: an attributed, multi-source tokens-saved ledger (`filter`/`crusher`/`cold-context`/`cache`), a savings rollup with an efficiency ratio, surfaced as an always-on `st-statusbar` efficiency segment, in the TUI metrics panel, and via `smj savings`. Provider cache reads count as savings, kept distinct from compression.
- Lean specs: an umbrella + slice model — the umbrella holds the durable thought-trail with detail chunked into the vault; thin slices link to it by pointer; loading is hybrid (intent in the KV-cached prefix, detail recalled per slice via cold-context); savings self-measured as `source=lean-spec`.
- Pluggable vault embedder: an `Embedder` port (FNV the offline default, a local `/v1/embeddings` learned backend) with per-row model/dim versioning, same-model-only comparison, and a `vault.reembed` backfill.
- Command-aware output filtering: `bash`/`run_command` output compressed by command-keyed strategies, configurable via `.smedja/filters.toml`, with the full output teed to the vault for recovery and savings recorded.
- Local-model UX: inventory, GPU probe, hot-swap, and install orchestration via `local.*` RPCs and `smj local`.
- Time-tiered local metrics rollups and a live TUI metrics panel (`Ctrl-T`).
- Sandbox read + network confinement: a tightened Landlock read allow-list so host secrets are unreadable, and best-effort network-namespace confinement (degrades to filesystem-confined host network where namespaces are unavailable).
- TUI: `@file`/`@git`/`@branch`/`@shell` context-fragment expansion; session resume via `--session`/`/resume`; an inline `y`/`n`/`m` cowork approval that honours the daemon's resolved contract.
- Terminal: Glyph Protocol PUA registration wired end to end (RGBA colour atlas).
- Cross-provider cache hints (OpenAI/Gemini) plus cross-turn cache-aligner persistence keyed by `(session, runner)`.

### Changed

- Methodology is now foundational: TDD + clean are always-on, steer-first (a directive in the sealed system prefix), default-on with a `.smedja/config.toml` escape; the diff gate is a sane backstop (TDD advisory, only `clean` hard-blocks).
- CI runs once per PR and once per merge to `main` (no duplicate push+PR runs) and cancels superseded runs.
- `smedja-agent-events` wire schema bumped `1 → 2` (backward-compatible) to carry a cumulative efficiency figure on `TurnEnd`.

### Removed

- The `/tdd` and `/ponytail` TUI commands and the selectable `tdd`/`ponytail`/`sre` methodology modes; ponytail ships as a review skill instead.

## [0.13.0] — 2026-06-24

### Added

- `smdjad`: automatic cold-stratum recall. Each turn the orchestrator recalls semantically-relevant context from the vault through the `ColdStore` port and injects it as a single budget-capped `<cold_context>` system block inside the sealed prefix. Top-K is per tier (`cold_k_for_tier`) and the block is capped to a fraction of the tier budget; admission is highest-relevance-first with a minimum-score floor. The `smedja.turn.cold_results_injected` span attribute records how many entries were admitted.
- `smdjad`: provider failover. When a routed provider becomes unusable mid-turn (rate-limited beyond back-off, quota exhausted, context-window exceeded, or down), routing rotates to the next eligible runner of a compatible tier. Rotation walks a bounded ring (routed provider first, then compatible alternatives in pool-priority order, default last; at most three rotations per turn), never degrades below the routed tier, and preserves the assembled prompt and accumulated tool history. Each rotation is visible through the `smedja.error.kind` / `smedja.error.retryable` span attributes.
- `smj security {scan,report,sbom}` + `smedja-security`: a proportionate, advisory-by-default security plane. `scan` runs a workspace posture scan and prints findings without blocking; `report` summarises recorded `security_finding` audit events as a read-only query; `sbom` emits a CycloneDX-style SBOM from the resolved `Cargo.lock`.
- `smdjad`/`smj audit run`: read-only repo/PR/branch auditor. The `audit.run` RPC runs the Review role over a selected scope (working-tree diff, whole repo/path, `--branch <base>...HEAD`, or `--pr`) with only `graph_query`/`read_file`/`list_files`, aggregates structured `AuditFinding`s, persists them as `smedja-ingot` audit events, and renders a deterministic markdown report. The loop is read-only by two independent guarantees: the read-only tool allowlist and `review`-mode `role_allows_write_bash` denial.
- `smdjad`/`smj mcp`: MCP server mode. smedja is co-mounted as an MCP server on the authenticated ACP HTTP listener (`/mcp`, JSON-RPC 2.0), dispatching `tools/call` into the executor under an effective `review`-mode (read-only) session. OAuth 2.0 Authorization Code + PKCE (S256) flow with loopback redirect, `state` validation, token exchange, and refresh-token grant. A newline-framed JSON-RPC 2.0 stdio transport spawns and reuses a registered MCP server as a child process. ACP turn events are forwarded over SSE. `smj mcp {add,list,remove,refresh}` manages the server registry.
- `smdjad`/`smj sandbox`: cross-platform tool sandbox. Shell tools run inside a per-platform isolation boundary selected by capability detection — Docker (opt-in), macOS Seatbelt, or Linux Landlock. Network policy is governed by `SMEDJA_SANDBOX_NETWORK` (`none`/`allowlist`/`open`) over an SSRF floor; fallback behaviour by `SMEDJA_SANDBOX_MODE` (`auto`/`required`/`off`). Each execution emits a `smedja.sandbox.exec` span; `smj sandbox status` reports the selected backend, network policy, and mode, and `smj sandbox build` builds the Docker image.
- `smedja-eval` + `smj eval run` + `cargo xtask eval`: an eval harness. Loads a case suite directory, runs it through the eval engine, prints a report, and gates on a pass-rate threshold; `cargo xtask eval` runs the offline gate in CI. An `evals/` corpus ships in-repo.

## [0.10.1] — 2026-06-22

### Fixed

- `smedja` GPU terminal: `FontSystem` now initialises with an empty `fontdb::Database` (< 5 ms); system fonts are loaded lazily on first glyph rasterisation, eliminating the ~10 s startup freeze.
- `smedja` GPU terminal: Glyphs are pre-warmed via `ensure_cell_glyphs()` before each render pass, replacing the previous behaviour where cells missing from the atlas were silently dropped. Atlas misses now emit a `warn!` trace line instead of discarding content.
- `smdjad`: socket path auto-derives from `$XDG_RUNTIME_DIR/smdjad.sock` (fallback `/tmp/smdjad.sock`). The path can be overridden with the `--sock` flag (a `clap` argument on both `smj` and `smedja-tui`) or the `SMEDJA_SOCK` environment variable; an earlier changelog note that `--sock` "does not exist" was incorrect.
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
- `smdjad`: `loop.run` now drives the real `smedja-loop` engine — it loads `.smedja/loop.json`, verifies the policy SHA-256 at load (aborting in `policy_tampered` if the file changes mid-run), routes roles by configured runner/tier, runs the deterministic verification gate per slice, and applies bounded fix retries.
- `smdjad`/`smedja-memory`: stable-prefix KV-cache hints are wired to the live turn path. The orchestrator seals the prefix with `seal_prefix()` and passes `stable_prefix_len` (derived from `mem.stable_prefix()`) to the Anthropic runner, where the adapter applies the cache hints.
- `smdjad`: `session.rollback` RPC and the `smj session rollback <id> <turn>` CLI reconstruct any point in session history from the structured compaction format.
- `smdjad`: readiness signalling — `sd_notify(READY=1)` notifies `Type=notify` systemd units on startup, and an unauthenticated `/health` probe returns `200` once the daemon is serving.
- Smoke test suite: 6 user-journey tests covering `daemon_unavailable`, session creation, unknown method, streaming deltas, and `turn.submit` + subscribe round-trip.
- VTE conformance tests: 12 `vte_*` tests in `st-pty` covering the sequences listed above.

### Changed

- README install section replaced: no release binaries exist yet; instructions now describe build-from-source workflow.
- README Context Budget Control: stable-prefix KV-cache hints described as delivered on the Anthropic live path; `SmartCrusher` and verbosity steering marked as working. Cross-provider cache alignment remains roadmap.
- README vault: `smedja_vault_search` tool described as available — it embeds the query, runs hybrid cosine + keyword + recency search over the named namespace, and returns ranked results.
- README cowork gate: inline keyboard approval widget noted as roadmap.
- README GPU terminal: current rendering state described accurately.
- DELIVERY.md: feature matrix, test counts, and test pyramid updated to match codebase reality.

### Roadmap (not yet implemented)

- Inline cowork approval widget in `smedja-tui` (keyboard `y`/`n`/`m`).
- Glyph Protocol PUA glyph registration (the protocol is specified; PUA codepoint wiring is not built).
- Background image GPU blit in `st-render`.
- Session-resume TUI `--session` picker.
- Cross-provider `CacheAligner` (beyond Anthropic stable-prefix hints, which ship).
- Metrics rollup dashboard.
- Local-model install/swap UX.
- `@file` / `@git` / `@shell` context fragments.

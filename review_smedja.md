# smedja v0.17.0 — Full Review

**Date:** 2026-06-26  
**Scope:** Security · SRE/Reliability · Product/UX · Code Quality  
**Result:** Security clean. 50 findings across three dimensions.

---

## Security

No findings at the required confidence threshold.

Defence-in-depth verified across: Unix socket (0o600 + 4 MiB frame cap), ACP auth (constant-time comparison, UUID v4 token at 0o600), bash sandbox (`SandboxMode::Required` fails closed), file tools (`canonicalize()` + `starts_with()` path guard), loop policy (SHA-256 tamper detection before verification runs), MCP OAuth (PKCE from 256-bit entropy), SSRF guard (`is_blocked_ip()` covers all RFC-1918/loopback/ULA ranges).

---

## SRE / Reliability

### Critical

**R-1 Unbounded tool-output cache causes OOM**  
File: `bin/smdjad/src/executor/mod.rs:104`  
`retrieve_store()` returns a `&'static Mutex<HashMap<Sha256, String>>` that accumulates every filtered command output and is never evicted. A daemon session running `cargo test` or `git log --all` repeatedly fills this map without bound until the kernel OOM-kills the process.  
Fix: Replace with an LRU bounded by 512 entries or 64 MB. Add a `smedja.executor.cache_bytes` gauge.

**R-2 MCP HTTP tool calls have no timeout**  
File: `bin/smdjad/src/mcp_http.rs:34`  
`reqwest::Client::new()` with no `timeout` or `connect_timeout`. A stalled MCP server holds the tool loop `.await` forever; with N tool calls the effective ceiling is N × 5 minutes before the stream-drain guard fires.  
Fix: Build client with `.timeout(Duration::from_secs(30)).connect_timeout(Duration::from_secs(5))`. Wrap each `send().await` in `tokio::time::timeout(TOOL_CALL_DEADLINE)`.

### High

**R-3 33rd concurrent stream connection silently dropped with no error to client**  
File: `bin/smdjad/src/stream_server.rs:205`  
`Semaphore::new(32)` exhaustion closes the socket without writing a response; TUI sees EOF and falls back to polling, missing all incremental events silently.  
Fix: Write `{"type":"error","code":"at_capacity"}` before closing. Or make acquire async to queue rather than drop.

**R-4 Per-turn stream timeout resets on every tool call — effective budget is N × 5 minutes**  
File: `bin/smdjad/src/orchestrator/mod.rs:748`  
`tokio::time::timeout(300s)` is entered freshly inside the tool loop. 20 tool calls = 100-minute ceiling with no per-turn wall-clock budget.  
Fix: Capture `Instant::now()` at turn start; wrap the entire tool loop in `timeout_at(deadline)`. Default: `agent_timeout_s` from `loop.json` or 15 minutes hard cap.

**R-5 TUI has no reconnect path after smdjad restart**  
File: `bin/smedja-tui/src/main.rs:4077`  
After smdjad restarts the TUI's `Client` holds a dead fd. All subsequent `client.call()` return `BrokenPipe`. No health-check task, no reconnect loop, no user-visible indicator.  
Fix: Background health-check task pings `system.ping` every 5 s; enter "reconnecting" UI state on failure; reconnect with exponential backoff (cap 30 s); re-subscribe to stream on reconnect.

**R-6 Agent timeout fires but underlying LLM stream task keeps running**  
File: `bin/smdjad/src/loop_runner.rs:141`  
`tokio::time::timeout` drops the future but the inner `tokio::spawn`'d streaming task is not cancelled; it continues consuming tokens and writing to an abandoned channel.  
Fix: Store `JoinHandle` and call `handle.abort()` in the timeout arm. Or thread a `CancellationToken` through the streaming path.

**R-7 `prune_old_sessions` runs five DELETEs without a wrapping transaction**  
File: `crates/smedja-ingot/src/lib.rs:648`  
Five auto-committing DELETEs; a crash mid-prune leaves orphaned child rows that accumulate across restarts. Future prune passes may miss them depending on WHERE clauses.  
Fix: Wrap all five DELETEs in a single `BEGIN`/`COMMIT`. Better: add `ON DELETE CASCADE` to child-table foreign keys and replace with one `DELETE FROM sessions WHERE …`.

### Medium

**R-8 Single SQLite connection serialises all async callers under concurrent turns**  
File: `crates/smedja-ingot/src/handle.rs:33`  
One `Arc<Mutex<Ingot>>` over one connection. Four concurrent `flush_message` calls queue on the mutex; P99 write latency spikes to 4–20 ms visible as TUI stutter.  
Fix: Use a connection pool (`deadpool-sqlite`, 4–8 connections in WAL mode) or a single-writer actor task.

**R-9 LSP server permanently `Degraded` after 3 restarts — no recovery without daemon restart**  
File: `crates/smedja-lsp/src/manager.rs:187`  
After `MAX_RESTART_ATTEMPTS` the per-server task exits; no periodic probe, no `lsp.restart` RPC.  
Fix: Schedule 5-minute retry probe after cap exhausted. Expose `lsp.restart` RPC.

**R-10 SRE observability query functions have no HTTP timeout**  
File: `crates/smedja-sre/src/metrics.rs:36`  
`otel_query`, `metric_query`, `log_tail` all call `.send().await` with no timeout; overloaded backends block indefinitely.  
Fix: Set `.timeout(Duration::from_secs(15))` on the client. Add retry with exponential backoff (max 3 attempts).

**R-11 Delta buffer silently drops oldest events when a turn exceeds 2048 events**  
File: `bin/smdjad/src/stream_server.rs:101`  
`DeltaBuffer::push` evicts oldest events with no notification. A reconnecting TUI silently receives a partial turn.  
Fix: Emit `{"type":"buffer_overflow","lost":N}` synthetic event at head of replay. Raise cap to 8192 or make configurable.

**R-12 Shutdown drain does not wait for stream server connections**  
File: `bin/smdjad/src/main.rs:1169`  
Turn tasks are drained (30 s) but stream server connection tasks are plain `tokio::spawn` — not tracked. Process exits before the final `done` event flushes; TUI sees abrupt EOF.  
Fix: Move stream connection tasks into a tracked `JoinSet`; include in shutdown drain with a 5-second deadline after turn set is drained.

**R-13 Migration DDL failures silently swallowed; `schema_migrations` records success on partial failure**  
File: `crates/smedja-ingot/src/lib.rs:454`  
`let _ = self.conn.execute_batch(sql)` discards the error. A crash mid-migration marks it applied; on restart the partial schema is silently skipped, leaving missing columns that cause runtime panics.  
Fix: Remove `let _ =`; propagate `?`. Wrap DDL and `schema_migrations` INSERT in a single transaction.

**R-14 `provider_sessions` HashMap never evicted**  
File: `bin/smdjad/src/main.rs:950`  
One entry per session that ever submitted a turn, never removed. Each `ProviderSession` holds a live HTTP connection pool; 10,000 sessions ≈ 10 MB heap + open HTTPS sockets.  
Fix: Add last-used timestamp; GC task every 5 minutes evicts entries idle > 30 minutes.

### Low

**R-15 `smedja.llm.ttft_ms` histogram defined but never populated**  
File: `crates/smedja-telemetry/src/lib.rs:82`  
The time-to-first-token metric is registered but no call site records it. Operators see an empty histogram and cannot distinguish provider from local regressions.  
Fix: Capture `Instant::now()` before LLM stream; record on first chunk with provider + model tags.

**R-16 OTLP export failure logs `warn!` and continues silently**  
File: `bin/smdjad/src/main.rs:749`  
Misconfigured endpoint produces a `warn!` then a no-op TracerProvider. No metric, no alert, no TUI indication.  
Fix: Log at ERROR level. Emit a `smedja.telemetry.otlp_disabled` counter so it appears in log-based monitors.

**R-17 `TerminalGuard` instantiated after terminal setup — panics during setup leave raw mode active**  
File: `bin/smedja-tui/src/main.rs:3999`  
If any code between `enable_raw_mode()` and `TerminalGuard` construction panics, the terminal is stuck in raw mode.  
Fix: Construct `TerminalGuard` immediately after `enable_raw_mode()` succeeds, before any subsequent init that can panic.

**R-18 No per-tool-call latency or error metrics**  
File: `bin/smdjad/src/orchestrator/mod.rs:727`  
Tool regressions are invisible in dashboards until turn-level P99 rises.  
Fix: Record `smedja.tool.calls_total{tool, status}` counter and per-tool latency histogram. Use `gen_ai.tool.name` semantic convention.

**R-19 `smj` gives the same error for "daemon not running" and "permission denied"**  
Fix: Match on `io::ErrorKind::NotFound` vs `PermissionDenied` and print distinct actionable messages.

---

## Product / UX

### High

**P-1 No first-run experience when smdjad is not running**  
Raw context-chain error with no detection of "not installed" vs "not started". Extremely high new-user drop-off.  
Fix: Connectivity-check helper that detects socket-not-found vs permission-denied vs binary-not-on-PATH and prints a tiered actionable message.

**P-2 Cowork y/n/m shortcuts documented as working but wiring unconfirmed**  
`docs/tui.md` says "Press `y`/`n`/`m`". The widget and state are wired but keyboard routing in the main event loop may not be hooked when `!state.pending_cowork.is_empty()`. `/approve <id>` RPC works; keyboard shortcut path needs verification.  
Fix: Verify `handle_key` routes `y`/`n`/`m` to cowork RPCs when cowork prompt is visible, or update docs to remove keyboard claim until it's live.

**P-3 `smj loop run` returns immediately with no progress feedback**  
Prints "Loop {id} running" and exits. No slice output, no verification pass/fail, no state machine events. Multi-hour loops are unmonitorable.  
Fix: Stream NDJSON progress events until terminal state. `--follow` on by default, `--no-follow` for fire-and-forget. Document state machine states in `docs/smj.md`.

**P-4 API key / provider setup not in getting-started; silent turn failures for new users**  
`docs/getting-started.md` never mentions `ANTHROPIC_API_KEY`. Daemon logs `DEGRADED` at error level but this is invisible to TUI users. First message silently fails.  
Fix: Add "Step 2.5: Configure a provider" to getting-started. Add `/status` or `/provider` TUI command showing active provider or "no provider" warning. Surface degraded-state message in TUI startup.

**P-5 `smj session blocks` prints an internal TODO and returns exit 0**  
File: `bin/smj/src/main.rs:846`  
Documented as a real command; implementation is a stub printing `"requires smdjad 'session.blocks' RPC (not yet wired)"`. The RPC is not registered in the daemon.  
Fix: Implement the RPC or remove the command from CLI and docs. Stub must exit non-zero.

**P-6 LLM failure errors not surfaced distinctly in the TUI**  
Turn errors appear as undifferentiated text. No colour coding, no classification (rate-limit / auth / quota / network), no retry keybinding, no actionable suggestion.  
Fix: Classify error types, render with `palette().error` colour, add actionable hints per category. Add `r` keybinding in scroll mode to retry last turn.

### Medium

**P-7 `/cowork` not in `SLASH_COMPLETIONS` or `HELP_TEXT`**  
Documented in tui.md; typing `/cowork` produces no autocomplete and the command is silently swallowed.  
Fix: Add `/cowork` to completions and help. Implement `/cowork on|off` handler calling `cowork.set`.

**P-8 Session picker `updated_at` column always blank**  
`format_resume_rows()` calls `.and_then(Value::as_str)` on a JSON number; `as_str()` returns `None` on numbers. Every session shows an empty timestamp.  
Fix: Fall through to `as_f64()` and format as relative time ("2 hours ago") or ensure `session.list` serialises as ISO-8601 string.

**P-9 `smj cost` requires `--session` but documents it as optional; `--since` accepted but unused**  
No `--session` prints a non-error "required" message and exits 0. `--since` is destructured with `..`.  
Fix: Implement all-sessions cost summary mode for `--session`-less invocation. Wire `--since` filtering. Align README and smj.md on subcommand path.

**P-10 `SMEDJA_TIMELINE_URL` and `SMEDJA_LOG_FORMAT` undocumented**  
Both env vars are used in smjad/smj but absent from all env-var tables in docs.  
Fix: Add to `docs/configuration.md` and `docs/smj.md` with examples.

**P-11 LSP and Obs panels documented as default-on but `PanelVisibility` defaults all fields to `false`**  
File: `bin/smedja-tui/src/main.rs` (`PanelVisibility`)  
`docs/tui.md` shows LSP and Obs as "On" in the default column. Both start hidden.  
Fix: Set `panels: PanelVisibility { lsp: true, obs: true, ..Default::default() }` in AppState init, or update docs to show "Off".

**P-12 govctl has no `smj gov` CLI counterpart**  
All `/gov` commands are TUI-only. Cannot manage WIs from CI, git hooks, or non-interactive shells.  
Fix: Add `Cmd::Gov` to `smj` with `list`, `create`, `transition` subcommands. No daemon RPC needed — govctl operates on local TOML files.

**P-13 `@shell` cowork gate claim is unclear / unimplemented**  
`docs/tui.md` says `@shell` pauses for cowork approval. Fragment expansion runs client-side before `turn.submit`; no async pause mechanism exists for fragments. May be a security expectation gap.  
Fix: Clarify whether cowork gates `@shell` expansion. If not implemented, remove the qualifier from docs.

**P-14 No diff-before-apply preview outside cowork mode**  
Outside cowork, `edit_file` diffs appear only after the file is already written. No lightweight pre-apply review.  
Fix: Add `[tools] confirm_edits = true` to workspace config that activates a TUI diff preview with `[y] apply / [n] skip` without full cowork overhead.

**P-15 No conversation export to readable format**  
`smj session export` emits raw JSON cost lineage, not readable conversation history.  
Fix: Add `smj session export --format md|txt` that renders user/assistant turns as markdown. Add `/export` TUI slash command.

**P-16 `smj workspace agents init` template does not match documented `agents.toml` format**  
Generated template uses `bash = ["read", "write"]` fields; daemon parser expects `runner`, `tier`, `model`, `tools`. The file looks plausible but produces no routing effect.  
Fix: Update `AGENTS_TOML_TEMPLATE` to match the documented + parsed format. Add a parse round-trip test against `smedja_assayer::load_rules`.

### Low

**P-17 Panel search (`/` in scroll mode) not in `HELP_TEXT`**  
The capability exists; it's invisible to users relying on in-TUI help.  
Fix: Add `/  (scroll mode)  — search panel` to `HELP_TEXT`.

**P-18 `smj term convert-wezterm` documented in terminal.md but not implemented**  
`TermCmd` has no `ConvertWezterm` variant. Running `smj term` shows only `install`.  
Fix: Implement the stub (exit with "not yet implemented") or remove from docs.

**P-19 `/session` TUI command absent from `SLASH_COMPLETIONS` and `HELP_TEXT`**  
Documented in tui.md; not discoverable via autocomplete.  
Fix: Add to completions and help; verify or implement handler.

**P-20 `/gov transition` accepts invalid statuses silently**  
No validation against kind-specific valid status sets; an invalid status is written to the TOML file.  
Fix: Validate status against the kind's valid set; error with "valid: planned | in_progress | done | cancelled".

---

## Code Quality

### High

**Q-1 Eight RPC handler files have zero unit tests**  
Files: `handlers/session.rs` (685 lines, 11 endpoints), `checkpoint.rs`, `task.rs` (305 lines, 6 endpoints), `loops.rs`, `audit.rs`, `routing.rs`, `graph.rs`, `lsp.rs`  
The heaviest, most business-critical handlers — `session.fork`, `session.rollback`, `task.parallel`, `loop.create/run`, `cowork.approve/deny` — are entirely untested. The pattern exists in `handlers/savings.rs`.  
Fix: Add `#[tokio::test]` tests using `Ingot::open_in_memory()`. Start with `session.fork` (mutation-risk), `session.rollback` (destructive), `task.parallel` (coordination-risk).

**Q-2 `smedja-tui/src/main.rs` is 8,006 lines with two mega-functions**  
`handle_key`: lines 2427–3397 (970 lines). `dispatch_slash`: lines 1381–2246 (865 lines).  
One-file TUI with 260-field `AppState`, event loop, render, tests. Merge-conflict magnet.  
Fix: Extract `dispatch_slash` → `src/slash.rs`; `handle_key` → `src/event.rs`; `AppState` → `src/state.rs`; render functions → `src/renderer.rs`. No API changes required.

**Q-3 `opentelemetry-semantic-conventions` compiled into every build but never imported**  
`Cargo.toml:69` (workspace dep), `crates/smedja-adapter/Cargo.toml:13` (only consumer). Zero `use opentelemetry_semantic_conventions` across all 222 `.rs` files. Semantic-attribute constants are presumably hardcoded as string literals elsewhere.  
Fix: Remove from `smedja-adapter/Cargo.toml`. Either move to `smedja-telemetry` where it belongs, or remove from workspace deps entirely.

### Medium

**Q-4 `agents.toml` `tools` field parsed but silently discarded — per-role tool gate not wired**  
File: `crates/smedja-assayer/src/config.rs:19` (`#[allow(dead_code)] tools: Vec<String>`)  
The `BashArity` classifier is already wired (`executor/mod.rs:393`); only the per-workspace role policy lookup is missing.  
Fix: Pass `tools` vec through `RoutingRule`/`ToolPolicy` into `HandlerState`; consult it in `executor::role_allows_write_bash` alongside session mode.

**Q-5 `ContextRail.visible` and `toggle()` are dead — visibility controlled by `AppState.panels.context_rail`**  
File: `crates/smedja-tui/src/context_rail.rs:87`, `:102`  
Suppressed with `#[allow(dead_code)]`. `ContextRail::new` always passes `visible: true`; the field is never read in render. State duplication footgun.  
Fix: Remove `visible` and `toggle()` from `ContextRail`. It's a stateless renderer; the caller owns the flag.

**Q-6 Stale `#[allow(dead_code)]` comment on `turn_in_flight` — active field, misleading docs**  
File: `bin/smedja-tui/src/main.rs:326`  
Comment says "read path hasn't landed yet" but the field is read at line 3533, set at 612, cleared at four sites, and tested. The allow attribute suppresses a warning that no longer exists.  
Fix: Remove the attribute and stale comment. Add: `/// True while a turn is awaiting a streaming response.`

**Q-7 `smedja-bellows::drain_ready` has no test for the `Lagged` arm**  
File: `crates/smedja-bellows/src/lib.rs:32`  
`Lagged(n)` breaks out of the drain loop, silently returning a partial batch. Under burst load (channel capacity 16) this fires in production.  
Fix: Add a test that forces `Lagged` by publishing more events than the receiver's queue, then asserts `drain_ready` returns an empty or partial batch without panicking.

**Q-8 `session.create` is 161 lines with a background `tokio::spawn` and no tests**  
File: `bin/smdjad/src/handlers/session.rs:23`  
Background workspace re-index side effect cannot be tested without extraction.  
Fix: Extract `maybe_reindex_workspace(state, workspace_root)` as a named function testable in isolation.

**Q-9 `cumulative_totals` dead SQL function reserved for "future cost-estimator integration"**  
File: `crates/smedja-ingot/src/token_snapshot.rs:91`  
`handlers/savings.rs` and `handlers/cost.rs` already compute totals by different means. Schema migration footgun if the function's SQL assumptions diverge from the actual schema.  
Fix: Determine whether it duplicates `ingot.session_cost()`. If yes, delete. If no, wire it in and remove the `allow(dead_code)`.

### Low

**Q-10 `review_route` in `auditor.rs` is a dead function documenting a routing invariant**  
File: `bin/smdjad/src/handlers/auditor.rs:885`  
The invariant is already tested in `smedja-assayer`. Dead production code with stale comment.  
Fix: Delete `review_route`. Reference the assayer test in a module comment if the docs intent matters.

**Q-11 `BlockStore` and its `impl` carry `#[allow(dead_code)]` on live, actively-used code**  
File: `bin/smedja-tui/src/blocks.rs:171`, `:177`  
`push()`, `len()`, `is_empty()` are all called in production paths. Comment says "upcoming story" but the story shipped.  
Fix: Remove both `#[allow(dead_code)]` attributes. If the compiler is silent, they were not needed.

**Q-12 `tokio-test` workspace dep possibly unused**  
File: `Cargo.toml:95`  
Listed in workspace deps, referenced only by `smedja-ingot` dev-deps. No `use tokio_test` found in any `.rs` file; ingot tests use `#[tokio::test]`.  
Fix: Verify with `grep -rn "tokio_test::"`. If zero hits, remove from ingot dev-deps and workspace table.

---

## Summary

| Dimension | Critical | High | Medium | Low | Total |
|-----------|----------|------|--------|-----|-------|
| Security  | 0        | 0    | 0      | 0   | 0     |
| SRE       | 2        | 5    | 6      | 6   | 19    |
| Product   | 0        | 6    | 10     | 4   | 20    |
| Quality   | 0        | 3    | 5      | 4   | 12    |
| **Total** | **2**    | **14**| **21** | **14** | **51** |

## Implementation order

**P0 — fix before next release:**
R-1 (OOM cache), R-2 (MCP timeout), R-13 (migration atomicity), R-7 (prune transaction)

**P1 — next sprint:**
R-3, R-4, R-5, R-6, R-12 (reliability loop); P-1, P-4, P-5 (new-user blockers); Q-1 (handler tests); Q-2 (main.rs split); Q-3 (dead dep)

**P2 — following sprint:**
R-8, R-9, R-10, R-11, R-14; P-2, P-3, P-6, P-7, P-8, P-11, P-16; Q-4, Q-5, Q-7, Q-8

**P3 — backlog / polish:**
R-15–R-19; P-9, P-10, P-12–P-15, P-17–P-20; Q-6, Q-9–Q-12

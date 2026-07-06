# Changelog

All notable changes to smedja are documented here.

Format: `## [version] — YYYY-MM-DD` / `### Added|Fixed|Changed|Removed|Roadmap`.

---

## [0.25.2] — 2026-07-06

### Fixed
- **/review timed out at 30s** — audit.run is a minutes-long blocking auditor loop
  that the 0.25.0 default 30s client RPC timeout killed. It now runs under a 30-min
  long-timeout with a "reviewing…" progress message; ordinary RPCs keep the 30s
  guard.

### Changed
- Split the 3,818-line smedja-tui tests.rs into a per-feature tests/ directory +
  a shared test_support module; dropped the test-only main.rs re-exports.

## [0.25.1] — 2026-07-06

### Added
- **Semantic memory by default** — a bundled local sentence embedder
  (all-MiniLM-L6-v2, 384-dim, downloaded on first use) replaces the lexical FNV
  hash as the default; FNV remains the offline fallback with degraded-status
  surfacing. Recall separates a zero-shared-word paraphrase from noise (0.516
  margin) where FNV cannot (0.000).
- **ACP item 4** — per-session mcpServers wiring + the tool_call/tool_call_update
  status stream (pending -> in_progress -> completed|failed) + a diff content type.
- **Live shared block on handoff** — session takeover seeds a concurrently-editable
  block the receiver reads and appends to (snapshot fallback intact).
- **GPU-terminal hanging indent** — transcript body aligns under the author label
  via render geometry (copy/selection stays clean; the ANSI-space hack avoided).

### Fixed
- **Transcript scroll** — the anchor was pinned to the last line in follow mode, so
  scroll-up did nothing for a viewport; now scrolls one line per press and re-arms
  follow at the bottom.
- **External-runner output** — codex/claude output now gets markdown hierarchy
  (headings, bullets, links) and dim chrome, and the redundant `↳ ok · [cmd]` echo
  is dropped (one dim line per external tool call).

### Changed
- **Orchestrator `run()` refactor** — the ~1600-line god-method is a 56-line driver
  over a TurnRun state struct + 16 phase methods; the too_many_lines suppression is
  gone. Behavior-preserving.
- **Enforcing file-size gate** — a ratcheting clean-as-you-code gate blocks new
  oversized files + growth of baselined ones.

## [0.25.0] — 2026-07-06

The universal-agent-runtime release: bring any backend (claude-cli, codex-cli,
minimax, local) and smedja layers the same superpowers on all of them, delivered
through three runner-agnostic rails (the shared tool catalog, the system-prompt
block, and the MCP server) plus an ACP coordinator.

### Fixed (correctness tranche)
- claude-cli approvals now route through the real interactive cowork gate (Ask
  prompts instead of hard-denying; Modify -> updatedInput); fail-CLOSED when a
  gate is expected but missing.
- codegraph 0-symbols: /index absolutizes + the daemon canonicalizes/validates
  the path (real error instead of silent Ok(0)); one canonical spelling shares the
  DB key with query.
- The loop's fix role now receives the real verification output (failing tests +
  stdout/stderr) instead of a generic "slice N failed" string.
- claude-cli/codex-cli now receive smedja's injected skills/rules/methodology
  (--append-system-prompt-file / managed AGENTS.md) instead of silently discarding
  them; detect_agents_md wired both directions.
- Tier ranking unified to Local < Fast < Deep (one source of truth), fixing a
  failover that could rotate a Fast turn down to a weaker Local provider.

### Added (runner-agnostic capabilities)
- **Skills/rules/agents**: one normalized Bundle (collapses the two skill systems
  + an external toolkit folder), an auto-activation selector (L1 index + selected
  bodies), an MCP prompts/resources surface, and subagent-def materialization.
- **LSP as agent tools**: a real LSP client (didOpen/sync + request/response), the
  lsp_* tools (references/definition/hover/workspace-symbols/rename), and a
  post-edit diagnostics feedback loop — on the tool-result path, so every runner
  gets them.
- **test_run**: language-agnostic detect -> run-all -> parse (libtest/go/jest/junit)
  -> one TestReport, affected fast-mode; the loop reuses it.
- **review_run**: polyglot deterministic tools (rustfmt/clippy/ruff/prettier/
  eslint/gofmt/...) -> SARIF + an LLM layer -> a seven-dimension quality bar.
- **spec.***: a native in-daemon OpenSpec engine (delta parser/merger,
  create/validate/show/diff/list/archive-with-merge) + spec_* tools + a validate
  gate before loop.run, replacing the external-binary shell-out.
- **Loop x tier**: an executed plan phase (plan -> implement -> verify -> review),
  the plan=fable / implement=sonnet|haiku / review=opus binding, an escalate-on-
  failure descent ladder, per-tier token accounting, and problem-statement
  continuity across handoffs.
- **Memory**: an in-process ANN index (scales past full-scan), a status-surfacing
  embedder (no silent lexical fallback), an MCP memory surface (memory_search/
  write/list) so external clients share the vault, and live concurrently-editable
  shared blocks for parallel agents.
- **ACP coordinator**: session/request_permission routes into the one cowork gate;
  session/load replays history; allow-always persists as a permission rule.

### Changed
- Single canonical git-root active repo (git-root detection; auto-index on
  activation; real mtime incrementality replacing the dead commit_sha path).

Gates: workspace build 0 warnings, clippy -D warnings clean, fmt clean, 2428 tests.

## [0.24.3] — 2026-07-05

### Changed
- **Transcript readability overhaul** — the main message window now guides the
  eye instead of reading as uniform monochrome:
  - Tool calls collapse to one dim line (`⏵ execute · <cmd> · ✓ 0.4s`, only the
    status glyph colored); consecutive successes coalesce, expand on failure —
    the wall of `✓ execute … ok` becomes quiet chrome.
  - Role differentiation: molten-orange bold `you` + heavy gutter bar; the
    assistant kept quietest so its prose is the main content; thinking dim/italic;
    tools dim gray.
  - The dim-the-chrome rule enforced (no echoed path / ok / timestamp brighter
    than content), with a test as the invariant.
  - Full markdown rendering (headings, inline-code tint, bold/italic, lists,
    blockquotes, thematic-break rules) — and `- item` no longer mis-renders as a
    red diff removal.
  - Diff `@@` hunk headers dimmed, bold file-path labels; a guard test ensures
    syntax highlighting yields distinct token colors.
  - Disciplined semantic palette with a truecolor→16-color ANSI fallback so
    tmux/remote sessions stay legible.

## [0.24.2] — 2026-07-05

### Fixed
- **LSP panel overwritten by the trace-waterfall:** the LSP rail slot used an
  unbounded `Fill` so enabling the trace starved it to zero height. It now has a
  minimum-height floor and both panels coexist.
- **Trace "x to inspect" did nothing:** the keybind was trapped inside scroll
  mode; it now fires when the trace panel is visible in normal mode (steps + expands
  spans with name/kind/duration/status), with an accurate state-aware hint, and
  never steals a typed `x` mid-message.
- **Obs panel throughput stuck at zero:** the mid-stream Usage events updated the
  live token counters but not the obs snapshot, so the throughput bar stayed at
  zero during a turn. Usage now feeds obs throughput live (per-turn high-water
  mark over committed totals).

## [0.24.1] — 2026-07-05

### Fixed
- **Quality/value panels never populated:** the post-turn `QualitySnapshot` is
  emitted just after `Completed`, but both stream ends closed on `done`, so the
  trailing quality event was always dropped — leaving the quality score (and the
  value ROI derived from it) stuck at zero. Added a bounded grace window that
  holds the stream open for the trailing snapshot on both server and client.

### Changed
- Quality panel: A–F grade badge + score gauge + verdict pill + a quality trend
  sparkline over recent turns + per-gate pips (tdd/clean/size/skill). Value panel:
  ROI gauge + cost-vs-value micro-bar. Clear empty states for LSP ("no server for
  <lang>"), savings ("accrues on cache hits & filtering"), and value ("no active
  change") instead of bare zeros.

## [0.24.0] — 2026-07-05

A major hardening + terminal-UX release: closed the security holes and crash/leak
bugs a review surfaced, overhauled the TUI visualization, and decomposed the
god-modules.

### Fixed
- **Security (Tier-0):** Docker read-confinement escape via a `.git` symlink;
  Landlock post-fork allocation deadlock; SSRF (DNS-rebind pinning + no unchecked
  redirects); sandboxed children reaped on timeout; socket single-instance +
  0600 permission window; **terminal SSH now verifies host keys (was accept-any,
  MITM)**; pipe-deadlock + blocking-in-async on the daemon runtime; **approvals
  fail-closed** (Ask no longer auto-allows; codex maps to `--sandbox` instead of a
  hardcoded bypass).
- **Panics:** 5 UTF-8 byte-slice crash sites (emoji/CJK), a statusbar
  use-after-free, a vault `cast_slice` DoS, and boot-time `expect` panics.
- **Data-loss/correctness:** non-transactional vault writes, loop resume
  re-running the batch + parallel slices sharing one workspace, streaming UTF-8
  corruption in all provider adapters, an RPC hang, an in-flight-session GC wipe,
  a provider-key collision disabling Berget, orchestrator cap-exhaustion persisted
  as an answer, and ingot count/migration/overflow bugs.
- **Leaks:** PTY thread/fd/zombie on close, the approval map, stream buffers,
  unbounded TUI vectors + colour atlas; plus skill/role path-traversal guards.

### Added
- **TUI visualization overhaul:** real status-bar telemetry (tokens/latency/trace/
  context, previously hardcoded 0); the **molten-orange brand accent**; unified +
  cached syntax highlighting (tree-sitter for Rust/Go/Python/TypeScript);
  ACP-schema tool-call cards; a state-keyed **live line**; an **in-terminal OTel
  trace-waterfall** in the obs panel; plan step-completion; a **multi-agent fleet
  roster**; and change-detection alerts.

### Changed
- Decomposed the god-modules (executor, orchestrator, `smdjad`/`tui` main,
  st-pty, st-render, ingot, vault) into cohesive modules; public paths preserved.
- CLI: `term convert-wezterm` wired to the real migration engine; `session export`
  retargeted to a registered RPC; `/agent` accepts all real roles.
- Docs reconciled to the code (traceparent scope, compaction, vault search,
  `/health`); disambiguated smedja's ACP (Agent Coordination Protocol) from the
  Zed/JetBrains Agent Client Protocol.

### Removed
- `session blocks` (no daemon block store); dead `code_widget` + unused
  statusbar modules.

## [0.23.3] — 2026-07-01

### Fixed

- **Release build icon embedding** — moved the Linux window icon loader to the
  parent `st-app` module and fixed the embedded brand asset path after the app
  module split.

## [0.23.2] — 2026-07-01

### Changed

- **Thin CLI entrypoint** — `smj` dispatch now routes through focused modules (`audit`, `sessions`, `tasks`, `usage`, `workspace`, `timeline`, `local`, `governance`, `mcp`, `sandbox`, `security`, `terminal`, `eval`, and daemon control) instead of one large command body.
- **Grouped CLI command definitions** — `clap` subcommand shapes moved under `crates/smedja-cli/src/cli/commands/` by domain, keeping `cli.rs` focused on the root parser and top-level `Cmd` enum.
- **Thin terminal app entrypoint** — `st-app` tests moved out of `lib.rs`, leaving the terminal library entrypoint focused on startup and event-loop wiring.
- **Maintenance documentation** — added `docs/maintenance.md` with module ownership, test placement policy, and verification commands; linked from README and docs index.

### Fixed

- **Refactor regression coverage** — CLI and terminal app test suites now live in dedicated test modules, preserving existing parser, formatting, workspace, security, input, title, and app-construction coverage after the module split.

## [0.23.0] — 2026-06-30

### Added

- **Large response offload** — tool responses >100 k chars are written to `$TMPDIR/smedja-tool-responses/<hash>` and replaced with a compact reference in the model context; prevents single large outputs exhausting the context window (WI-022)
- **Extended skill frontmatter** — `arguments`, `tags`, and `supporting_files` fields on skill manifests; `apply_skill_arguments()` substitutes `$NAME`, `$ARGUMENTS`, and `$ARGUMENTS[n]` placeholders in skill bodies (WI-023)
- **Multi-IDE skill symlinks** — `smj skills link-ides [--dir <project>]` creates `.codex/skills` and `.cursor/skills` symlinks to `~/.claude/skills/` (WI-024)
- **Unicode tag sanitization** — U+E0000–U+E007F stripped from user messages before forwarding to providers (WI-025)
- **Turn-context injection** — a `<turn-context>` XML block with the current date and working directory is prepended to the first user message of each turn (WI-026)
- **`load_skill` built-in tool** — registered in LOCAL_TOOLS, MCP_SERVER_TOOLS, and READ_ONLY_TOOLS; loads a skill by name from the default registry and returns it wrapped in XML (WI-027)
- **`TurnEvent::HistoryReplaced`** — emitted after auto-summarisation succeeds, carrying session ID, turn ID, and estimated summary token count (WI-028)
- **Warm message truncation** — warm-stratum messages that exceed the remaining token budget are now truncated to the budget with a `[... truncated]` notice instead of being dropped entirely (WI-029)
- **Session search** — `Ingot::search_sessions(query)` and `IngotHandle::search_sessions(query)` do case-insensitive substring search on `title` and `workspace_root`; exposed as `session.search` RPC (M4)

### Fixed

- **Pre-commit hang** — `LandlockBackend::netns_supported()` now probes with a 3-second thread timeout; the netns-degraded-path test is marked `#[ignore]` (CI-only) to prevent indefinite blocking on machines where the kernel policy stalls namespace creation

---

## [0.22.1] — 2026-06-30

### Fixed

- **Adapter bwrap crash** — disabled `claude` CLI's own bwrap subprocess isolation via `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=0` and always pass `--dangerously-bypass-approvals-and-sandbox` to `codex`; both CLIs failed with `EAFNOSUPPORT` on `AF_NETLINK` when smdjad ran inside a Claude Code session whose seccomp filter blocked the required socket
- **Model flag not forwarded to claude** — `opts.model` (e.g. `claude-haiku-4-5-20251001`) was captured but never passed as `--model` to the `claude` binary; haiku and other model selections had no effect
- **Session list unbounded** — `session.list` now returns at most the 10 most recent sessions

---

## [0.22.0] — 2026-06-30

### Added

- **Executor tools** — `grep_files`, `find_files`, `move_file`, `copy_file`, `delete_file` with workspace-containment and session permission checks (WI-019)
- **Parallel multi-tool execution** — read-only tools in a multi-tool model response run concurrently via `FuturesUnordered`; writes go through the cowork gate sequentially (WI-020)
- **Mid-stream usage events** — `StreamEvent::Usage` emitted for each `Delta::Usage` from the provider so clients can show live token budgets (WI-021)
- **Tool-call chunk streaming** — `StreamEvent::ToolCallChunk` emitted for each partial `input_json` fragment from Anthropic and OpenAI providers; `StreamEvent::ToolCall` remains as the terminal complete event (WI-021)
- **Fetch-web tool** — `fetch_web` with SSRF protection via `NetworkPolicy` and configurable `max_bytes` cap (WI-013)
- **Declarative cowork permission rules** — `[[permission.rules]]` in `.smedja/workspace.toml` for tool-level allow/deny patterns without interactive prompting (WI-014)
- **Bash extensions** — per-call `timeout_secs`, `env`, `stdin`; configurable compact threshold; denylist enforcement; stderr block on non-zero exit; partial output on timeout (WI-011, WI-012)
- **Loop parallel slice execution** — `drive()` runs up to `max_parallel_slices` slices concurrently via bounded semaphore (WI-015)
- **Max-tool-turns cap and loop checkpoint/resume** — `limits.max_tool_turns` terminates runaway tool loops; `.smedja/loop-state.json` checkpoint enables `loop.resume` re-entry (WI-016, WI-017)
- **Poolside pool adapter** — `Pool` runner wraps the `pool` CLI subprocess for `poolside-ai` remote execution (WI-018)
- **Read-file line ranges and base64 encoding** — `start_line`/`end_line` and `encoding: base64` on `read_file`; `depth`/`pattern` on `list_files` (WI-010)
- **Fork-at-turn** — `session.fork` with `turn_n` allows re-entering a session from any prior turn (WI-010)

### Fixed

- **Clippy pedantic** — resolved `cast_possible_truncation`, `items_after_statements`, `option_if_let_else`, `must_use`, and `too_many_lines` warnings introduced across WI-001–WI-021

---

## [0.20.9] — 2026-06-28

### Changed

- **All panels visible by default** — metrics, session rail, and all right-rail panels now open on startup for full-visibility testing.

---

## [0.20.8] — 2026-06-28

### Changed

- **Default panel visibility** — quality gate panel and role cockpit panel now open by default alongside obs, lsp, and value panels.

---

## [0.20.7] — 2026-06-28

### Fixed

- **Codex trust check** — `codex exec` now runs with `current_dir` set to the session workspace root and `--skip-git-repo-check`, resolving the `Not inside a trusted directory` error when smdjad is rooted outside the indexed project repo.

---

## [0.20.6] — 2026-06-28

### Added

- **Quality panel (Tier 1)** — deterministic post-turn quality scoring (TDD, clean build, file-size, skill-injection gates) displayed in the right rail. `Ctrl-Q` toggles the panel; score bands colour-coded green/amber/red. Two consecutive sub-60 scores push a `CoworkGate` soft-interrupt.
- **Quality panel (Tier 2)** — on-demand LLM review via `/quality` or `Ctrl-Q` hold ≥ 500 ms. Routes to an adversary model (cross-family from the primary provider) and surfaces a rubric-based score with up to 5 actionable findings. Panel shows `[llm]` badge; gracefully falls back when the adversary model is unreachable.
- **Value panel** — `Ctrl-V` toggles a right-rail ROI panel tracking cumulative token cost (and USD estimate) for the active openspec change. Polling reuses the existing 3-second obs interval. `/value` prints a Markdown cost/quality report to the main view.
- **Token cost attribution** — `change_name` column added to `audit_events` (migration 25, `ALTER TABLE … ADD COLUMN`, zero downtime). Active openspec change detected at smdjad startup and stamped on every audit event; `cost.active_change` RPC endpoint exposes the running total.

### Fixed

- **Input wrapping** — multi-line input in the TUI now wraps at the content-area width rather than the full terminal width, preventing overflow past the right rail.

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

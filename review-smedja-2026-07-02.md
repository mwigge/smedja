# smedja Code Quality Review

Review date: 2026-07-02
Repository: `/data/src/smedja` @ `d20d57b` (v0.25.0)
Scope: **code quality** (structure, duplication, error handling, complexity, gates) — not a README/feature audit. See `rewiev-smedja-0628-md` for the prior functional review.

## Verification run

- `cargo fmt --all -- --check`: **clean** (prior review's failure is fixed).
- `cargo clippy --workspace --all-targets`: **clean**, 0 warnings (prior `await_holding_lock` failures now `#[allow]`'d with justifications — fixed, see note in §Medium).
- Scale: ~226 Rust files, ~101k LOC incl. `term/`. Largest: `smedja-tui/main.rs` 9013, `smj/main.rs` 4262, `smdjad/executor/mod.rs` 3529, `smdjad/orchestrator/mod.rs` 3044.

## Status of the prior (2026-06-28) findings

| Prior finding | Status |
|---|---|
| Loop pipeline not wired to `verify` | **Fixed** — `loop_runner` → `smedja_loop::drive` → `run_verification` (engine.rs:318, verify.rs:45) |
| fmt not clean | **Fixed** |
| clippy `await_holding_lock` fails gate | **Fixed** (suppressed with test-only justifications) |
| `write_file`/`edit_file` not native | **Still open** (finding H1 below) |
| Duplicated path-resolution helper | Partly — now routed via `assert_within_workspace`, but config re-parse duplication remains (M2) |

## Addressed in this commit (2026-07-02)

- **H1** — `write_file`/`edit_file` now have native dispatch arms in `execute_tool` (create-with-parents write; unique-match edit with `replace_all` for ambiguous cases). No longer falls through to MCP.
- **H2** — Gemini key moved from the URL query string to the `x-goog-api-key` header (marked sensitive).
- **H3** — `AnthropicProvider`/`OpenAiProvider`/`GeminiProvider` now build clients via `streaming_http_client()` (15s connect + 120s idle read timeout; no overall timeout so long streams survive).
- **M1** — Gemini non-2xx routed through `classify_http_error` + explicit 429→`RateLimited` (retryable), matching the other providers.
- **M2** (client half) — SRE `otel_query`/`metric_query`/`log_tail` share one `OnceLock` client instead of rebuilding per call.
- **M5** — parallel read batch now threads `session` into `execute_tool` (consistent secret-scan + audit).
- **Low** — `ingot/session.rs` search escapes LIKE `%`/`_`/`\` and adds `ESCAPE`.

Deferred (unchanged): **H4** monolithic-function decomposition (multi-day refactor), M2 workspace.toml re-parse, M3 dedup, M6 Result-vs-Vec, M7/M8 allow/clone hygiene, remaining Lows.

## Findings

### High

**H1 — `write_file`/`edit_file` advertised + fully gated but have no native dispatch.**
`executor/mod.rs:86-87` lists them in `LOCAL_TOOLS`; they pass every native guard (path-traversal, methodology gate, review-mode block, confirm_edits) but `execute_tool` has **no match arm** — they fall through to `dispatch_mcp_tool` (`:1281`) and return `"error: tool 'write_file' is not available"` unless an MCP server happens to own the name. `move_file`/`copy_file`/`delete_file` *are* native. Either implement native arms mirroring those, or drop write/edit from `LOCAL_TOOLS` and the README.

**H2 — Gemini API key sent in the URL query string.** `adapter/gemini.rs:178` uses `?key={api_key}`. URLs are logged by proxies, servers, and reqwest tracing far more readily than headers. Move to an `x-goog-api-key` header.

**H3 — HTTP providers have no request/connect timeout.** `adapter/anthropic.rs:25`, `openai.rs:34/51`, `gemini.rs:36/44` build `reqwest::Client::new()` with no timeout; a stalled LLM connection hangs the stream forever (CLI/subprocess paths have `kill_on_drop`, HTTP has nothing). Use `Client::builder().timeout(...)` as `local.rs:224` already does.

**H4 — Monolithic functions are the dominant maintainability problem.** Several single functions carry whole subsystems and are untestable as units / merge-conflict magnets:
- `orchestrator/mod.rs:309` `TurnOrchestrator::run` ~1600 lines (routing + context + streaming + tool-loop + gating + cost + OTel).
- `smedja-tui/main.rs:1596` `handle_key` ~1340 lines, 94 KeyCode arms; `:405` `AppState` ~100-field god struct; `:2936` `render` ~558 lines.
- `smj/main.rs:1235` CLI `main` ~1360 lines with a 117-arm match, each arm inlining connect+RPC+format+print.
- `tui/slash.rs:300` `dispatch_slash` ~1110 lines / 76 arms.

Fix pattern is the same everywhere: extract per-phase/per-mode/per-command private methods (the codebase already does this well for `cmd_doctor`, `render_slash_popup`, etc.) and split `AppState` into sub-structs (`SlashPopupState`, `FilePickerState`, `PollTimers`, …). This is also the root cause of the 34 `#[allow(too_many_lines)]` + 10 `too_many_arguments` suppressions.

### Medium

**M1 — Gemini error handling is weaker than its siblings.** `gemini.rs:217-225` collapses every non-2xx (429/500/quota) into `InvalidResponse`, which `is_retryable()` treats as non-retryable, and never handles `TOO_MANY_REQUESTS`. Route through `classify_http_error` like `anthropic.rs:396`/`openai.rs:210`. The 429-parse + classify + span-status block is copy-pasted across anthropic/openai anyway — extract one `handle_http_status(resp)` helper and bring gemini onto it.

**M2 — Repeated re-parse / re-build on the hot path.**
- `executor/mod.rs:474-506`: `is_confirm_edits_enabled` and `bash_config` each re-open+parse `.smedja/workspace.toml` on **every tool call**. Parse once.
- `executor/mod.rs:1130-1181`: the reqwest client (identical 15s/5s timeouts) is rebuilt in `otel_query`/`metric_query`/`log_tail`, each with `.unwrap_or_default()` that silently yields a broken default client on failure. Hoist to a shared `OnceLock<Client>`.
- `orchestrator/mod.rs:362-469`: `ingot.get_session(&id)` awaited 4× in the first ~160 lines of `run` — load once, reuse (also removes inconsistent-read risk).

**M3 — Duplicated blocks worth factoring.**
- `loop_runner.rs:355-393` vs `513-551`: `run` and `resume` duplicate the umbrella-preload block and the full `LoopRoleRunner` literal (~40 lines each, verbatim). Extract `build_runner` + `preload_umbrella_sources`.
- `adapter/{claude_cli,codex_cli,subprocess}.rs`: the spawn → stream-stdout → `wait()` → read-stderr loop is reimplemented 3× with subtle divergence (codex surfaces empty-output errors, claude doesn't). Factor a shared CLI-stream driver.
- `smj/main.rs`: 13+ subcommands inline `Client::connect(...).with_context("smdjad not running")`, while the purpose-built `connect_or_exit` (`:2907`, better multi-line guidance) is used only 3×. Route all through `connect_or_exit`.

**M4 — Gate/security hooks that are inert but read as enforcement.** `executor/mod.rs:564` (`confirm_edits`), `:948` (`delete_file` cowork gate), `:1066` (`smedja_retrieve` audit) just `tracing::info!("... is in roadmap")` and proceed. Either remove or clearly mark inert so they aren't mistaken for controls.

**M5 — Parallel tool batch drops session context.** `orchestrator/mod.rs:1197-1205` calls `execute_tool(..., None, ...)` in the multi-tool batch, so the output secret-scanner and session-scoped audit attribution are skipped for batched tools but applied when the same tool runs singly. Thread `session.as_ref()` in for a consistent security posture.

**M6 — Silent error swallowing in retrieval.** `memory/cold.rs:32` (`ColdStore::retrieve`) and `memory.rs:317` (`cold_context`) return `Vec` not `Result`, so a failing vault/embedding query is indistinguishable from "no matches". Return `Result` or at least log.

**M7 — `#[allow]` sprawl (192 total).** Top: `cast_possible_truncation` 40, `too_many_lines` 34, `cast_precision_loss` 17, `dead_code` 16, `await_holding_lock` 12, `too_many_arguments` 10. Most `await_holding_lock` are test-only ENV_LOCK (fine), but confirm `adapter/local.rs:444,469,557` and `codex_cli.rs:668` (no "test-only" comment) aren't holding a std Mutex across await in production streaming. `smedja-tui/main.rs` suppressing 8 `unused_imports` signals conditional-compile churn — clean those up.

**M8 — `.clone()` churn on hot paths.** `orchestrator/mod.rs` 69 non-test clones, `stream_server.rs` 36 (all non-test, per-message on the stream path). Worth an Arc/borrow pass since both sit on the request path.

### Low

- `plugins/registry.rs:40` — `.expect("HOME ... must be set")` panics in HOME-less containers/CI; degrade gracefully.
- `adapter/claude_cli.rs:87-91` — tool-gate settings written to a fixed shared `temp_dir()/smedja-claude-settings.json`; concurrent sessions race and a write failure silently fails **open** (gating disabled). Per-session temp file + log on write error.
- `tui/main.rs` — `permission_mode`/`tier`/`runner` are stringly-typed (`"ask"`,`"accept_edits"`,`"plan"`) compared via literals across the file; typos compile. Introduce enums with `as_str`/`FromStr`.
- `tui/main.rs:186-341` — `SLASH_COMMAND_DESCRIPTIONS` and `SLASH_COMPLETIONS` are hand-maintained parallel lists; derive the second from the first.
- `vault/vault.rs:377` — `search` full-scans a namespace and scores in Rust per query (no vector index); fine now, note the ceiling.
- `ingot/session.rs:231` — `LIKE '%{q}%'` is parameterized (no injection) but doesn't escape `%`/`_`; add `ESCAPE`.
- `smedja-tui/main.rs` — 237 tests inlined in the same file inflate it past 9k lines; move `mod tests` out via `#[path]`.

## What's solid

- **Panic-safety is genuinely excellent**: after excluding tests, only ~26 `unwrap`/~34 `expect` in production, none on untrusted input; 0 `panic!`/`todo!`/`unimplemented!`, 0 `dbg!`, no stray `println!` in library/daemon code (enforced by `methodology/clean.rs` + `quality_runner`).
- **Concurrency is careful**: no std lock held across `.await` in production; vault/sqlite work uses `spawn_blocking` + poison-tolerant guards; structured cancellation via drop guards.
- **Good existing factoring**: SSE parsing centralized in `adapter/sse.rs`; `AdapterError`/`is_retryable`/`classify_http_error` taxonomy; SSRF DNS re-resolution in `fetch_web`; all SQL parameterized.
- fmt + clippy both green; ~966 tests; only 5 production `unsafe` blocks, all with SAFETY comments.

## Recommended order

1. H1 (implement or unadvertise write/edit) and H2/H3 (Gemini key + HTTP timeouts) — correctness/security, small diffs.
2. M2/M3 dedup + M5/M6 (session threading, retrieval errors) — cheap, real.
3. H4 decomposition — highest maintainability payoff, largest effort; do it crate-by-crate (orchestrator `run`, then tui `handle_key`/`AppState`, then CLI `main`). Retire the `too_many_lines`/`too_many_arguments` allows as you go.
4. M7/M8 (allow-sprawl + clone churn) — ongoing hygiene.

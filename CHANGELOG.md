# Changelog

All notable changes to smedja are documented here.

Format: `## [version] — YYYY-MM-DD` / `### Added|Fixed|Changed|Removed|Roadmap`.

---

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

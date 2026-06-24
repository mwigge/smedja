## Context

The agent loop spends a large share of its context budget on verbose plain-text command output. The relevant surface today:

- `compress_command_output(cmd, output) -> (String, f32)` in `crates/smedja-adapter/src/crush.rs` already does command-aware *text* compression, but only for two hardcoded cases: `identify_command` recognises `cargo test` and `git status`, and everything else falls through to `remove_blank_lines`. It honours the `SMEDJA_NO_TOOL_COMPRESS=1` bypass and returns a `(compressed, ratio)` pair. There is also a `command_compressor(cmd)` transform constructor and a `ContentPipeline` for chaining transforms.
- `compress_tool_result` (SmartCrusher) in the same file strips JSON nulls. It is the structured-JSON analogue of what this change adds for text.
- `execute_tool` in `bin/smdjad/src/executor/mod.rs` dispatches `bash`/`run_command`: it resolves the command string from the `command`/`cmd` input field, runs it through `SandboxExecutor::run_confined` (which captures combined output), and then passes every tool result through `scan_tool_output` on the return path before returning it. `scan_tool_output` is the model for an advisory return-path transform: it inspects the result, records an audit event, and by default returns the content unchanged.
- The `smedja_retrieve` tool reads from an in-memory `retrieve_store()` keyed by content hash and is the existing mechanism for expanding compressed/omitted content.
- The vault (`crates/smedja-vault/src/vault.rs`) stores `VaultEntry { id, content, namespace, … }` and is searchable/retrievable; `smedja_vault_store` / `smedja_retrieve` are wired in the executor.
- `estimate_tokens(text)` in `crates/smedja-memory/src/budget.rs` (`len()/4`-style estimate) is the project's token estimator.
- The cost ledger (`crates/smedja-ingot/src/cost.rs`) records `CostEntry { input_tok, output_tok, … }` rows via `insert_cost`, aggregated by `smj cost`.
- Workspace config lives under `.smedja/`: `load_security_config` reads `.smedja/config.toml` (`security.rs`), `loop.json` and `agents.toml` are loaded similarly. A missing file degrades to defaults.

## Goals / Non-Goals

Goals:
- Compress verbose `bash`/`run_command` text output in-process, keyed on the detected command, before it enters working memory or is serialised to the provider.
- Provide four rtk-style strategies — smart-filter, group, truncate, dedup — selected per command by a filter registry.
- Let users add or override filters via a `.smedja/filters.toml` DSL.
- Tee the full uncompressed output to the vault so an over-compressed result is recoverable via `smedja_retrieve`.
- Record tokens saved by filtering to the cost ledger.
- Keep filtering advisory: unknown commands pass through unchanged; the bypass env var skips it.

Non-Goals:
- **No shell hooks.** We do not install shell wrappers or intercept output outside the program (rtk's delivery mechanism, explicitly discarded).
- **No subprocess-per-command.** We do not spawn a filter process around each command; filtering runs in-process on the already-captured output.
- **No command rewriting.** We never modify the command string, inject flags, or change what the user/agent asked to run.
- **No exit-code changes.** Filtering operates on the textual result only; it never alters the tool's success/failure contract or the captured exit status.
- Not replacing SmartCrusher for JSON — this is the text analogue; JSON results still flow through `compress_tool_result`.
- Not building a query language inside `filters.toml` — the DSL selects a strategy and parameters, nothing Turing-complete.

## Decisions

**Decision: command detection generalises the existing `identify_command`.**
Replace the two-arm `CommandKind` enum with a registry lookup: the first token (and where useful the first two tokens, e.g. `git status`, `cargo build`) of the trimmed command string keys into a `FilterRegistry`. The default set covers `git`, `cargo`, `pytest`, `npm`, `docker`, `kubectl` and preserves today's `cargo test` / `git status` behaviour as registry entries. An unrecognised command resolves to a pass-through entry (today's `remove_blank_lines` becomes the default `Other` strategy, configurable to `none`).
- Rationale: minimal blast radius — `compress_command_output`'s `(compressed, ratio)` contract and the bypass check are kept; only the dispatch table grows.
- Alternative: regex-per-line classification. Rejected — heavier, and the command token is a precise, cheap key the executor already has.

**Decision: four strategies, one per registry entry.**
- `smart-filter` — keep only high-signal lines (errors, warnings, failures) and drop progress/boilerplate. Generalises `compress_cargo_test`: e.g. a long `cargo build` collapses to its `error[...]` / `warning:` lines.
- `group` — cluster related lines under a heading. Generalises `git status`: entries grouped by directory with a per-group count.
- `truncate` — keep the first N lines and append an omitted-lines marker that names `smedja_retrieve` (mirrors `trim_code_block`'s `// … N lines omitted (smedja_retrieve to expand)` convention).
- `dedup` — collapse runs of identical (or near-identical after timestamp stripping) lines into a single line with an `(×N)` occurrence count.
Each strategy is a pure `fn(&str, params) -> String`; the registry maps a command to `(strategy, params)`.
- Rationale: pure functions are unit-testable in `crush.rs` exactly as the existing compressors are, and compose with `ContentPipeline`.

**Decision: `.smedja/filters.toml` DSL shape.**
A flat table of per-command filters, loaded like `.smedja/config.toml`:
```toml
[filters.cargo]
strategy = "smart-filter"   # smart-filter | group | truncate | dedup | none
keep = ["error", "warning"] # smart-filter: line markers to retain

[filters.kubectl]
strategy = "dedup"

[filters."docker build"]    # two-token key matches before one-token "docker"
strategy = "truncate"
max_lines = 40
```
Loading mirrors `load_security_config`: missing file → built-in defaults only; present file → user entries merged over (overriding) the default set by command key. Longer (two-token) keys win over shorter ones.
- Rationale: TOML and the `.smedja/` convention are already established; merge-over-defaults matches `agents.toml` override semantics.
- Alternative: per-command files. Rejected — one file is discoverable and matches the existing config files.

**Decision: tee-to-vault recovery via content hash + `smedja_retrieve`.**
Before returning the compressed result, the executor stashes the full uncompressed output under a recovery namespace (vault entry, content-addressed by hash, also registered in the `retrieve_store` the `smedja_retrieve` tool reads). The compressed result carries a trailing marker naming the hash (same UX as `trim_code_block`'s omitted-lines note), so the agent can call `smedja_retrieve` to expand. Only stash when filtering actually reduced the output (ratio < 1.0).
- Rationale: reuses the existing recovery tool and store; no new tool surface. Non-lossy in spirit — nothing is discarded, only deferred.
- Alternative: inline the full output behind a fold. Rejected — that defeats the token saving; the whole point is to keep the full text *out* of the prompt unless asked for.

**Decision: savings accounting on the cost ledger.**
Compute `saved = estimate_tokens(original) - estimate_tokens(compressed)` (clamped at 0) via `smedja_memory::estimate_tokens` and record it so `smj cost` can attribute it. Store it as a dedicated tokens-saved field/row keyed by session and turn, parallel to `CostEntry`, never folded into billed `input_tok`/`output_tok` (savings are negative cost, not incurred cost).
- Rationale: keeps the billed totals exact (the ledger's existing invariant) while still surfacing value delivered.

**Decision: bypass env var is the existing `SMEDJA_NO_TOOL_COMPRESS=1`.**
Command filtering checks the same bypass `compress_command_output` already honours. One switch disables both JSON and text compression, matching operator expectation.
- Rationale: a single, already-documented control; no new env surface.

**Decision: default filter set.**
`cargo` → smart-filter (errors/warnings), `git status` → group (by directory), `pytest` → smart-filter (failures/errors), `npm` → dedup + truncate, `docker` → dedup, `kubectl` → dedup. Anything else → `none` (today's blank-line removal stays as the conservative fallback).
- Rationale: these are the highest-volume noisy commands; conservative strategies (dedup/truncate) for the ones whose "noise" is least predictable.

**Decision: composition with SmartCrusher for mixed output.**
The executor inspects the result first: if it parses as JSON, route through `compress_tool_result` (SmartCrusher) unchanged; otherwise route through the command filter. They never both run on the same payload, so there is no double-compression. The `bash`/`run_command` path is text by construction, so it always takes the command-filter branch; the JSON branch covers structured MCP/tool results as today.
- Rationale: one branch point keeps the two compressors orthogonal and preserves SmartCrusher's existing behaviour exactly.

## Risks / Trade-offs

- [Risk] An aggressive filter drops a line the agent needed → Mitigation: tee-to-vault recovery makes every filtered output expandable via `smedja_retrieve`; the marker names the hash; `SMEDJA_NO_TOOL_COMPRESS=1` disables filtering wholesale.
- [Risk] Misdetecting a command applies the wrong strategy → Mitigation: unknown/ambiguous commands resolve to the conservative `none`/blank-line fallback, never an aggressive strategy; users override via `.smedja/filters.toml`.
- [Risk] A user `filters.toml` selects a destructive strategy on a critical command → Mitigation: recovery still applies (full output teed to the vault); filtering remains text-only and never touches exit codes.
- [Risk] Vault tee adds per-command write cost → Mitigation: stash only when filtering actually reduced the output, and only the once-per-command full text; writes use the existing `spawn_blocking` vault path.
- [Risk] Token-saving estimate is approximate (`len()/4`) → Mitigation: it is the same estimator the budgeting path uses; accuracy parity is sufficient for an advisory `smj cost` figure, and it is recorded separately from billed totals.

## Why

Verbose plain-text command output is the largest uncompressed source of context bloat in the agent loop. A single `cargo build`, `npm install`, or `kubectl get` can spill thousands of mostly-noise lines into working memory and onto the wire to the provider, crowding out signal and inflating cost. The existing SmartCrusher (`compress_tool_result` in `crates/smedja-adapter/src/crush.rs`) only compresses *structured JSON* tool results by stripping null fields; it does nothing for the command **text** that `bash`/`run_command` return, which is exactly where the bloat lives.

rtk-ai/rtk solves the same problem with command-aware filters (smart-filter, group, truncate, dedup), but it delivers them by wrapping the shell — hooks and a subprocess-per-command that intercept output outside the program. smedja owns its own execution layer (`execute_tool` dispatches `bash`/`run_command` through `SandboxExecutor::run_confined`), so we adopt rtk's *idea* — command-aware filtering keyed on the detected command — while discarding its delivery mechanism entirely. We filter **in-process** on the tool-result return path, where the command string and its captured output are both already in hand, so there is no shell hook, no extra process, and no risk of the filter and the executor disagreeing about what ran.

This change extends SmartCrusher from JSON to command TEXT: a filter registry keyed on the detected command (`git`, `cargo`, `pytest`, `npm`, `docker`, `kubectl`, …) compresses verbose output before it enters working memory or is serialised to the provider. Filtering is advisory and non-lossy in spirit — it never alters exit codes or breaks the tool contract, the full uncompressed output is teed to the vault so the agent can recover it if a filter over-compressed, and the tokens saved are recorded to the cost ledger so `smj cost` can show the value delivered.

## What Changes

- **Command-aware text filtering on the tool-result path**: extend the command compressor in `crush.rs` from its two hardcoded cases (`cargo test`, `git status`) into a filter registry keyed on a detected command, and invoke it from the `bash`/`run_command` arm of `execute_tool` (`bin/smdjad/src/executor/mod.rs`) so verbose command text is compressed before the result is returned and pushed into working memory.
- **Four filter strategies (à la rtk)**: `smart-filter` (collapse a long build to error/warning lines), `group` (cluster `git status` entries by directory), `truncate` (cap line count with an omitted-lines marker), and `dedup` (collapse repeated log lines with an occurrence count). Each registry entry selects the strategy for its command.
- **`.smedja/filters.toml` DSL**: a workspace-config file (loaded the same way as `.smedja/config.toml` / `loop.json` / `agents.toml`) lets users add or override filters per command, choosing the strategy and parameters. User entries override the built-in default set.
- **Tee-to-vault recovery**: the full uncompressed output is stashed in the vault under a recovery namespace, addressed by a content hash that is appended to the compressed result, so the agent can retrieve the original via the existing `smedja_retrieve` tool if a filter over-compressed.
- **Savings accounting**: the difference between pre- and post-filter token estimates (via `smedja_memory::estimate_tokens`) is recorded to the cost ledger so `smj cost` can attribute tokens saved by filtering.
- **Bypass env var**: `SMEDJA_NO_TOOL_COMPRESS=1` (already honoured by SmartCrusher) skips command filtering too, returning output verbatim.

## Capabilities

### New Capabilities

- `command-output-filtering`: `bash`/`run_command` text output is compressed in-process by a command-keyed filter registry applying one of four strategies (smart-filter, group, truncate, dedup) before the result enters working memory or is serialised to the provider; unknown commands pass through unchanged, exit codes are never altered, and `SMEDJA_NO_TOOL_COMPRESS=1` bypasses filtering.
- `filter-config`: a `.smedja/filters.toml` DSL lets users define and override per-command filters (strategy plus parameters), loaded from workspace config; user entries take precedence over the built-in default filter set.
- `output-recovery`: the full uncompressed command output is teed to a vault recovery namespace addressed by a content hash appended to the compressed result, so an over-compressed result is recoverable via `smedja_retrieve`, and tokens saved by filtering are recorded to the cost ledger.

## Impact

- `crates/smedja-adapter/src/crush.rs`: generalise `compress_command_output` / `identify_command` / `CommandKind` into a strategy-driven filter registry (smart-filter, group, truncate, dedup); preserve the existing `cargo test` / `git status` behaviour as registry entries.
- `bin/smdjad/src/executor/mod.rs`: invoke command filtering on the `bash`/`run_command` result before it is returned from `execute_tool` (mirroring the advisory `scan_tool_output` return-path), tee the full output to the vault, and record tokens saved to the cost ledger.
- `bin/smdjad/src/` (new module): load and parse `.smedja/filters.toml`, merging user entries over the default set (mirrors `load_security_config` in `security.rs`).
- `crates/smedja-vault/src/`: a recovery namespace for stashed full outputs, retrievable by content hash via the existing `smedja_retrieve` tool path (`retrieve_store` in `executor/mod.rs`).
- `crates/smedja-ingot/src/cost.rs`: tokens-saved accounting recorded alongside the existing `CostEntry` ledger so `smj cost` can surface filtering value.
- README: command-output filtering documented as the text analogue of SmartCrusher, with the bypass env var and recovery path.

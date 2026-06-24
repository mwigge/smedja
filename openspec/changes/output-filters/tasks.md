## 1. Filter strategies in crush.rs (text analogue of SmartCrusher)

- [x] 1.1 Add a failing test in `crates/smedja-adapter/src/crush.rs` for a `smart_filter` strategy that collapses a long `cargo build` to its `error[...]`/`warning:` lines (assert error lines survive, progress lines dropped)
- [x] 1.2 Implement `smart_filter(output, keep_markers) -> String` (generalise `is_cargo_test_noise`/`compress_cargo_test` into a keep-markers predicate)
- [x] 1.3 Add a failing test for a `group` strategy that clusters `git status` entries by directory with a per-group count; implement `group_by_directory(output) -> String`
- [x] 1.4 Add a failing test for a `truncate` strategy that keeps the first N lines and appends an omitted-lines marker naming `smedja_retrieve` (mirror `trim_code_block`); implement `truncate_lines(output, max_lines) -> String`
- [x] 1.5 Add a failing test for a `dedup` strategy that collapses repeated lines into one with an `(×N)` count; implement `dedup_lines(output) -> String`

## 2. Command-keyed filter registry

- [x] 2.1 Add a failing test that a `FilterStrategy` enum round-trips from its string name (`smart-filter`/`group`/`truncate`/`dedup`/`none`); implement `FilterStrategy` with `from_str`/`as_str`
- [x] 2.2 Add a failing test that the registry resolves `cargo build` → smart-filter and `git status` → group, an unknown command → `none`, and a two-token key (`docker build`) wins over the one-token key (`docker`); implement `FilterRegistry` + `resolve(cmd) -> (FilterStrategy, params)`
- [x] 2.3 Add a failing test that the built-in default set covers `git`, `cargo`, `pytest`, `npm`, `docker`, `kubectl`; implement `FilterRegistry::with_defaults()`
- [x] 2.4 Refactor `compress_command_output(cmd, output) -> (String, f32)` to dispatch through `FilterRegistry::with_defaults()` while preserving the existing `cargo test` / `git status` / blank-line behaviour and the existing tests in `crush.rs`
- [x] 2.5 Add a failing test that `SMEDJA_NO_TOOL_COMPRESS=1` returns output verbatim with ratio `1.0` through the registry path (extend `bypass_env_skips_command_compression`); confirm it passes

## 3. .smedja/filters.toml DSL loader

- [x] 3.1 Add a failing test that a missing `.smedja/filters.toml` yields the built-in default registry (mirror `load_security_config` in `bin/smdjad/src/security.rs`)
- [x] 3.2 Add a failing test that a present `filters.toml` parses `[filters.<cmd>] strategy = "..."` entries into `FilterStrategy`/params
- [x] 3.3 Add a failing test that a user entry overrides the default for the same command key (e.g. `cargo` → `none`) and that a two-token user key wins over a one-token key
- [x] 3.4 Implement the loader module `bin/smdjad/src/filters.rs` (`load_filter_registry(workspace) -> FilterRegistry`) merging parsed user entries over `FilterRegistry::with_defaults()`

## 4. Wire filtering into the executor (in-process, no hooks/subprocess)

- [x] 4.1 Add a failing test in `bin/smdjad/src/executor/mod.rs` that a `bash` result for a known command (e.g. `cargo build`) is compressed on the `execute_tool` return path
- [x] 4.2 Implement a `filter_command_output` step invoked on the `bash`/`run_command` arm result (after `run_confined`, before return), keyed on the resolved command string and the loaded registry — JSON results route to `compress_tool_result`, text routes to the command filter (single branch point)
- [x] 4.3 Add a failing test that an unknown command's output is returned unchanged (pass-through), and that the captured exit/success contract is unaffected by filtering; confirm both pass
- [x] 4.4 Add a failing test that `SMEDJA_NO_TOOL_COMPRESS=1` skips executor-side filtering; confirm it passes

## 5. Tee-to-vault recovery

- [x] 5.1 Add a failing test that when filtering reduces output, the full uncompressed text is stashed (vault recovery namespace + `retrieve_store`) addressed by a content hash, and the compressed result carries a trailing marker naming that hash
- [x] 5.2 Implement the tee: stash only when ratio < 1.0, register the hash in the `smedja_retrieve` store, append the recovery marker
- [x] 5.3 Add a failing test that an over-compressed result is recoverable by passing the marker's hash to `smedja_retrieve` (returns the original output); confirm it passes

## 6. Savings accounting on the cost ledger

- [x] 6.1 Add a failing test in `crates/smedja-ingot/src/cost.rs` that tokens-saved are recorded separately from billed `input_tok`/`output_tok` and are queryable per session
- [x] 6.2 Implement the tokens-saved record (`saved = estimate_tokens(original) - estimate_tokens(compressed)`, clamped ≥ 0) and its insert/query
- [x] 6.3 Wire the executor filtering step to record tokens saved via `smedja_memory::estimate_tokens`; add a test that a filtered command contributes a positive tokens-saved figure

## 7. Verify

- [x] 7.1 `cargo fmt --all`
- [x] 7.2 `cargo clippy --workspace --all-targets -- -D warnings -W clippy::pedantic` clean for the touched crates (`smedja-adapter`, `smdjad`, `smedja-ingot`, `smedja-vault`)
- [x] 7.3 `cargo test --workspace` — all green
- [x] 7.4 `openspec validate output-filters --strict` — clean

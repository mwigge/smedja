## 1. Multi-source ledger schema (migration 24)

- [x] 1.1 Add a failing `smedja-ingot` test asserting a fresh DB has a `source` column on `tokens_saved_ledger` (query `PRAGMA table_info(tokens_saved_ledger)`)
- [x] 1.2 Add migration `(24, "ALTER TABLE tokens_saved_ledger ADD COLUMN source TEXT NOT NULL DEFAULT 'filter'; CREATE INDEX IF NOT EXISTS idx_tokens_saved_source ON tokens_saved_ledger(source);")` to `MIGRATIONS` in `crates/smedja-ingot/src/lib.rs`
- [x] 1.3 Add a failing test asserting a legacy row (inserted without `source`) backfills to `source = 'filter'` after migration
- [x] 1.4 Run `cargo test -p smedja-ingot`; confirm both schema tests pass

## 2. Source-tagged TokensSavedEntry

- [x] 2.1 Add a failing `cost.rs` test that inserts entries with sources `filter` and `crusher` and asserts `session_tokens_saved_by_source` returns per-source sums
- [x] 2.2 Add `source: String` to `TokensSavedEntry` in `crates/smedja-ingot/src/cost.rs`
- [x] 2.3 Update `insert_tokens_saved` to write the `source` column
- [x] 2.4 Add `session_tokens_saved_by_source(conn, session_id) -> Vec<(String, i64)>` and the `Ingot` wrapper method; keep `session_tokens_saved` as the all-source total
- [x] 2.5 Run `cargo test -p smedja-ingot`; fix until green

## 3. Attribute the existing savers

- [x] 3.1 Add a failing executor test asserting a filtered command writes a row with `source = "filter"`
- [x] 3.2 In `bin/smdjad/src/executor/mod.rs`, set `source = "filter"` on the `record_tokens_saved` entry, and tag the JSON `SmartCrusher` path with `source = "crusher"` (record its estimated saving on that path)
- [x] 3.3 In `crates/smedja-memory/src/`, when cold-stratum turns are omitted, record the dropped-token estimate with `source = "cold-context"` (advisory; swallow errors)
- [x] 3.4 Run `cargo test -p smdjad`; fix until green

## 4. Cache reads as savings (source = "cache")

- [x] 4.1 Add a failing orchestrator test asserting a turn reporting `cache_read_input_tokens = N` writes a ledger row with `source = "cache"` and `tokens_saved = N`
- [x] 4.2 In the orchestrator cache wiring (`bin/smdjad/src/orchestrator.rs`), read the provider-reported `gen_ai.usage.cache_read_input_tokens` (`smedja_telemetry::CACHE_READ_TOKENS`) and write one `source = "cache"` ledger row per turn (skip when zero; swallow errors)
- [x] 4.3 Run `cargo test -p smdjad`; fix until green

## 5. Savings rollup + efficiency ratio

- [x] 5.1 Add a failing test asserting the savings rollup groups `tokens_saved` by `(tier, bucket_start, source)` over a `Daily` tier, reusing `RollupTier::bucket_start`
- [x] 5.2 Implement the savings rollup (parallel to `metrics_rollup::compute`, keyed on `(bucket_start, source)`) in `crates/smedja-ingot/src/` and expose an `Ingot::savings_rollup(tier, since, until)` method
- [x] 5.3 Add a failing test asserting `efficiency_ratio = saved / (saved + billed_input)` over a tier, with `billed_input` summed from `cost_ledger`
- [x] 5.4 Implement the efficiency ratio computation and `Ingot::efficiency_ratio(tier, since, until)`
- [x] 5.5 Add a failing test asserting the headline keeps `cache` savings separate from compression savings (`filter`+`crusher`+`cold-context`) — they are not summed into one compression total
- [x] 5.6 Implement the headline split (compression total vs cache total) and make the test pass
- [x] 5.7 Run `cargo test -p smedja-ingot`; fix until green

## 6. Shared backend + CLI + TUI panel surfaces

- [x] 6.1 Add the `savings.summary` RPC handler in `smdjad` returning per-source buckets + efficiency ratio + the compression/cache split (the shared backend for all three surfaces)
- [x] 6.2 CLI companion: add a failing `smj` arg-parse test for `Cmd::Savings { tier, since, until, json }` (mirroring `Cmd::Metrics`), then add `Cmd::Savings` in `bin/smj/src/main.rs` calling `savings.summary` and rendering a per-source table + efficiency-ratio headline with `--json`
- [x] 6.3 TUI panel: add a failing pure-mapper test for `savings.summary` JSON → savings rows; render a savings-by-source section + efficiency-ratio headline in `bin/smedja-tui/src/metrics_view.rs` beside the existing cost/usage rows, keeping compression and cache totals as separate figures
- [x] 6.4 TUI panel: fetch `savings.summary` on the same poll path `metrics-live-fetch` establishes (do not add a second cadence); show an empty (not stale) savings section when there is no data
- [x] 6.5 Run `cargo test -p smj -p smedja-tui`; fix until green

> The TUI panel depends on `metrics-live-fetch` for the poll loop; if applied first, add a minimal fetch here and let `metrics-live-fetch` converge the cadence.

## 7. Status-bar efficiency segment (cross-stack — stageable as a follow-up)

- [x] 7.1 Add a cumulative efficiency/tokens-saved field to `AgentEvent`/`AgentEventEnvelope` in `crates/smedja-agent-events`; bump `CURRENT_SCHEMA_VERSION`; add a round-trip serde test for the new field and backward-compatible deserialisation of the prior version
- [x] 7.2 Emit the figure from the daemon on the relevant agent event (sourced from the savings rollup / `session_tokens_saved_by_source`); accumulate it into `st-agent` state (`term/crates/st-agent/src/lib.rs`) with a test
- [x] 7.3 Add `ModuleContext.efficiency` / `tokens_saved` and a new `EfficiencyModule: StatusModule` in `term/crates/st-statusbar/src/lib.rs` with an `evaluate` test (renders a segment when present; `None` when absent — no misleading zero)
- [x] 7.4 Populate the new `ModuleContext` field and register `EfficiencyModule` in `sb_modules` (`term/bin/smedja/src/main.rs`)
- [x] 7.5 Run `cargo test -p smedja-agent-events -p st-agent -p st-statusbar`; fix until green

## 8. Verify

- [x] 8.1 Run `cargo test --workspace` — all green
- [x] 8.2 Run `cargo clippy -p smedja-ingot -p smdjad -p smj -p smedja-tui -p smedja-agent-events -p st-agent -p st-statusbar --all-targets -- -D warnings -W clippy::pedantic` — clean for the touched code
- [x] 8.3 Run `openspec validate token-economy --strict` — clean

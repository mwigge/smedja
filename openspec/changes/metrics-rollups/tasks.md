## 1. Rollup tier model and schema

- [x] 1.1 Add a failing test for `RollupTier::bucket_start(ts_micros)` covering `raw` (identity), `hourly`, `daily`, `weekly` (ISO Monday 00:00 UTC), and `monthly` (first-of-month 00:00 UTC) truncation at boundary instants
- [x] 1.2 Add `crates/smedja-ingot/src/metrics_rollup.rs` with `RollupTier` (enum + `bucket_start`) and `MetricsBucket { tier, bucket_start, runner, turns, input_tok, output_tok, cost_usd: Microdollars, error_count }`; make the test pass
- [x] 1.3 Add migration 22 to `MIGRATIONS` in `lib.rs`: `CREATE TABLE IF NOT EXISTS metrics_rollups (tier TEXT, bucket_start INTEGER, runner TEXT, turns INTEGER, input_tok INTEGER, output_tok INTEGER, cost_usd INTEGER, error_count INTEGER, PRIMARY KEY(tier, bucket_start, runner))` plus indices `idx_cost_ledger_created_at` on `cost_ledger(created_at)` and `idx_audit_events_ts_status` on `audit_events(ts, status)`
- [x] 1.4 Add a test asserting `Ingot::open_in_memory()` succeeds with migration 22 applied and the table is present

## 2. On-read aggregation over the ingot

- [x] 2.1 Add a failing test: insert cost entries for two runners across two days, then `metrics_rollup(RollupTier::Daily, since, until)` returns one bucket per `(day, runner)` with exact summed `turns`/`input_tok`/`output_tok`/`cost_usd`
- [x] 2.2 Implement the `cost_ledger` aggregation in `metrics_rollup.rs`: `GROUP BY` the tier bucket expression and `runner`, summing `turns` (COUNT), `input_tok`, `output_tok`, `cost_usd` (micros via `read_micros`); bound by `since`/`until`
- [x] 2.3 Add a failing test: insert audit events with `status = 'error'` for a runner on a day, assert the matching daily bucket reports the correct `error_count`
- [x] 2.4 Implement the `audit_events` error aggregation: `GROUP BY` the tier bucket expression over `ts` and `runner`-equivalent (`tier`/`agent_name`/`actor` per the audit schema), counting rows `WHERE status = 'error'`
- [x] 2.5 Add a failing test: a cost row and an error event at the same instant share one `(bucket_start, runner)` `MetricsBucket`; implement the Rust merge of the two grouped result sets keyed on `(bucket_start, runner)`
- [x] 2.6 Expose `Ingot::metrics_rollup(tier, since, until) -> Result<Vec<MetricsBucket>, IngotError>` in `lib.rs`, ordered by `bucket_start` then `runner`; re-export `MetricsBucket` and `RollupTier`

## 3. Optional materialisation

- [x] 3.1 Add a failing test: `materialise_rollups(tier, until)` then reading `metrics_rollups` yields rows equal to `metrics_rollup(tier, since, until)` for the same range
- [x] 3.2 Implement `Ingot::materialise_rollups(tier, until)` upserting computed buckets via `INSERT … ON CONFLICT(tier, bucket_start, runner) DO UPDATE`
- [x] 3.3 Add a failing test asserting materialisation is idempotent: running it twice leaves identical row count and values; confirm it passes

## 4. metrics.summary RPC

- [x] 4.1 Add `bin/smdjad/src/handlers/metrics.rs` with `summary(state, params)` reading `tier`, `since`, optional `until` (missing `tier`/`since` → `missing_param`), calling `Ingot::metrics_rollup` via `spawn_blocking`, returning `{ tier, buckets: [...] }` with `cost_usd` as USD f64 (display boundary)
- [x] 4.2 Register the handler module in the smdjad handlers module list and `router.register("metrics.summary", …)` in `main.rs` beside `session.cost`
- [x] 4.3 Add a handler test: `metrics.summary` with a populated in-memory ingot returns the expected buckets; missing `tier` returns the `missing_param` error

## 5. smj metrics command

- [x] 5.1 Add a `Metrics { tier, since, until, runner, json }` variant to the `smj` `Cmd` enum mirroring `Cost`
- [x] 5.2 Implement the `Cmd::Metrics` branch: connect, call `metrics.summary`, render a per-runner-over-buckets table (bucket, runner, turns, input, output, cost, errors) or `--json`, mirroring the `Cmd::Cost` render
- [x] 5.3 Add a test (or `--json` snapshot assertion) that the command formats a known `metrics.summary` response into the expected rows

## 6. TUI metrics view

- [x] 6.1 Add a `metrics_view` module (beside `statusbar.rs`/`context_rail.rs`) with a render function over a cached `metrics.summary` snapshot (per-runner tokens/cost/errors for the latest window), following the read-only `ContextRail` pattern
- [x] 6.2 Add a visibility toggle in the app state (mirroring `context_rail_visible`) and wire a keybinding to show/hide the metrics view
- [x] 6.3 Add a render test asserting the panel shows per-runner cost, tokens, and error counts for a sample snapshot

## 7. Documentation: local rollups vs external OTel

- [x] 7.1 Document `smj metrics`, the five tiers, and `metrics.summary` in the README
- [x] 7.2 Add a README note distinguishing local rollups (ingot, offline) from the `smedja-sre` Prometheus/SigNoz path (external backend), stating they are complementary

## 8. Verify

- [x] 8.1 Run `cargo test -p smedja-ingot -p smdjad` — all green
- [x] 8.2 Run `cargo test --workspace` — no regressions introduced by the new command / TUI view
- [x] 8.3 Run `cargo clippy -p smedja-ingot -p smdjad -p smj -- -D warnings` — clean for the touched code
- [x] 8.4 Run `openspec validate metrics-rollups --strict` — clean

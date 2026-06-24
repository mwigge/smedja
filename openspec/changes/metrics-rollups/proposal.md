## Why

smedja persists a precise cost ledger (`cost_ledger` → `CostEntry`) and an immutable audit log (`audit_events` → `AuditEvent`) in `smedja-ingot`, and emits OTel spans through `smedja-telemetry`/`smedja-agent-events`. But the only local aggregation surfaces are session-scoped: `Ingot::session_cost` / `session_cost_entries` (`cost.rs`) sum a single session, and `Ingot::recent_conversations` (`ConversationRollup` in `lib.rs`) rolls up per-conversation counters. There is **no time-tiered rollup** and **no live metrics view** spanning sessions and runners over time.

The Go predecessor (milliways) shipped a SQLite metrics store with rollup tiers — raw / hourly / daily / weekly / monthly — and a live `/metrics` dashboard showing tokens, cost, and errors per runner across time windows. smedja lost that surface in the rewrite. Today an operator who wants "tokens and spend per runner over the last 24h, and which runner is erroring" has to either query an external OTel backend through `smedja-sre` (`metric_query` against Prometheus) — which most local installs do not run — or hand-roll SQL against `cost_ledger`/`audit_events`.

This change adds **time-tiered metrics rollups** computed locally over the existing ingot data, plus a way to view them: a `smj metrics` subcommand (mirroring the existing `smj cost` and `smj timeline` commands) and a TUI metrics view. It is purely additive — it reads the cost ledger and audit log that smdjad already writes; it does not change how either is produced.

## What Changes

- **New `metrics_rollups` table + tiering** in `smedja-ingot`: a single table keyed by `(tier, bucket_start, runner)` holding aggregated `turns`, `input_tok`, `output_tok`, `cost_usd` (microdollars), and `error_count`. Tiers are `raw` (per-entry passthrough granularity), `hourly`, `daily`, `weekly`, `monthly`. A version-gated migration appends the table and its index (the next entry after migration 21).
- **On-read rollup aggregation** over `cost_ledger` (tokens, cost, turns per runner) and `audit_events` (`status = 'error'` counts per runner): a new `Ingot::metrics_rollup(tier, since, until)` returns `Vec<MetricsBucket>` by bucketing source rows into the requested tier's time grid with SQL `GROUP BY`. No background writer is introduced; buckets are computed from source-of-truth rows at query time.
- **Optional materialisation**: `Ingot::materialise_rollups(tier, until)` upserts computed buckets into `metrics_rollups` so a future caller can read pre-aggregated rows. Materialisation is idempotent (upsert on `(tier, bucket_start, runner)`) and never invented — it only ever stores what the on-read aggregation computes.
- **New `metrics.summary` RPC** in smdjad (registered beside `session.cost` in `main.rs`, handled in a new `handlers/metrics.rs`): given `tier`, `since`, and optional `until`, returns the rolled-up buckets as JSON.
- **New `smj metrics` subcommand** mirroring `smj cost`: `smj metrics --tier daily --since 7d [--runner …] [--json]` renders a per-runner table over time buckets (tokens, cost, errors), calling `metrics.summary`.
- **New TUI metrics view** alongside the status bar / context rail (`smedja-tui`): a toggleable panel showing the latest rollup window (tokens, cost, errors per runner) sourced from `metrics.summary`.
- **Clarify external-OTel vs local rollups**: `smedja-sre::metric_query` (Prometheus range query) remains the path for installs running an external OTel backend; local rollups are the always-available, zero-dependency path over the ingot. The two are documented as complementary, not overlapping.

Out of scope: changing the cost/audit write path or schema of `cost_ledger`/`audit_events`; emitting new OTel metrics instruments; a long-running rollup daemon/cron; per-session context-window metrics (owned by the token-snapshot path); retention/pruning of the source ledger.

## Capabilities

### New Capabilities

- `metrics-rollups`: time-tiered (raw / hourly / daily / weekly / monthly) aggregation of tokens, cost, turns, and error counts per runner over the existing cost ledger and audit log, exposed through an ingot API, a `metrics.summary` RPC, a `smj metrics` command, and a TUI metrics view, with optional idempotent materialisation into a `metrics_rollups` table.

## Impact

- `crates/smedja-ingot/src/lib.rs`: append migration 22 (`CREATE TABLE metrics_rollups` + index); add `Ingot::metrics_rollup` and `Ingot::materialise_rollups`; re-export the new types.
- `crates/smedja-ingot/src/metrics_rollup.rs` (new): `MetricsBucket`, `RollupTier`, the bucketing SQL over `cost_ledger`/`audit_events`, and the upsert.
- `bin/smdjad/src/handlers/metrics.rs` (new): `metrics::summary` handler.
- `bin/smdjad/src/handlers/mod.rs` (or the handlers module list): register the `metrics` handler module.
- `bin/smdjad/src/main.rs`: `router.register("metrics.summary", …)` beside `session.cost`.
- `bin/smj/src/main.rs`: add the `Metrics` subcommand variant and its render branch mirroring `Cmd::Cost`.
- `bin/smedja-tui/src/`: a new metrics view module plus a toggle in the app state, beside `statusbar.rs` / `context_rail.rs`.
- README: document `smj metrics`, the rollup tiers, and the local-rollups-vs-external-OTel distinction.

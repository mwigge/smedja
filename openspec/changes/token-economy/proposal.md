## Why

smedja already ships roughly five distinct token savers — command-output filtering, the `SmartCrusher` tool-result compressor, cold-context omission, provider prompt caching, and lean specs — but they are siloed with no unified accounting. The only persisted ledger is `tokens_saved_ledger` (`crates/smedja-ingot/src/lib.rs:177`), and only the output-filter path writes to it (`bin/smdjad/src/executor/mod.rs:209` `record_tokens_saved`). The table has no `source` column (`id, session_id, turn_n, command, tokens_saved, created_at`), so even when other savers do write, there is no way to attribute a saving to the lever that produced it.

Meanwhile a separate `metrics-rollups` engine (`crates/smedja-ingot/src/metrics_rollup.rs`) trends billed tokens/cost per runner over five time tiers but knows nothing about savings, and provider cache-read telemetry (`CACHE_READ_TOKENS` = `gen_ai.usage.cache_read_input_tokens`, `crates/smedja-telemetry/src/lib.rs:42`) is observed but never recorded as a saving. The result: efficiency cannot be measured, so it cannot be improved.

This change unifies the savers into a single **token economy**: every saver writes a `source`-tagged row to one ledger, provider cache reads are recorded as savings, and a rollup trends savings-by-source with an efficiency ratio as the headline. Because savings become attributed and trended, the system (and the user) can see which lever pays and tune the weak ones, closing the loop with `cache-aligner` / `lean-specs` (umbrella-in-cached-prefix → cache reads → recorded savings = proof the strategy pays).

## What Changes

- **Multi-source ledger (MODIFIED)**: add a `source TEXT NOT NULL` column to `tokens_saved_ledger` (values: `filter`, `crusher`, `cold-context`, `cache`, `lean-spec`, …) via a new migration (version 24, after the current max of 23). Every saver writes a tagged row; existing rows backfill to `source = 'filter'`. `TokensSavedEntry` gains a `source` field; `insert_tokens_saved` writes it; a new `session_tokens_saved_by_source` returns per-source sums.
- **Cache reads ARE savings (NEW)**: record provider-reported `gen_ai.usage.cache_read_input_tokens` (`CACHE_READ_TOKENS`) as `source = 'cache'` rows. This is the biggest, most measurable lever. NOTE the philosophical point: cache-reads are "input not re-paid" rather than content compressed away, so they MUST be labelled distinctly and the headline MUST NOT fold `cache` savings into compression savings without distinguishing them.
- **Savings rollup + efficiency ratio (NEW)**: aggregate tokens-saved by `source` over the same five time tiers as `metrics_rollup`, and compute an efficiency ratio `saved / (saved + billed_input)` as the headline trend. Three surfaces over one shared `savings.summary` backend: an always-on **`st-statusbar` efficiency segment** (primary glanceable gauge), the per-source breakdown in the **TUI metrics panel** (riding `metrics-live-fetch`'s poll), and a **`smj savings`** CLI companion. The status-bar segment requires a `smedja-agent-events` schema bump + `st-agent`/terminal plumbing (cross-stack; stageable as a follow-up).
- **Feedback loop**: expose the per-source breakdown so the weak levers are visible; optionally feed the efficiency ratio to the eval-harness as a token-efficiency metric so regressions are caught (referenced, owned by `eval-harness`).

## Capabilities

### New Capabilities

- `cache-savings`: the orchestrator records provider-reported cache-read tokens (`gen_ai.usage.cache_read_input_tokens`) onto the savings ledger as `source = 'cache'`, distinct from compression savings.
- `savings-rollup`: a time-tiered rollup aggregates savings by source over the five `RollupTier` tiers and computes an efficiency ratio `saved / (saved + billed_input)`, surfaced as an always-on `st-statusbar` efficiency segment (primary), a per-source breakdown in the TUI metrics panel, and a `smj savings` CLI companion; the headline keeps cache savings distinct from compression savings.

### Modified Capabilities

- `savings-ledger`: `tokens_saved_ledger` gains a `source` discriminator; every saver (filter, crusher, cold-context, cache, lean-spec) writes a source-tagged row, and savings are queryable per source. Replaces the single-source, untagged ledger that only the output-filter path wrote.

## Impact

- `crates/smedja-ingot/src/lib.rs`: add migration `(24, ...)` (`ALTER TABLE tokens_saved_ledger ADD COLUMN source TEXT NOT NULL DEFAULT 'filter';` + a source index); add `SavingsBucket`/`SavingsRollupTier` wiring and `Ingot` methods (`session_tokens_saved_by_source`, `savings_rollup`, `efficiency_ratio`).
- `crates/smedja-ingot/src/cost.rs`: `TokensSavedEntry` gains `source`; `insert_tokens_saved` writes it; add `session_tokens_saved_by_source`.
- `crates/smedja-ingot/src/metrics_rollup.rs` (or a parallel `savings_rollup.rs`): aggregate savings by `(tier, bucket_start, source)`, reusing `RollupTier::bucket_start`.
- `bin/smdjad/src/executor/mod.rs`: `record_tokens_saved` tags `source = "filter"` (crusher path tags `source = "crusher"`).
- `bin/smdjad/src/orchestrator.rs` (cache wiring): record `cache_read_input_tokens` as a `source = "cache"` ledger row per turn.
- `bin/smdjad/src/handlers/`: add a `savings.summary` RPC (shared backend for all three surfaces).
- `bin/smj/src/main.rs`: add `Cmd::Savings` calling `savings.summary`, mirroring `Cmd::Cost` / `Cmd::Metrics`.
- `bin/smedja-tui/src/metrics_view.rs`: add a savings-by-source section + efficiency-ratio headline, fed by the `metrics-live-fetch` poll loop.
- `crates/smedja-memory/src/`: cold-context omission writes a `source = "cold-context"` row when context is dropped.
- **Status-bar segment (cross-stack — stageable):** `crates/smedja-agent-events/src/lib.rs` (schema bump: a cumulative efficiency/tokens-saved field on `AgentEvent`/`AgentEventEnvelope`, `CURRENT_SCHEMA_VERSION`); `term/crates/st-agent/src/lib.rs` (accumulate it into state); `term/crates/st-statusbar/src/lib.rs` (new `EfficiencyModule` + `ModuleContext.efficiency`/`tokens_saved`); `term/bin/smedja/src/main.rs` (populate the context field, register the module in `sb_modules`).
- README / observability docs: savings are now attributed, trended, and surfaced.

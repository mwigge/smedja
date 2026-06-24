## Why

The `metrics-rollups` change shipped the local time-tiered rollup stack: a `metrics.summary` RPC (`bin/smdjad/src/handlers/metrics.rs:32`) over the cost ledger and audit log, and a read-only TUI metrics panel (`bin/smedja-tui/src/metrics_view.rs`) toggled with Ctrl-T. The panel is wired to render from a cached snapshot — `metrics_snapshot: Vec<metrics_view::MetricsRow>` (`bin/smedja-tui/src/main.rs:247`) — and the toggle flips `metrics_view_visible` (`bin/smedja-tui/src/main.rs:245`) at `bin/smedja-tui/src/main.rs:2089`, with the render block at `bin/smedja-tui/src/main.rs:2668`.

But `metrics_snapshot` defaults empty (`Vec::new()` at the two construction sites, `:2874`-area and `:4018`-area) and is **never populated**. Nothing in the TUI ever calls `metrics.summary` — `grep` for the RPC name in `bin/smedja-tui` returns nothing. So Ctrl-T opens a panel that always renders the `"(no metrics)"` placeholder (`metrics_view.rs:55`), regardless of how much cost/audit data the ingot holds.

This is a documented narrowing left open by `metrics-rollups`: that change built the panel and the RPC but stopped short of wiring the fetch. The panel is blank without it. This change finishes that narrowing — it adds a live fetch that calls `metrics.summary`, maps the result into `Vec<MetricsRow>`, and populates the snapshot, so the panel shows real per-runner tokens / cost / errors.

## What Changes

- **Add a metrics poll cadence field**: introduce `last_metrics_poll: Option<std::time::Instant>` to the TUI app state, mirroring the existing `last_poll` (`bin/smedja-tui/src/main.rs:221`) and `last_cowork_poll` (`bin/smedja-tui/src/main.rs:296`) precedent. Default `None` at every construction site.
- **Fetch once on toggle-on**: when Ctrl-T toggles the panel visible (`bin/smedja-tui/src/main.rs:2089`), clear `last_metrics_poll` so the next event-loop tick fetches immediately, giving instant feedback rather than waiting a full interval for first paint.
- **Periodic refresh while visible**: add a poll branch in the event loop beside the `cowork.pending` poll (`bin/smedja-tui/src/main.rs:3232`). When `metrics_view_visible` and a slow interval (~3s) has elapsed since `last_metrics_poll`, call `metrics.summary { tier: "hourly", since: <now − 24h> }`, map the `buckets` array into `Vec<MetricsRow>` aggregated per runner, and replace `metrics_snapshot`. Metrics are aggregates, not live deltas, so the interval is deliberately slow.
- **A pure JSON→rows mapper**: a free function (e.g. `metrics_rows_from_summary(&Value) -> Vec<MetricsRow>`) that folds the RPC `buckets` (`{runner, input_tok, output_tok, cost_usd, error_count, …}` — see `bin/smdjad/src/handlers/metrics.rs:74`) into one `MetricsRow` per runner (summing `input_tok + output_tok` into `tokens`, `cost_usd`, and `error_count`). Pure and unit-testable with no client, no I/O.
- **Empty result clears, never goes stale**: an empty `buckets` array maps to an empty `Vec<MetricsRow>`, which the render path already shows as `"(no metrics)"`. The snapshot is always replaced, never merged, so a window with no data shows an empty panel rather than the previous window's rows.
- **Never block the render**: the fetch runs in the async event loop exactly like the existing `cowork.pending` / turn polls — it is the only place the RPC is awaited, the cadence is slow, and the result only mutates the cached `metrics_snapshot`. The render block (`bin/smedja-tui/src/main.rs:2668`) continues to read the cached snapshot and never fetches.

Out of scope: the rollup aggregation, the `metrics.summary` RPC, the `MetricsRow`/`MetricsView` widget, and the `smj metrics` CLI command — all owned by the shipped `metrics-rollups` change. Tier/window selection UI (the tier and since window are fixed defaults here). The token-economy savings trend (a sibling proposal) — this change only ensures the snapshot/fetch shape can absorb a savings row set later without rework.

## Capabilities

### New Capabilities

- `live-metrics-fetch`: the TUI metrics panel is backed by a live fetch — opening it calls `metrics.summary` once for immediate feedback, then refreshes on a slow interval while visible, mapping the RPC buckets into the per-runner snapshot the panel renders, off the render hot path.

## Impact

- `bin/smedja-tui/src/main.rs`: add `last_metrics_poll` field; clear it on the Ctrl-T toggle (`:2089`); add a `metrics.summary` poll branch in the event loop beside the cowork poll (`:3232`); add the pure `metrics_rows_from_summary` mapper. Default `last_metrics_poll: None` at all construction sites.
- `bin/smedja-tui/src/metrics_view.rs`: unchanged (the snapshot/row shape is designed for reuse; a savings row set is a later additive change).
- README: note that the Ctrl-T metrics panel now shows live per-runner rollups (it previously documented a panel that rendered empty).

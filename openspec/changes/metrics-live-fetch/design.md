## Context

The `metrics-rollups` change shipped a complete read surface but left the TUI fetch unwired. The relevant surface:

- `metrics.summary` RPC (`bin/smdjad/src/handlers/metrics.rs:32`): params `tier` (required, one of `raw`/`hourly`/`daily`/`weekly`/`monthly`), `since` (required, micros since the Unix epoch), `until` (optional, defaults to now). Result is `{ "tier": <str>, "buckets": [ … ] }` where each bucket is `{ bucket_start, runner, turns, input_tok, output_tok, cost_usd (f64 USD), error_count }` (`metrics.rs:74`). Note: buckets are per `(bucket_start, runner)`, so one runner can appear in several buckets across a multi-bucket window.
- `MetricsRow { runner, tokens: i64, cost_usd: f64, errors: i64 }` and `MetricsView` (`bin/smedja-tui/src/metrics_view.rs:18`): a read-only widget that renders one row per runner from a snapshot, or `"(no metrics)"` when the snapshot is empty (`metrics_view.rs:54`). It never fetches.

The TUI side:

- App state holds `metrics_view_visible: bool` (`bin/smedja-tui/src/main.rs:245`) and `metrics_snapshot: Vec<metrics_view::MetricsRow>` (`:247`). The snapshot is constructed as `Vec::new()` and never written.
- Ctrl-T toggles visibility (`:2089`): `state.metrics_view_visible = !state.metrics_view_visible;`.
- The render block (`:2668`) clones `metrics_snapshot` into a `MetricsView` each frame — pure read, no fetch.
- The event loop already runs async polls keyed by `Instant`: turns via `last_poll` (`:221`, due-check at `:3117` with a 50 ms interval) and `cowork.pending` via `last_cowork_poll` (`:296`, due-check at `:3234` with a 200 ms interval, gated on a turn being in flight). The client call shape is `client.call("<method>", json!({ … })).await` (e.g. `:3240`).

## Goals / Non-Goals

Goals:
- Populate `metrics_snapshot` from a live `metrics.summary` call so the Ctrl-T panel shows real per-runner data.
- Fetch once on toggle-on for immediate feedback; refresh on a slow interval while the panel is visible.
- Keep the fetch off the render hot path, mirroring the existing async-poll pattern.
- Map the RPC buckets into the snapshot with a pure, unit-testable function.
- Shape the snapshot/fetch so a future savings row set (token-economy sibling) can be added without rework.

Non-Goals:
- Changing the rollup aggregation, the `metrics.summary` RPC, or the `MetricsRow`/`MetricsView` widget (owned by `metrics-rollups`).
- User-selectable tier or time window — tier and since-window are fixed defaults here.
- A long-lived background fetch task or any new thread — the existing event-loop poll dispatch is sufficient.
- Persisting the snapshot across restarts — it is re-fetched on demand, matching the cowork/turn poll model.
- Rendering the token-economy savings trend — only the extensibility is in scope, not the rows.

## Decisions

**Decision: mirror the existing poll cadence with a `last_metrics_poll: Option<Instant>` field.**
Add `last_metrics_poll: Option<std::time::Instant>` to app state alongside `last_poll` and `last_cowork_poll`, defaulting `None` at every construction site. The event loop gains a poll branch beside the cowork poll: when `metrics_view_visible` and `last_metrics_poll.is_none_or(|t| t.elapsed() >= Duration::from_secs(3))`, set `last_metrics_poll = Some(Instant::now())`, call `metrics.summary`, and replace the snapshot. On toggle-on (Ctrl-T flipping visibility to true), set `last_metrics_poll = None` so the next tick fetches immediately rather than waiting a full interval.
- Rationale: reuses the proven `Instant`-keyed due-check pattern already used for turns (50 ms) and cowork (200 ms); no new task, thread, or shared mutable state.
- Cadence / tier / window defaults: interval **~3 s** (`Duration::from_secs(3)`) — metrics are aggregates, not live deltas, so a slow refresh is correct and cheap; tier **`"hourly"`** — fine-grained enough to show recent movement without the per-entry noise of `raw`; window **last 24h** (`since = Timestamp::now().as_micros() − 24 * 3_600 * 1_000_000`), giving a full day of hourly buckets. `until` is omitted so the RPC defaults it to now.
- Alternative: poll always (even when hidden). Rejected — wasted RPCs; the panel is hidden by default and the data is only consumed when visible.

**Decision: a pure JSON→rows mapper, separate from the fetch.**
Add a free function `metrics_rows_from_summary(resp: &serde_json::Value) -> Vec<metrics_view::MetricsRow>` that reads `resp["buckets"]`, folds buckets by `runner` (summing `input_tok + output_tok` into `tokens`, accumulating `cost_usd` and `error_count`), and returns the aggregated rows in first-seen runner order. Missing/!array `buckets` yields an empty `Vec`. The poll branch is the only caller; it does `state.metrics_snapshot = metrics_rows_from_summary(&resp);`.
- Rationale: the mapping is the part with edge cases (multi-bucket runners, empty result, malformed JSON); isolating it as a pure function makes it directly unit-testable with no client, socket, or async — the same testability the daemon's `summary_with` enjoys (`metrics.rs:42`).
- Aggregation is per runner (not per bucket) because the panel shows one row per runner; an hourly 24h window can return up to 24 buckets per runner, which must collapse to a single row.
- Alternative: map one row per bucket. Rejected — the `MetricsView` is a per-runner table, not a time series; collapsing matches its semantics.

**Decision: always replace the snapshot, never merge — empty result clears it.**
Each fetch assigns `state.metrics_snapshot = …` outright. An empty `buckets` array maps to an empty `Vec`, which the render path already shows as `"(no metrics)"` (`metrics_view.rs:55`). The previous window's rows are never retained.
- Rationale: a stale panel is worse than an honestly-empty one; replace-not-merge guarantees the panel reflects only the current window. The fetch never blocks the render — it only writes the cached field the render later reads.

**Tie-in (forward-looking, not built here): savings trend extensibility.**
The token-economy sibling proposal will surface a savings trend on this same panel. The mapper returns a fresh `Vec<MetricsRow>` and the fetch replaces the snapshot wholesale, so a later change can either (a) extend `metrics_rows_from_summary` to append savings-derived rows, or (b) add a parallel snapshot field and a second mapper, without touching the poll/cadence wiring introduced here. Keeping the mapper pure and the fetch a thin replace-the-snapshot step is what makes that additive.

## Risks / Trade-offs

- [Risk] A 3 s poll while the panel is open adds RPC load → Mitigation: the panel is hidden by default and gated on `metrics_view_visible`; 3 s is slow and the `metrics.summary` query is a bounded `GROUP BY` over local ingot rows, not a network call.
- [Risk] The fixed `hourly` / 24h defaults may not suit every operator → Mitigation: out of scope here; a tier/window selector is a clean later addition and the mapper is window-agnostic.
- [Risk] Malformed or error RPC responses could panic the mapper → Mitigation: the mapper treats missing/!array `buckets` and missing fields as zeros/empty (no `unwrap`), returning an empty `Vec`; the fetch ignores RPC `Err` and leaves the prior snapshot until the next tick, mirroring the cowork poll's tolerant `if let Ok(...)` handling (`:3239`).
- [Risk] Toggling rapidly could fire back-to-back fetches → Mitigation: clearing `last_metrics_poll` only forces one immediate fetch; the 3 s interval then applies, so repeated toggles cost at most one fetch per tick.

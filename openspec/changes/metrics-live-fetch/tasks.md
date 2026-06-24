## 1. Pure JSON→rows mapper (Red → Green)

- [x] 1.1 Add a failing test `metrics_rows_from_summary_folds_buckets_per_runner`: given a `metrics.summary`-shaped `Value` with two buckets for `"claude"` and one for `"local"`, assert the mapper returns two `MetricsRow`s, that `claude.tokens == sum(input_tok + output_tok)` across its buckets, and that `cost_usd`/`errors` accumulate
- [x] 1.2 Add a failing test `metrics_rows_from_summary_empty_buckets_yields_no_rows`: an object with `"buckets": []` (and one with `buckets` missing entirely) maps to an empty `Vec<MetricsRow>`
- [x] 1.3 Add a failing test `metrics_rows_from_summary_tolerates_missing_fields`: a bucket missing `input_tok`/`cost_usd`/`error_count` is treated as zeros, not a panic
- [x] 1.4 Implement the pure `metrics_rows_from_summary(resp: &serde_json::Value) -> Vec<metrics_view::MetricsRow>` in `bin/smedja-tui/src/main.rs`, folding `resp["buckets"]` by runner (first-seen order), with no client/IO; make 1.1–1.3 green

## 2. Poll-due predicate (Red → Green)

- [x] 2.1 Add a failing test `metrics_poll_due_when_visible_and_unset_or_elapsed`: a small pure predicate `metrics_poll_due(visible: bool, last: Option<Instant>, now: Instant)` returns true when `visible` and `last` is `None` or `now - last >= 3s`, and false when hidden or within the interval
- [x] 2.2 Implement `metrics_poll_due` (interval `Duration::from_secs(3)`) and make 2.1 green

## 3. Fetch on toggle (Red → Green)

- [x] 3.1 Add `last_metrics_poll: Option<std::time::Instant>` to the TUI app state next to `last_cowork_poll`; default `None` at every construction site (initial build and any test harness builder)
- [x] 3.2 In the Ctrl-T handler (`bin/smedja-tui/src/main.rs:2089`), when the toggle flips `metrics_view_visible` to true, set `state.last_metrics_poll = None` so the next tick fetches immediately; add a test asserting toggling the panel on resets `last_metrics_poll` to `None`
- [x] 3.3 Compile-check the state changes (`cargo build -p smedja-tui`)

## 4. Periodic refresh in the event loop (Red → Green)

- [x] 4.1 Add a poll branch beside the `cowork.pending` poll (`bin/smedja-tui/src/main.rs:3232`): when `metrics_poll_due(state.metrics_view_visible, state.last_metrics_poll, Instant::now())`, set `last_metrics_poll = Some(now)`, call `metrics.summary { tier: "hourly", since: <now − 24h micros> }`, and on `Ok` set `state.metrics_snapshot = metrics_rows_from_summary(&resp)` (tolerant `if let Ok(...)`, mirroring the cowork poll)
- [x] 4.2 Add a test asserting that applying a non-empty `metrics.summary` response through the mapper produces a non-empty `metrics_snapshot` (live-fetch populates the previously-blank panel), and that an empty response replaces it with an empty snapshot (no stale rows)
- [x] 4.3 Confirm the render block (`:2668`) is unchanged and still reads only the cached snapshot (the fetch never blocks render) — assert by inspection that no `client.call` lives in the render path

## 5. Verify

- [x] 5.1 `cargo test -p smedja-tui` — all green (mapper, predicate, toggle-reset, snapshot-population tests)
- [x] 5.2 `cargo clippy -p smedja-tui -- -D warnings` clean for the touched code
- [x] 5.3 `cargo build --workspace` — no breakage
- [x] 5.4 `openspec validate metrics-live-fetch --strict`

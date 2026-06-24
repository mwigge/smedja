## ADDED Requirements

### Requirement: Opening the metrics panel fetches the rollup snapshot once

When the metrics panel is toggled visible, the TUI SHALL perform a `metrics.summary` fetch on the next event-loop tick — not after a full poll interval — so the panel shows current data immediately. The fetched buckets MUST be mapped into the per-runner snapshot the panel renders.

#### Scenario: toggle-on triggers an immediate fetch

- **WHEN** the user presses Ctrl-T and the metrics panel becomes visible
- **THEN** the metrics poll-due state SHALL be reset so the next event-loop tick is due
- **AND** that tick SHALL call `metrics.summary` and replace the cached snapshot with the mapped result

#### Scenario: non-empty result shows per-runner rows

- **WHEN** the ingot holds cost and audit data and `metrics.summary` returns buckets for one or more runners
- **THEN** the metrics snapshot SHALL contain one row per runner
- **AND** each row's tokens SHALL be the sum of input and output tokens across that runner's buckets, with cost and error counts accumulated likewise

### Requirement: The metrics panel refreshes on a slow interval while visible

While the metrics panel is visible, the TUI SHALL re-fetch `metrics.summary` on a slow interval (approximately 3 seconds) and replace the snapshot, because the rollups are aggregates rather than live deltas. The TUI SHALL NOT fetch while the panel is hidden.

#### Scenario: periodic refresh while visible

- **WHEN** the metrics panel has been visible for at least the refresh interval since the last fetch
- **THEN** the TUI SHALL call `metrics.summary` again and replace the cached snapshot with the newly mapped rows

#### Scenario: no fetch while hidden

- **WHEN** the metrics panel is not visible
- **THEN** the TUI SHALL NOT call `metrics.summary`

### Requirement: The metrics fetch never blocks the render

The metrics fetch SHALL run within the existing asynchronous event-loop poll dispatch, exactly like the `cowork.pending` and turn polls, and SHALL only mutate the cached snapshot. The render path SHALL read only the cached snapshot and SHALL NOT issue any `metrics.summary` call.

#### Scenario: render reads only the cached snapshot

- **WHEN** a render frame draws the metrics panel
- **THEN** it SHALL render from the cached snapshot without performing any fetch
- **AND** an RPC error from the periodic fetch SHALL leave the prior snapshot in place until the next successful fetch

### Requirement: An empty result shows an empty, not stale, panel

The fetch SHALL replace the snapshot wholesale rather than merge into it. When `metrics.summary` returns no buckets for the requested window, the snapshot SHALL become empty so the panel shows its empty-state placeholder rather than rows from a previous window.

#### Scenario: empty window clears the panel

- **WHEN** `metrics.summary` returns an empty `buckets` array
- **THEN** the metrics snapshot SHALL be replaced with an empty row set
- **AND** the panel SHALL render its no-metrics placeholder rather than the previous window's rows

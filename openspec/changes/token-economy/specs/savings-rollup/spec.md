## ADDED Requirements

### Requirement: Savings rollup aggregates by source over time tiers

A savings rollup SHALL aggregate `tokens_saved` grouped by `(tier, bucket_start, source)` over the same five fixed time tiers as the metrics rollup (`raw`/`hourly`/`daily`/`weekly`/`monthly`), reusing `RollupTier::bucket_start` for bucket truncation so savings buckets align exactly with billed buckets. Results SHALL be ordered by `bucket_start` then `source`.

#### Scenario: rollup returns saved-by-source over a tier

- **WHEN** savings rows for sources `filter` and `cache` fall on the same UTC day and a `Daily` rollup is requested over a range covering that day
- **THEN** the rollup SHALL return one bucket per `(day, source)` with the summed `tokens_saved`
- **AND** both buckets SHALL share the same `bucket_start` (the start of that UTC day)

### Requirement: Efficiency ratio is the headline trend

The rollup SHALL expose an efficiency ratio computed as `saved / (saved + billed_input)` over a tier, where `billed_input` is the sum of `cost_ledger.input_tok` over the same time range and `saved` is the sum of `tokens_saved` over the same range. When `saved + billed_input` is zero, the ratio SHALL be zero.

#### Scenario: efficiency ratio computed

- **WHEN** a tier window has `saved = 200` total tokens saved and `billed_input = 800` billed input tokens
- **THEN** the efficiency ratio SHALL equal `200 / (200 + 800)` = `0.2`

#### Scenario: empty window yields zero ratio

- **WHEN** a tier window has no savings and no billed input
- **THEN** the efficiency ratio SHALL be `0`

### Requirement: Efficiency surfaced as a status-bar segment, a metrics panel, and a CLI command

The savings rollup SHALL be surfaced across three surfaces sharing one `savings.summary` backend. The **primary glanceable surface SHALL be an always-on `st-statusbar` segment** (an `EfficiencyModule` rendered each tick) showing the efficiency-ratio headline (and/or cumulative tokens saved) beside the existing tier/model/tokens segments. The **TUI metrics panel** (`bin/smedja-tui/src/metrics_view.rs`) SHALL show the per-source breakdown + ratio alongside cost/usage. A **`smj savings`** command SHALL provide the same rollup as a CLI companion (`--json`). In every surface the headline MUST present compression savings (`filter` + `crusher` + `cold-context`) separately from cache savings (`source = 'cache'`).

The status-bar segment's data path SHALL be: the daemon emits a cumulative efficiency/tokens-saved figure on an agent event (a `smedja-agent-events` schema field), `st-agent` accumulates it, and the `EfficiencyModule` renders it from `ModuleContext`. This cross-stack segment MAY be staged as a follow-up; when staged, the TUI panel and `smj savings` SHALL still satisfy this requirement.

#### Scenario: status-bar segment shows the efficiency headline

- **WHEN** the GPU terminal renders the status bar and a cumulative efficiency figure is available
- **THEN** the `EfficiencyModule` SHALL produce a segment showing the efficiency ratio (and/or tokens saved)
- **AND** when no figure is available the module SHALL produce no segment (it does not render a misleading zero)

#### Scenario: metrics panel shows savings beside cost metrics

- **WHEN** the TUI metrics panel is shown and the savings rollup has data
- **THEN** the panel SHALL display per-source savings and the efficiency-ratio headline alongside the cost/usage rows
- **AND** the compression total and the cache total SHALL be shown as separate figures

#### Scenario: savings command renders per-source breakdown and ratio

- **WHEN** `smj savings` is invoked for a tier with savings present
- **THEN** the output SHALL list each `source` with its summed `tokens_saved`
- **AND** the output SHALL show the efficiency-ratio headline
- **AND** the compression total and the cache total SHALL be shown as separate figures

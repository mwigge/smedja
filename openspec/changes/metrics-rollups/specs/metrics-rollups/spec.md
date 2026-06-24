## ADDED Requirements

### Requirement: Time-tiered rollup aggregation over the cost ledger and audit log

`smedja-ingot` SHALL provide an `Ingot::metrics_rollup(tier, since, until)` operation that aggregates the existing `cost_ledger` and `audit_events` rows into per-runner, per-time-bucket `MetricsBucket` values. The supported tiers MUST be `raw`, `hourly`, `daily`, `weekly`, and `monthly`. Each bucket MUST report exact summed `turns`, `input_tok`, `output_tok`, `cost_usd` (in integer microdollars, converted to USD only at a display boundary), and `error_count`. The operation MUST NOT modify, prune, or summarise away the source rows.

#### Scenario: daily rollup sums cost-ledger rows per runner

- **WHEN** cost-ledger entries exist for two runners across two distinct days within `[since, until]`
- **THEN** `metrics_rollup(daily, since, until)` SHALL return one bucket per `(day, runner)` pair
- **AND** each bucket's `turns`, `input_tok`, `output_tok`, and `cost_usd` SHALL equal the exact sum of the contributing entries
- **AND** the source `cost_ledger` rows SHALL be left unchanged

#### Scenario: error counts come from the audit log

- **WHEN** audit events with `status = "error"` exist for a runner within a bucket's time range
- **THEN** the matching `MetricsBucket` SHALL report `error_count` equal to the number of those error events

#### Scenario: cost and error facts merge on the same bucket key

- **WHEN** a cost-ledger entry and an error audit event for the same runner occur at the same instant
- **THEN** they SHALL be reported in a single `MetricsBucket` for that `(bucket_start, runner)`
- **AND** that bucket SHALL carry both the cost/token totals and the error count

#### Scenario: tier truncation is deterministic and UTC

- **WHEN** `metrics_rollup` buckets a timestamp for the `hourly`, `daily`, `weekly`, or `monthly` tier
- **THEN** the bucket start SHALL be the timestamp floored to the start of the hour, day, ISO week (Monday 00:00 UTC), or month (first-of-month 00:00 UTC) respectively
- **AND** two timestamps in the same tier window SHALL produce the same `bucket_start`

### Requirement: Optional idempotent materialisation of rollups

`smedja-ingot` SHALL provide an `Ingot::materialise_rollups(tier, until)` operation that upserts the computed buckets into a `metrics_rollups` table keyed on `(tier, bucket_start, runner)`. Materialisation MUST be idempotent and MUST store only values that the on-read aggregation produces, so that `metrics_rollups` is a derived cache and never a divergent source of truth.

#### Scenario: materialised rows equal on-read aggregation

- **WHEN** `materialise_rollups(tier, until)` is called and then the `metrics_rollups` rows for that tier are read
- **THEN** the stored rows SHALL equal the result of `metrics_rollup(tier, since, until)` over the same range

#### Scenario: re-materialisation is idempotent

- **WHEN** `materialise_rollups(tier, until)` is called twice over an unchanged ledger
- **THEN** the `metrics_rollups` row count and values SHALL be identical after both calls

### Requirement: metrics.summary RPC exposes rollups

smdjad SHALL register a `metrics.summary` RPC method that returns time-tiered rollups for a requested `tier` and time window. The method MUST require a `tier` and a `since` parameter and MAY accept an `until` parameter. It MUST return the buckets as JSON with `cost_usd` rendered in USD at the response boundary while the underlying aggregation keeps integer microdollars.

#### Scenario: summary returns rolled-up buckets

- **WHEN** `metrics.summary` is called with a valid `tier` and `since` against a populated ledger
- **THEN** the response SHALL contain the per-runner, per-bucket rollup values for that tier and window

#### Scenario: missing required parameter is rejected

- **WHEN** `metrics.summary` is called without a `tier` (or without `since`)
- **THEN** the call SHALL return a missing-parameter error
- **AND** no rollup SHALL be computed

### Requirement: smj metrics command renders rollups

The `smj` CLI SHALL provide a `metrics` subcommand that calls `metrics.summary` and renders the result, mirroring the existing `cost` subcommand. It MUST accept a `--tier` and a `--since` flag, MAY accept `--until` and `--runner` filters, and MUST support a `--json` output mode.

#### Scenario: human-readable table

- **WHEN** `smj metrics --tier daily --since 7d` is run against a daemon with ledger data
- **THEN** the command SHALL print a per-runner table over time buckets showing tokens, cost, and error counts

#### Scenario: json output

- **WHEN** `smj metrics --tier daily --since 7d --json` is run
- **THEN** the command SHALL emit the `metrics.summary` buckets as JSON

### Requirement: TUI metrics view

The TUI SHALL provide a toggleable, read-only metrics view, alongside the status bar and context rail, that displays per-runner tokens, cost, and error counts for the latest rollup window, sourced from `metrics.summary`.

#### Scenario: metrics view toggles and shows per-runner totals

- **WHEN** the operator toggles the metrics view on with ledger data present
- **THEN** the view SHALL display, per runner, the rolled-up tokens, cost, and error counts for the current window
- **AND** toggling it off SHALL hide the view without affecting the rest of the TUI

### Requirement: Local rollups are distinct from the external OTel path

Local metrics rollups SHALL be computed solely from the ingot SQLite store and MUST function without any external metrics backend. The external OTel query path (`smedja-sre::metric_query` against Prometheus/SigNoz) SHALL remain available and documented as the complementary surface for installs that run an external backend; neither path SHALL be reimplemented in terms of the other.

#### Scenario: rollups work offline

- **WHEN** no Prometheus or SigNoz backend is configured or reachable
- **THEN** `metrics.summary`, `smj metrics`, and the TUI metrics view SHALL still return rollups computed from the ingot

#### Scenario: external OTel path remains for infra metrics

- **WHEN** documentation describes the metrics surfaces
- **THEN** it SHALL state that local rollups cover smedja's own cost/token/error ledger and the `smedja-sre` OTel path covers external infra metrics, and that they are complementary

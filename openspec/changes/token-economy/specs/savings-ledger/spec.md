## ADDED Requirements

### Requirement: Savings ledger carries a source discriminator

`tokens_saved_ledger` SHALL carry a `source` column identifying the saver that produced each row. Permitted values include `filter`, `crusher`, `cold-context`, `cache`, and `lean-spec`. The schema migration MUST default existing rows to `source = 'filter'`, since the output-filter path is the only historical writer. `TokensSavedEntry` SHALL expose a `source` field and `insert_tokens_saved` MUST persist it.

#### Scenario: fresh database has a source column

- **WHEN** a fresh `Ingot` database is opened and all migrations have applied
- **THEN** `tokens_saved_ledger` SHALL have a `source` column that is `NOT NULL`

#### Scenario: legacy rows backfill to filter

- **WHEN** a row predating the migration exists without an explicit `source`
- **THEN** after the migration its `source` SHALL be `'filter'`

### Requirement: Every saver writes a source-tagged row

Each token saver SHALL write its saving to `tokens_saved_ledger` tagged with its own `source`. The output filter SHALL write `source = 'filter'`, the `SmartCrusher` tool-result path SHALL write `source = 'crusher'`, and cold-stratum omission SHALL write `source = 'cold-context'`. Savings recording MUST remain advisory: a ledger error MUST be logged and swallowed and MUST NOT break the tool or turn path.

#### Scenario: filtered command output writes a filter-tagged row

- **WHEN** command-output filtering reduces a result and records a positive saving
- **THEN** a `tokens_saved_ledger` row SHALL be written with `source = 'filter'`
- **AND** the recorded `tokens_saved` SHALL be the clamped estimate of tokens removed

#### Scenario: crusher writes a crusher-tagged row

- **WHEN** the `SmartCrusher` tool-result path reduces a JSON result
- **THEN** the recorded saving SHALL be tagged `source = 'crusher'`

### Requirement: Savings are queryable per source

The ledger SHALL expose a query that returns the sum of `tokens_saved` grouped by `source` for a session, distinct from the all-source total. Savings MUST remain separate from the billed `cost_ledger` input/output totals.

#### Scenario: per-source sums are returned

- **WHEN** a session has savings rows tagged `filter` and `crusher`
- **THEN** the per-source query SHALL return one entry per source with its summed `tokens_saved`
- **AND** the billed `cost_ledger` input/output totals SHALL be unchanged

## ADDED Requirements

### Requirement: Provider cache reads recorded as savings

The orchestrator SHALL record provider-reported `gen_ai.usage.cache_read_input_tokens` (the `CACHE_READ_TOKENS` telemetry key) onto the savings ledger as a row with `source = 'cache'`, written once per turn. The recorded `tokens_saved` MUST equal the provider-reported cache-read count. When the reported count is zero, no row SHALL be written. Recording MUST be advisory — a ledger error MUST be logged and swallowed and MUST NOT break the turn.

#### Scenario: cache-read tokens recorded as source=cache

- **WHEN** a turn completes and the provider reports `cache_read_input_tokens = N` with `N > 0`
- **THEN** a `tokens_saved_ledger` row SHALL be written with `source = 'cache'` and `tokens_saved = N`

#### Scenario: zero cache reads write no row

- **WHEN** a turn completes and the provider reports `cache_read_input_tokens = 0`
- **THEN** no `source = 'cache'` row SHALL be written for that turn

### Requirement: Cache savings are labelled distinctly from compression savings

Cache savings represent input not re-paid, not content compressed away. The system SHALL keep `source = 'cache'` savings categorically distinct from compression savings (`filter`, `crusher`, `cold-context`) wherever savings are aggregated or surfaced, and MUST NOT fold cache savings into a combined compression total.

#### Scenario: headline does not double-count cache as compression

- **WHEN** a session has both cache savings and compression savings
- **THEN** the surfaced compression total SHALL include only `filter`, `crusher`, and `cold-context` rows
- **AND** the cache savings SHALL be presented as a separate figure, not summed into the compression total

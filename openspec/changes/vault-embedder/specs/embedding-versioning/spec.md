## ADDED Requirements

### Requirement: Vault rows record their producing model and dimension

Every vault row SHALL record the `embedder_model_id` and `dim` of the model that produced its embedding. `Vault::insert` and `Vault::upsert` SHALL persist these alongside the embedding BLOB, and reads SHALL surface them on `VaultEntry`. Legacy rows lacking these fields SHALL default to `model_id = "fnv-bow-128"` and a `dim` derived from the stored BLOB length.

#### Scenario: model id and dim round-trip

- **WHEN** an entry tagged with a `model_id` and `dim` is inserted and later read back via `search`
- **THEN** the read entry SHALL carry the same `embedder_model_id` and `dim`

#### Scenario: legacy row defaults to FNV identity

- **WHEN** a row created before this change (no model columns) is read
- **THEN** its `embedder_model_id` SHALL default to `"fnv-bow-128"`
- **AND** its `dim` SHALL equal the stored embedding BLOB length divided by four

### Requirement: Search compares only same-model vectors

`Vault::search` and `Vault::query` SHALL compare the query vector only against rows whose `embedder_model_id` and `dim` match the query's. A row produced by a different model or dimension SHALL be excluded from ranking — it SHALL NOT be passed to cosine comparison and SHALL NOT cause an error.

#### Scenario: mismatched-model rows excluded, not crashed

- **WHEN** a namespace holds rows from two different models and a query is run under one model
- **THEN** only rows matching the query's `model_id` and `dim` SHALL be ranked and returned
- **AND** rows from the other model SHALL be excluded without raising an error

#### Scenario: mismatched-dimension row does not crash the scan

- **WHEN** the vault holds a row whose embedding dimension differs from the query vector's
- **THEN** that row SHALL be skipped during the scan
- **AND** the search SHALL return the same-model results normally rather than erroring

### Requirement: Re-embed/backfill upgrades existing rows

The daemon SHALL provide a re-embed/backfill command that walks existing rows, re-embeds each row's stored content with the active embedder, and rewrites the embedding, `embedder_model_id`, and `dim`. The operation SHALL be idempotent and restartable.

#### Scenario: backfill upgrades FNV rows to the active model

- **WHEN** a backfill runs over a namespace of FNV-tagged rows under an active learned embedder
- **THEN** every row's `embedder_model_id` and `dim` SHALL become the learned model's
- **AND** each row's embedding SHALL be the learned embedding of its stored content
- **AND** subsequent same-model queries SHALL rank those rows

#### Scenario: backfill is idempotent for already-current rows

- **WHEN** a backfill runs over rows already tagged with the active model
- **THEN** those rows SHALL be left semantically unchanged (a no-op rewrite)

## ADDED Requirements

### Requirement: smedja_vault_search returns ranked vault results

The `smedja_vault_search` agent tool SHALL embed its `query` input, run the vault's hybrid cosine search over the requested `namespace` (default `"default"`) limited to `k` results (default `5`), and return a JSON object with a `results` array. Each result entry SHALL carry `id`, `content`, `namespace`, and `payload`, and the entries SHALL be ordered by descending relevance score.

#### Scenario: matching query returns ranked results

- **WHEN** the tool is invoked with a `query` whose terms match stored entries in the namespace
- **THEN** the response SHALL be a JSON object containing a non-empty `results` array
- **AND** each entry SHALL include `id`, `content`, `namespace`, and `payload`
- **AND** the entries SHALL be ordered by descending relevance score

#### Scenario: k bounds the result count

- **WHEN** the tool is invoked with `k` set to a value smaller than the number of matching entries
- **THEN** the `results` array SHALL contain at most `k` entries

### Requirement: smedja_vault_search returns empty results rather than an error on no match

When the vault contains no entry matching the query in the requested namespace, the tool SHALL return a JSON object whose `results` array is empty. An empty vault or an empty namespace SHALL NOT be reported as an error.

#### Scenario: empty vault yields an empty results array

- **WHEN** the tool is invoked against a vault with no matching entries
- **THEN** the response SHALL be a JSON object with an empty `results` array
- **AND** the response SHALL NOT be an error string

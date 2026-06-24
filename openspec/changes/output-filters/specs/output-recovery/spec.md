## ADDED Requirements

### Requirement: Full output teed to the vault for recovery

When command filtering reduces a result, the daemon SHALL stash the full uncompressed output in a vault recovery namespace addressed by a content hash, and SHALL register that hash with the `smedja_retrieve` store. The compressed result MUST carry a trailing marker naming the recovery hash.

#### Scenario: filtered output is stashed with a recovery marker

- **WHEN** a command's output is filtered to a smaller result
- **THEN** the full uncompressed output SHALL be stored under the recovery namespace addressed by a content hash
- **AND** the compressed result returned to the caller SHALL include a marker naming that hash

#### Scenario: unreduced output is not stashed

- **WHEN** filtering does not reduce the output (ratio is 1.0)
- **THEN** no recovery entry SHALL be created
- **AND** no recovery marker SHALL be appended

### Requirement: Over-compressed output is recoverable via smedja_retrieve

An over-compressed result SHALL be fully recoverable by passing its recovery hash to the existing `smedja_retrieve` tool, which MUST return the original uncompressed output.

#### Scenario: recovery returns the original output

- **WHEN** the agent passes a recovery marker's hash to `smedja_retrieve`
- **THEN** the tool SHALL return the original uncompressed command output
- **AND** the returned content SHALL match the output captured before filtering

### Requirement: Tokens saved are recorded to the cost ledger

The daemon SHALL record the tokens saved by filtering (`estimate_tokens(original) - estimate_tokens(compressed)`, clamped at zero) to the cost ledger, kept separate from billed input/output token totals so that `smj cost` can attribute filtering value without distorting incurred cost.

#### Scenario: a filtered command records positive tokens saved

- **WHEN** a command's output is filtered to a smaller result
- **THEN** a tokens-saved figure SHALL be recorded for the session
- **AND** the figure SHALL be the clamped difference between the original and compressed token estimates

#### Scenario: tokens saved do not change billed totals

- **WHEN** tokens saved are recorded for a turn
- **THEN** the turn's billed `input_tok` and `output_tok` totals SHALL be unchanged
- **AND** the tokens-saved figure SHALL be queryable separately

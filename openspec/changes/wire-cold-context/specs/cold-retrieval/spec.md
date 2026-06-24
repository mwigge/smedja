## ADDED Requirements

### Requirement: WorkingMemory retrieves cold context through a ColdStore port

`smedja-memory` SHALL define a `ColdStore` port with an async `retrieve(query, namespace, k)` method returning ranked `ColdResult`s, and `WorkingMemory` SHALL hold an optional `ColdStore`. `WorkingMemory::cold_context(query)` SHALL delegate to the attached store, mapping its ranked results to `Message`s, and SHALL return an empty result when no store is attached. The memory crate MUST NOT depend on `smedja-vault` or any embedder; retrieval MUST be reached only through the port.

#### Scenario: cold_context returns ranked messages from the store

- **WHEN** a `WorkingMemory` has a `ColdStore` attached and `cold_context(query)` is awaited
- **THEN** the store's `retrieve(query, namespace, k)` SHALL be invoked with the configured cold namespace and `k`
- **AND** the returned messages SHALL preserve the store's descending relevance-score order

#### Scenario: no store yields empty cold context

- **WHEN** a `WorkingMemory` has no `ColdStore` attached and `cold_context(query)` is awaited
- **THEN** the result SHALL be an empty list
- **AND** no retrieval SHALL be attempted

### Requirement: Vault-backed cold store embeds and cosine-searches the vault

The daemon SHALL provide a `ColdStore` adapter over the SQLite vault. `retrieve(query, namespace, k)` SHALL embed `query` with the daemon embedder, run the vault's hybrid cosine search over `namespace` limited to `k`, and return results ranked by descending score. All vault access MUST occur off the async executor (via blocking-task dispatch) because the vault is synchronous. Results below a minimum relevance floor SHALL be discarded.

#### Scenario: adapter returns ranked vault entries

- **WHEN** the vault contains entries in the queried namespace and the adapter's `retrieve` is awaited
- **THEN** the adapter SHALL embed the query, search the vault, and return matching entries
- **AND** the entries SHALL be ordered by descending relevance score

#### Scenario: empty vault yields no cold results

- **WHEN** the queried namespace contains no entry that clears the relevance floor
- **THEN** the adapter SHALL return an empty list
- **AND** it SHALL NOT return an error

### Requirement: Orchestrator injects bounded cold context into the prompt

The orchestrator SHALL attach the vault-backed cold store to the per-turn `WorkingMemory`, retrieve cold context for the user turn, and inject any results as a single delimited block ahead of the user message within the sealed prefix. The block's estimated token cost MUST NOT exceed the per-tier cold-budget fraction; lowest-scored results SHALL be dropped until the block fits, and hot turns SHALL never be displaced by cold context.

#### Scenario: cold block injected within budget

- **WHEN** a turn executes against a populated vault and cold results are retrieved
- **THEN** a single delimited cold-context block SHALL be inserted ahead of the sealed user turn
- **AND** the block's estimated token cost SHALL NOT exceed the per-tier cold-budget fraction
- **AND** all hot turns SHALL remain present verbatim

#### Scenario: no cold results means no block

- **WHEN** cold retrieval returns no result for the user turn
- **THEN** no cold-context block SHALL be added
- **AND** prompt assembly SHALL proceed exactly as without cold retrieval

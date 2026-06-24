## ADDED Requirements

### Requirement: Delivered features are not labelled roadmap

Documentation (README and CHANGELOG) SHALL NOT label a feature as roadmap, planned, or "in progress" when the feature has a reachable, non-stub implementation on the live code path. When a merged change closes a previously-documented caveat, the documentation SHALL describe the feature as available.

#### Scenario: a wired feature is described as available

- **WHEN** a feature has a reachable handler or command on the live path (for example `loop.run` driving the `smedja-loop` engine, the `session.rollback` RPC and `smj session rollback` CLI, stable-prefix KV-cache hints derived from the sealed prefix, or the `sd_notify(READY=1)` / `/health` readiness probe)
- **THEN** the README and CHANGELOG SHALL present it as available
- **AND** SHALL NOT list it under "roadmap", "planned", or "in progress"

#### Scenario: a stale "does not exist" claim is corrected

- **WHEN** the documentation asserts that a flag or capability does not exist but a corresponding implementation is present in the code (for example the `--sock` flag on `smj` and `smedja-tui`)
- **THEN** the documentation SHALL be corrected to describe the actual implemented behaviour

### Requirement: Stub-backed capabilities are not advertised as available

Documentation SHALL NOT advertise a tool or capability as available when its backing handler returns empty, fixed, or stub results. Such a capability MAY be described as planned, but MUST be clearly marked as roadmap so that no reader or agent builds on absent data.

#### Scenario: automatic cold-stratum recall is marked roadmap

- **WHEN** `WorkingMemory::cold_context()` returns an empty result and the orchestrator injects no cold-context block into the prompt
- **THEN** the documentation SHALL mark automatic cold-stratum recall into the prompt as roadmap
- **AND** SHALL NOT state that history beyond the warm window is recalled automatically

#### Scenario: a working tool is not described as broken

- **WHEN** a tool's handler returns real results on the live path (for example `smedja_vault_search`, which embeds the query, calls `Vault::search`, and returns a ranked `results` array)
- **THEN** the documentation SHALL describe the tool as available
- **AND** SHALL NOT carry a caveat claiming the tool returns empty results

### Requirement: Every availability claim is verified against the code

Each "available" or "implemented" claim in the README and CHANGELOG SHALL be traceable to a specific reachable handler, command, or RPC method in the codebase, and SHALL be re-verified against that code before the documentation is considered accurate.

#### Scenario: claims are checked against handlers, not intent

- **WHEN** the documentation states that a capability is available
- **THEN** there SHALL exist a reachable, non-stub handler, command, or RPC method that delivers it
- **AND** the verification step SHALL re-read each corrected claim against the cited code location

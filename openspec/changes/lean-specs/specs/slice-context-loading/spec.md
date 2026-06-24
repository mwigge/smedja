## ADDED Requirements

### Requirement: Umbrella intent loads from the sealed cached prefix

A slice SHALL load the umbrella's intent and contract (small, stable) from the sealed stable prefix. The umbrella intent MUST be pushed before `seal_prefix()` so it falls within `stable_prefix()` and is re-sent cheaply from the provider KV-cache on every slice, while the slice's own delta stays in the mutable window after the boundary.

#### Scenario: umbrella intent sits inside the sealed prefix

- **WHEN** a slice's context is assembled
- **THEN** the umbrella intent/contract SHALL be within the sealed stable prefix
- **AND** the slice-specific delta SHALL be in the mutable window after the prefix boundary

### Requirement: Umbrella detail loads from the vault on demand

A slice SHALL load the umbrella's design detail (large, variable) on demand from the vault via cold retrieval, with the cold-query namespace set to the umbrella's `umbrella:<id>` namespace. This is the cold stratum applied to specs: the detail is recalled per slice rather than re-sent in full each time.

#### Scenario: slice loads intent from prefix and detail from vault

- **WHEN** a slice is assembled with a resolvable umbrella
- **THEN** the umbrella intent SHALL be taken from the cached stable prefix
- **AND** the umbrella design detail SHALL be retrieved from the `umbrella:<id>` vault namespace via cold retrieval
- **AND** both SHALL appear in the assembled slice context

#### Scenario: hybrid loading pays the big context once

- **WHEN** several slices of the same umbrella are assembled in sequence
- **THEN** the umbrella intent SHALL be sealed into the cached prefix once
- **AND** each slice SHALL re-send only the cached intent plus its own thin delta, retrieving detail on demand rather than restating the full umbrella

### Requirement: The loop consumes umbrella-once and slice-each

The loop SHALL read the umbrella's coarse `tasks.md` `- [ ]` lines as the slice list and iterate one slice at a time, loading the umbrella intent once (the prefix sealed before iteration) and the slice content each iteration.

#### Scenario: loop runs slices loading umbrella-once

- **WHEN** the loop drives an umbrella's slice list
- **THEN** the umbrella intent SHALL be sealed into the cached prefix exactly once before the slice iteration begins
- **AND** the prefix SHALL NOT be re-sealed per slice
- **AND** each slice SHALL be loaded as a thin delta on top of the already-cached umbrella intent

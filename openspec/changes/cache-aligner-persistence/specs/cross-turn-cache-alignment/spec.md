## ADDED Requirements

### Requirement: CacheAligner is persisted across turns per (session, runner)

`smdjad` SHALL maintain a single `CacheAligner` instance per `(session_id, runner)` pair that outlives an individual turn, and SHALL reuse that instance for every turn served by the same runner within the same session. The orchestrator MUST NOT construct a fresh `CacheAligner` per turn for a `(session, runner)` that has already been observed.

#### Scenario: grown prefix observed across two turns of the same (session, runner)

- **WHEN** a turn for `(session, runner)` seals a stable prefix and a later turn for the same `(session, runner)` seals a strictly longer prefix whose earlier messages are byte-identical
- **THEN** the second turn's `align` call SHALL observe the prior turn's boundary
- **AND** the resulting `CacheHint` drift SHALL be `Drift::Grown`
- **AND** the hint SHALL NOT be a fresh first-turn `Drift::Unchanged`

#### Scenario: mutated prefix observed across two turns of the same (session, runner)

- **WHEN** a turn for `(session, runner)` seals a stable prefix and a later turn for the same `(session, runner)` changes the content of a message that lay inside the prior sealed boundary
- **THEN** the second turn's resulting `CacheHint` drift SHALL be `Drift::Mutated`
- **AND** the hint breakpoint SHALL be truncated to before the first changed message

### Requirement: Runner rotation starts a fresh aligner for the new runner

Because a cache hint targets one specific provider's warm cache, the persisted aligner SHALL be keyed by `(session_id, runner)` and MUST NOT share prefix-digest history across different runners for the same session. A `provider-failover` runner rotation MUST be served by a fresh aligner for the new runner.

#### Scenario: failover to a new runner does not inherit the prior runner's history

- **WHEN** a session has already observed a grown prefix under one runner, and a turn then routes to a different runner for the same session
- **THEN** the new runner's first `align` call SHALL use a fresh `CacheAligner` for `(session, new_runner)`
- **AND** the resulting `CacheHint` drift SHALL be `Drift::Unchanged` at the full sealed prefix
- **AND** the new runner's aligner SHALL NOT be compared against the prior runner's prefix digests

#### Scenario: rotating back to the original runner resumes its history

- **WHEN** traffic rotates back to a runner that previously observed turns for the session
- **THEN** that runner's preserved aligner SHALL resume observing drift from its last recorded boundary
- **AND** a subsequently grown prefix SHALL be reported as `Drift::Grown`, not `Drift::Unchanged`

### Requirement: The persisted hint drives per-runner cache options unchanged

The persisted aligner's `CacheHint` SHALL be passed to the existing per-runner cache-option selection without altering that selection's semantics, so that an advanced breakpoint produces a longer stable cache prefix for cache-capable providers.

#### Scenario: a grown hint advances the realised cache breakpoint

- **WHEN** the persisted aligner reports `Drift::Grown` with an advanced breakpoint for a cache-capable runner
- **THEN** the realised `stable_prefix_len` for that runner SHALL equal the advanced breakpoint
- **AND** the cache strategy selected for that runner SHALL be the same strategy the runner used before persistence was added

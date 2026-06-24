## ADDED Requirements

### Requirement: Retryable provider failures are classified at the adapter boundary

`AdapterError` SHALL expose a `#[must_use] is_retryable(&self) -> bool` predicate and a `kind(&self) -> &'static str` classifier. Rate-limit, quota-exhausted, context-length-exceeded, and provider-down (transport/5xx) failures MUST be reported as retryable; parse and semantically-invalid responses MUST NOT be reported as retryable.

#### Scenario: quota and context-length errors are retryable

- **WHEN** an adapter returns `AdapterError::QuotaExhausted` or `AdapterError::ContextLengthExceeded`
- **THEN** `is_retryable()` SHALL return `true`
- **AND** `kind()` SHALL return `"quota_exhausted"` or `"context_length_exceeded"` respectively

#### Scenario: malformed responses are not retryable

- **WHEN** an adapter returns `AdapterError::Parse` or `AdapterError::InvalidResponse`
- **THEN** `is_retryable()` SHALL return `false`
- **AND** the orchestrator SHALL NOT rotate to another provider for that failure

### Requirement: Pool exposes an ordered, de-duplicated eligible rotation ring

`ProviderPool` SHALL provide `eligible_ring(runner, tier)` returning the providers eligible to serve a turn routed to `(runner, tier)`. The routed entry MUST come first, followed by entries of a compatible tier in the pool's stable priority order, ending with the pool default. Each `(Runner, Tier)` key MUST appear at most once so the ring is finite.

#### Scenario: routed provider is tried first, default last, no duplicates

- **WHEN** `eligible_ring(runner, tier)` is built for a pool with several compatible providers
- **THEN** the first entry SHALL be the routed `(runner, tier)` when present
- **AND** the pool default SHALL appear last if not already yielded
- **AND** no `(Runner, Tier)` key SHALL appear more than once

#### Scenario: a deep-routed turn does not rotate down to a fast tier

- **WHEN** a turn routed to the `deep` tier builds its eligible ring
- **THEN** the ring SHALL exclude `fast`-tier entries
- **AND** SHALL include only entries whose tier is at least as capable as `deep`

#### Scenario: context-length failure requires a strictly more capable tier

- **WHEN** the classified failure kind is `context_length_exceeded`
- **THEN** only entries with a strictly more capable tier than the routed tier SHALL be eligible
- **AND** if no such entry exists the turn SHALL fail with kind `context_length_exceeded`

### Requirement: Turn rotates to the next eligible provider on a retryable failure

On a retryable provider failure the orchestrator SHALL rotate to the next entry in the eligible ring rather than failing the turn, re-using the turn's assembled `WorkingMemory` prompt and accumulated tool history. A rate-limit failure SHALL first exhaust its same-provider back-off budget before escalating to a rotation.

#### Scenario: quota error rotates to the next provider and the turn completes

- **WHEN** the first provider in the ring returns a quota-exhausted error and the next provider succeeds
- **THEN** the turn SHALL complete against the next provider
- **AND** the prompt sent to the next provider SHALL be the same `WorkingMemory`-assembled prompt
- **AND** no tool SHALL be re-executed solely because of the rotation

#### Scenario: native session id is not carried across runners

- **WHEN** the turn rotates from one runner to a different runner
- **THEN** the new provider call SHALL resolve `provider_session_id` from the new runner's own session store key
- **AND** SHALL NOT reuse the failed runner's provider-native resume identifier

### Requirement: Rotation is bounded and fails loudly when exhausted

A turn SHALL rotate at most `MAX_PROVIDER_ROTATIONS` times and SHALL visit each eligible entry at most once. When the rotation cap or the end of the ring is reached, the turn SHALL fail with the last classified error kind rather than rotating indefinitely.

#### Scenario: ring exhaustion fails the turn with the last kind

- **WHEN** every entry in the eligible ring returns a retryable error
- **THEN** the turn SHALL fail (it MUST NOT hang)
- **AND** the failure reason SHALL carry the last classified error kind

### Requirement: Each rotation is observable via error telemetry

Each rotation SHALL record `smedja.error.kind` and `smedja.error.retryable` on the turn span and emit a structured log line naming the from-runner, the to-runner, and the classified kind. A terminal failure after exhaustion SHALL record `smedja.error.retryable = false`.

#### Scenario: rotation records error kind and retryability

- **WHEN** a turn rotates from one provider to another on a retryable failure
- **THEN** the turn span SHALL carry `smedja.error.kind` set to the classified kind
- **AND** the turn span SHALL carry `smedja.error.retryable` set to `true`
- **AND** a log line SHALL name the from-runner and to-runner

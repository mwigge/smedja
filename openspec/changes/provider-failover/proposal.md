## Why

smedja's routing is static first-match: `Assayer::route_decision(role, complexity)` resolves a `(Runner, Tier, model)` and the orchestrator calls `ProviderPool::get(runner, tier)`, which falls back exactly once to the pool default and otherwise pins the turn to a single provider for its whole lifetime (`bin/smdjad/src/provider_pool.rs`, `bin/smdjad/src/orchestrator/mod.rs`). There is no automatic rotation when that provider becomes unusable mid-turn.

The orchestrator's `'tool_loop` already distinguishes `DrainError::RateLimited` from `DrainError::Other` (`bin/smdjad/src/common.rs`), but its only recovery is a fixed exponential back-off (`MAX_RATE_LIMIT_RETRIES = 4`) against the **same** provider. Every other failure — quota exhausted, context-window exceeded, provider down — collapses into `DrainError::Other` and fails the turn immediately, even when another configured runner of a compatible tier could have served it. The Go predecessor (milliways) handled this with a "rotation ring" that advanced to the next eligible runner on quota/context exhaustion; smedja dropped that behaviour.

Two gaps compound the problem:

- The provider boundary cannot tell these failures apart. `AdapterError` (`crates/smedja-adapter/src/error.rs`) has only `Http`, `Parse`, `InvalidResponse`, `Request`, and `RateLimited`; quota and context-length errors surface as opaque `Request`/`InvalidResponse` strings, so the orchestrator cannot classify them as retryable-by-rotation.
- The pool has no concept of "the next eligible entry". `ProviderPool::get` returns one entry; there is no ordered iterator over compatible runners to rotate through.

This change adds a bounded provider rotation/failover ring: on a retryable provider error the orchestrator rotates to the next eligible runner of a compatible tier, preserving the turn (its assembled `WorkingMemory` and accumulated tool history), with bounded total attempts and full OTel visibility via the existing `smedja.error.kind` / `smedja.error.retryable` attributes.

## What Changes

- **Classify retryable provider failures at the adapter boundary**: add `AdapterError::QuotaExhausted` and `AdapterError::ContextLengthExceeded` variants alongside the existing `RateLimited`, and a `#[must_use] fn is_retryable(&self) -> bool` plus `fn kind(&self) -> &'static str` so callers and telemetry classify uniformly. Adapters that detect provider-down (HTTP 5xx, connection refused) map to a retryable `AdapterError::Request`-class signal.
- **Extend `DrainError` with a rotatable variant**: `bin/smdjad/src/common.rs` gains `DrainError::Rotatable { kind, retry_after }` (covering quota / context-length / provider-down) distinct from `DrainError::RateLimited` (back-off-then-retry-same-provider) and `DrainError::Other` (fatal). `drain_stream` maps each `AdapterError` onto the right `DrainError`.
- **Build the rotation ring in the pool**: add `ProviderPool::eligible_ring(runner, tier)` returning an ordered, de-duplicated list of pool entries eligible for a turn routed to `(runner, tier)` — the routed entry first, then other entries of a **compatible tier** (same tier, then a tier no more capable than the routed tier), ending with the pool default. Each entry is yielded at most once.
- **Rotate on retryable failure, preserving the turn**: the orchestrator drives the turn over the ring. On a `DrainError::Rotatable` (or `RateLimited` whose back-off budget is exhausted) it advances to the next ring entry, re-using the same `WorkingMemory` prompt and accumulated tool history, until the ring is exhausted or a bounded `MAX_PROVIDER_ROTATIONS` cap is hit. The turn fails only when no eligible provider remains.
- **OTel visibility for rotation**: each rotation records `smedja.error.kind` and `smedja.error.retryable` (via `tel::set_span_error`) plus a rotation attempt index on the turn span, and emits a structured `warn!` naming the from/to runner and the classified kind.

## Capabilities

### New Capabilities

- `provider-failover`: on a retryable provider error (rate-limited, quota exhausted, context-window exceeded, provider down) the orchestrator rotates to the next eligible runner of a compatible tier through a bounded ring, preserving the turn's assembled prompt and tool history, and surfaces each rotation through `smedja.error.kind` / `smedja.error.retryable` telemetry. Retryable failures are classified at the adapter boundary via `AdapterError::is_retryable`.

## Impact

- `crates/smedja-adapter/src/error.rs`: add `QuotaExhausted` and `ContextLengthExceeded` variants; add `is_retryable` and `kind` methods (additive; existing `RateLimited` unchanged).
- `crates/smedja-adapter/src/{anthropic,openai,openai_compat}.rs`: map provider quota / context-length / 5xx responses onto the new variants.
- `bin/smdjad/src/common.rs`: add `DrainError::Rotatable`; map `AdapterError` → `DrainError` in `drain_stream`.
- `bin/smdjad/src/provider_pool.rs`: add `eligible_ring(runner, tier)` and a tier-compatibility helper.
- `bin/smdjad/src/orchestrator/mod.rs`: drive the turn over the ring; rotate on retryable failure; record rotation telemetry. The existing `WorkingMemory` build and `stable_prefix_len` logic are reused unchanged.
- README: the routing section documents automatic failover across compatible providers.

## 1. Classify retryable provider failures at the adapter boundary

- [x] 1.1 Add a test in `crates/smedja-adapter/src/error.rs` asserting `is_retryable()` and `kind()` for each variant (`rate_limited`/`quota_exhausted`/`context_length_exceeded` retryable; `parse`/`invalid_response` not) — failing first
- [x] 1.2 Add `QuotaExhausted` and `ContextLengthExceeded` variants to `AdapterError`, each carrying a provider message string; keep existing `RateLimited { retry_after }` unchanged
- [x] 1.3 Implement `#[must_use] pub fn is_retryable(&self) -> bool` and `#[must_use] pub fn kind(&self) -> &'static str` on `AdapterError` per the design (provider-down = transport/5xx `Http`)
- [x] 1.4 In `crates/smedja-adapter/src/{anthropic,openai,openai_compat}.rs`, map provider quota (e.g. 403 / `insufficient_quota`) → `QuotaExhausted` and context-length (e.g. `context_length_exceeded` / "prompt is too long") → `ContextLengthExceeded`; add an adapter-level test for each mapping
- [x] 1.5 Run `cargo test -p smedja-adapter`; all green

## 2. Map AdapterError onto a rotatable DrainError

- [x] 2.1 Add a unit test in `bin/smdjad/src/common.rs` asserting `drain_stream` yields `DrainError::Rotatable { kind, .. }` for a quota/context-length error item and `DrainError::RateLimited` for a 429 — failing first
- [x] 2.2 Add `DrainError::Rotatable { kind: &'static str, retry_after: Option<std::time::Duration> }` and extend its `Display` impl
- [x] 2.3 In `drain_stream` (`common.rs` ~223), map `AdapterError` items via `is_retryable()`/`kind()`: `RateLimited` → `RateLimited`; retryable quota/context/provider-down → `Rotatable`; otherwise → `Other`
- [x] 2.4 Run `cargo test -p smdjad --lib common`; all green

## 3. Build the eligible rotation ring in the pool

- [x] 3.1 Add a `provider_pool.rs` test (`eligible_ring_orders_routed_first_then_compatible_dedup`) asserting the ring starts with the routed entry, contains compatible-tier alternatives in pool priority order, ends with the default, and yields each `(Runner, Tier)` at most once — failing first
- [x] 3.2 Add a test `deep_route_does_not_rotate_down_to_fast` and `context_length_kind_requires_more_capable_tier` for the `tier_compatible` helper
- [x] 3.3 Implement `fn tier_compatible(routed: Tier, candidate: Tier, kind: &str) -> bool` (capability order `Fast ≤ Local ≤ Deep`; never rotate below routed; context-length requires strictly-more-capable)
- [x] 3.4 Implement `#[must_use] pub fn eligible_ring(&self, runner: Runner, tier: Tier) -> Vec<&ProviderEntry>` preserving the pool's stable insertion/priority order and de-duplicating by `(Runner, Tier)`
- [x] 3.5 Run `cargo test -p smdjad --lib provider_pool`; all green

## 4. Rotate on retryable failure, preserving the turn

- [x] 4.1 Add `const MAX_PROVIDER_ROTATIONS: u32 = 3;` in `orchestrator/mod.rs` and an orchestrator-level test that, given a first provider yielding a quota error and a second yielding success, the turn completes against the second provider with the same `WorkingMemory` prompt (`rotates_to_next_provider_on_quota_error_preserving_prompt`) — failing first
- [x] 4.2 Replace the single `pool.get(route.runner, route.tier)` borrow with iteration over `pool.eligible_ring(route.runner, route.tier)`; the loop body keeps the existing `WorkingMemory`/`build_prompt`/`stable_prefix_len`/verbosity-steering construction unchanged
- [x] 4.3 On `DrainError::Rotatable` — and on `DrainError::RateLimited` once `MAX_RATE_LIMIT_RETRIES` back-off is exhausted — advance to the next ring entry instead of failing, re-deriving `CallOptions` for the new entry (recompute `stable_prefix_len` from `mem.stable_prefix()`; resolve `provider_session_id` from the **new** runner's session key; never carry a native resume id across runners)
- [x] 4.4 Enforce the bound: stop after `MAX_PROVIDER_ROTATIONS` rotations or when the ring is exhausted; fail the turn with the last classified error kind via `TurnEvent::fail` and `tel::set_span_error`
- [x] 4.5 Add a test `turn_fails_after_ring_exhausted_with_last_kind` asserting the turn fails (not hangs) when every ring entry yields a retryable error, and that the failure reason carries the last `kind`
- [x] 4.6 Run `cargo test -p smdjad`; all green

## 5. Rotation telemetry

- [x] 5.1 On each rotation, call `tel::set_span_error(&mut turn_span, kind, message, true)` and set a rotation attempt index attribute (reusing `tel::ERROR_COUNT`); emit a `warn!` naming from-runner, to-runner, and `kind`
- [x] 5.2 On final failure after exhaustion, record `smedja.error.retryable = false` for the terminal span status
- [x] 5.3 Add a test asserting the turn span carries `smedja.error.kind` and `smedja.error.retryable` after a rotation (`rotation_records_error_kind_and_retryable`)

## 6. Verify

- [x] 6.1 `cargo fmt --all` clean
- [x] 6.2 `cargo clippy -p smedja-adapter -p smdjad -- -D warnings -W clippy::pedantic` clean for the touched code (pre-existing workspace debt in untouched crates excluded)
- [x] 6.3 `cargo test --workspace` — all green
- [x] 6.4 `openspec validate provider-failover --strict` — clean

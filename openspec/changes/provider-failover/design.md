## Context

A turn is routed once and then pinned to a single provider for its whole lifetime:

- `Assayer::route_decision(role, complexity)` (`crates/smedja-assayer/src/assayer.rs`) resolves a `RoutingDecision`; the orchestrator turns it into a `Route { runner, tier, model }`.
- `ProviderPool::get(runner, tier)` (`bin/smdjad/src/provider_pool.rs`) returns the matching `ProviderEntry`, or — when absent — the single pool `default`; otherwise `None`. There is no ordered set of alternatives.
- `TurnOrchestrator::run` (`bin/smdjad/src/orchestrator/mod.rs`) borrows that one `provider` and calls `provider.stream_chat(&prompt, &opts)` inside `'tool_loop`. The prompt is assembled through `WorkingMemory` (sealed stable prefix + hot/warm strata) and `opts.stable_prefix_len` is derived from `mem.stable_prefix()`.

Failure handling today (`orchestrator/mod.rs` ~554–622, `common.rs` ~125–227):

- `drain_stream` returns `Result<_, DrainError>` where `DrainError::RateLimited { retry_after }` comes from `AdapterError::RateLimited`, and every other `AdapterError` becomes `DrainError::Other(String)`.
- The inner `loop` retries `RateLimited` against the **same** provider with exponential back-off up to `MAX_RATE_LIMIT_RETRIES = 4`; on exhaustion it fails the turn. `DrainError::Other` fails the turn immediately. A 5-minute `tokio::time::timeout` wraps each stream.

The provider boundary cannot distinguish quota / context-length / provider-down from a generic parse error:

- `AdapterError` (`crates/smedja-adapter/src/error.rs`) variants are `Http`, `Parse`, `InvalidResponse`, `Request`, `RateLimited { retry_after }`. Anthropic/OpenAI adapters already detect HTTP 429 and emit `RateLimited`; quota ("insufficient_quota", 403) and context-length ("context_length_exceeded", "prompt is too long") responses currently fall through to `Request`/`InvalidResponse` strings.

Telemetry primitives already exist (`crates/smedja-telemetry/src/lib.rs`): `ERROR_KIND` (`smedja.error.kind`), `ERROR_RETRYABLE` (`smedja.error.retryable`), `ERROR_COUNT` (`smedja.error.count`), and `set_span_error(span, kind, message, retryable)`.

## Goals / Non-Goals

Goals:
- Classify rate-limit, quota, context-length, and provider-down failures distinctly at the `AdapterError` boundary, with a single `is_retryable` predicate.
- Rotate a turn to the next eligible runner of a compatible tier on a retryable failure, preserving the assembled `WorkingMemory` prompt and accumulated tool history.
- Bound rotation: a turn rotates at most `MAX_PROVIDER_ROTATIONS` times and visits each eligible entry at most once.
- Make every rotation observable via `smedja.error.kind` / `smedja.error.retryable` and a structured log line.

Non-Goals:
- Changing the static routing table or `Assayer` rules — rotation is a recovery layer below routing, not a replacement for it.
- Persistent provider health tracking / circuit breaking across turns — rotation state is per-turn and rebuilt each turn (consistent with the per-turn `WorkingMemory` build).
- Re-summarising or trimming the prompt on a context-length error — rotation moves to a larger/different provider; prompt re-fitting is owned by the memory/context-budget work, not here.
- Mid-stream resumption: a rotation restarts the provider call from the current `WorkingMemory` state; it does not splice partial output from the failed provider.

## Decisions

**Decision: which errors are retryable-by-rotation.**
`AdapterError::is_retryable()` returns `true` for `RateLimited`, `QuotaExhausted`, `ContextLengthExceeded`, and `Http` errors that are transport-level/5xx/connection-refused (provider down). It returns `false` for `Parse`, `InvalidResponse` (well-formed but semantically wrong), and `Request` errors that are not transport faults (e.g. a missing binary — rotating cannot fix a config error). `kind()` returns a stable string (`"rate_limited"`, `"quota_exhausted"`, `"context_length_exceeded"`, `"provider_down"`, `"parse"`, `"invalid_response"`, `"request"`) used directly as the `smedja.error.kind` attribute value.
- Rationale: the orchestrator and telemetry must classify uniformly; a single predicate at the lowest layer avoids string-matching in `drain_stream`.
- Alternative considered: classify in the orchestrator by inspecting error strings. Rejected — fragile, duplicates logic across adapters, and loses the typed boundary.

**Decision: `DrainError` gains `Rotatable`, distinct from `RateLimited` and `Other`.**
`drain_stream` maps `AdapterError` → `DrainError`: `RateLimited` → `DrainError::RateLimited` (unchanged; back-off then retry same provider), `QuotaExhausted` / `ContextLengthExceeded` / provider-down → `DrainError::Rotatable { kind, retry_after }`, everything non-retryable → `DrainError::Other`. `RateLimited` whose back-off budget (`MAX_RATE_LIMIT_RETRIES`) is exhausted is then escalated to a rotation rather than failing the turn.
- Rationale: rate-limit back-off and provider rotation are different recovery strategies; keeping `RateLimited` separate preserves the existing back-off-on-same-provider behaviour as the first line of defence, with rotation as the escalation.
- Alternative: collapse rate-limit into rotation immediately. Rejected — a brief 429 is usually transient on the same provider; rotating away on the first 429 would waste the (often cheaper, session-warm) primary provider.

**Decision: rotation order — the eligible ring.**
`ProviderPool::eligible_ring(runner, tier)` yields, in order and de-duplicated by `(Runner, Tier)`:
1. the exact routed entry `(runner, tier)` if present;
2. other entries whose tier is **compatible** with the routed tier (see next decision), ordered by the pool's stable insertion priority (the same priority `build_provider_pool` uses to pick the default);
3. the pool default, if not already yielded.
Each `(Runner, Tier)` key appears at most once, so the ring is finite and terminates.
- Rationale: the routed provider is tried first (honours the assayer); compatible alternatives follow in the same priority order the pool already encodes; the default is the last resort. De-duplication guarantees termination.
- Alternative: round-robin across all providers. Rejected — ignores tier compatibility (a `fast`-routed turn must not silently fall to a model that cannot serve the task) and the assayer's priority intent.

**Decision: how the routed tier constrains eligible fallbacks.**
A candidate tier is compatible with the routed tier when it is the **same** tier, or a tier that is **at least as capable** for that turn. Capability order is `Fast ≤ Local ≤ Deep` for context/quality; a `Fast`-routed turn may rotate to `Local` or `Deep` (more capable is safe), but a `Deep`-routed turn does **not** rotate down to `Fast` (a smaller context window cannot serve a turn that needed `Deep`). For a `ContextLengthExceeded` failure specifically, only strictly-more-capable tiers are eligible (rotating to an equal-window provider would hit the same limit). This is encoded in a `tier_compatible(routed, candidate, kind)` helper.
- Rationale: rotation must not degrade the turn below what routing chose; context-length failures specifically demand a bigger window.
- Alternative: allow any tier. Rejected — would let a complex turn silently land on a `fast` model and produce worse output than failing loudly.

**Decision: bounded attempts.**
A turn rotates at most `MAX_PROVIDER_ROTATIONS = 3` times (4 providers total including the routed one), independent of the per-provider `MAX_RATE_LIMIT_RETRIES = 4` back-off budget. The ring is also naturally bounded by its de-duplicated length, so the effective cap is `min(MAX_PROVIDER_ROTATIONS, ring.len() - 1)`. When the cap or ring end is reached, the turn fails with the **last** classified error kind.
- Rationale: prevents unbounded fan-out across a large pool and bounds worst-case latency; the de-dup length is a hard upper bound regardless.

**Decision: idempotency / turn preservation across a rotation.**
Rotation re-uses the same per-turn `WorkingMemory` (sealed stable prefix + accumulated assistant/tool-result turns) and re-derives `CallOptions` for the new provider: `stable_prefix_len` is recomputed from `mem.stable_prefix()` (kept for cache-capable providers, `None` otherwise) and `provider_session_id` is taken from the **new** runner's session store key (a provider-native resume id from the failed provider is never carried across). No checkpoint is written between rotations and no tool is re-executed; only the `stream_chat` call is retried against the next provider. Cost/token snapshots accumulate across the attempt as today.
- Rationale: the turn's logical state lives in `WorkingMemory`, not in the provider; replaying it against a new provider is safe and produces an equivalent continuation. Resetting the native session id avoids sending one provider's opaque resume token to another.
- Alternative: rebuild `WorkingMemory` from scratch per rotation. Rejected — wasteful and would drop in-turn tool results already appended.

## Risks / Trade-offs

- [Risk] Rotating on a misclassified transient error could move a turn off a warm, cheaper provider unnecessarily → Mitigation: `RateLimited` keeps its same-provider back-off as the first response; rotation is the escalation only after the back-off budget is spent or for unambiguous quota/context-length/provider-down kinds.
- [Risk] A `ContextLengthExceeded` turn could rotate to another provider with the same window and fail again → Mitigation: `tier_compatible(..., ContextLengthExceeded)` admits only strictly-more-capable tiers; if none exist the turn fails loudly with `kind = "context_length_exceeded"`.
- [Risk] Carrying a provider-native resume id across runners would corrupt the new provider's session → Mitigation: `provider_session_id` is resolved per ring entry from that runner's own session store key; it is never reused across runners.
- [Risk] Rotation adds latency (a failed attempt then a retry) → Mitigation: bounded by `MAX_PROVIDER_ROTATIONS = 3` and the de-duplicated ring length; each attempt keeps the existing 5-minute stream timeout so a hung provider cannot stall the whole budget.
- [Risk] New `AdapterError` variants are a (minor) breaking change for exhaustive matches on `AdapterError` → Mitigation: the enum is already `#[non_exhaustive]`-shaped in practice (callers match the variants they handle and use `is_retryable`/`kind` for the rest); the orchestrator routes unknown kinds through `is_retryable`.

## Why

The single Anthropic stable-prefix KV-cache hint is **already shipped** by the `wire-memory` change. `TurnOrchestrator` sets `stable_prefix_len: Some(mem.stable_prefix())` for the Anthropic runner (`bin/smdjad/src/orchestrator/mod.rs` ~517) and the Anthropic adapter (`crates/smedja-adapter/src/anthropic.rs` `build_body`) applies `cache_control: ephemeral` to the system block, the last tool definition, and the message at index `stable_prefix_len - 1`. This change does **not** re-propose that hint.

Two gaps remain:

- **No drift tracking.** `stable_prefix_len` is read once per turn from `WorkingMemory::stable_prefix()`, which is sealed once and never re-evaluated as the conversation grows. Across a multi-turn session the sealed boundary stays fixed at the original seal point while the genuinely-stable region (system prompt + skills + early settled turns) can grow. There is no component that observes prefix growth, decides when a new cache breakpoint is worthwhile, or guards against the boundary landing on a message whose content has mutated between turns — so cache hits silently degrade as sessions lengthen.
- **Anthropic-only.** `stable_prefix_len` is consumed solely by `anthropic.rs`. `crates/smedja-adapter/src/openai.rs` (`build_body`) and `crates/smedja-adapter/src/gemini.rs` (`build_contents`) ignore it entirely. The orchestrator hard-codes the hint to `None` for every non-Anthropic runner. OpenAI offers automatic prompt caching that rewards a stable, byte-identical leading prefix (placement, not an explicit flag), and Gemini offers explicit context caching via `cachedContent`. Neither benefit is realised today.

This change adds a dedicated `CacheAligner` that tracks stable-prefix drift and selects cache breakpoints across turns, and generalises provider cache hints beyond Anthropic to OpenAI automatic prompt caching and Gemini explicit context caching.

## What Changes

- **Introduce `CacheAligner` (drift tracking + breakpoint selection)**: a per-session component that records the prior turn's sealed-prefix boundary and a digest of the messages within it, detects whether the stable region has grown or mutated since the last turn, and produces a `CacheHint` describing the breakpoint index (or `None` when realignment is unsafe). It guards against placing a breakpoint on a message whose digest changed between turns.
- **Provider-neutral `CacheHint` on `CallOptions`**: keep the existing `stable_prefix_len` field (BACKWARD-COMPATIBLE — Anthropic continues to read it), and add a provider-neutral `cache_strategy` describing how each adapter should realise the hint. `CacheAligner` populates it; `stable_prefix_len` is derived from the same boundary so the Anthropic path is unchanged.
- **OpenAI automatic prompt caching**: teach `openai.rs` to honour the aligner by keeping the leading `stable_prefix_len` messages in a stable, byte-identical order at the front of the array (no explicit cache flag exists for OpenAI automatic caching — alignment is purely ordering/stability). Add a `prompt_cache_key` passthrough when the provider config enables it.
- **Gemini explicit context caching**: teach `gemini.rs` to reference a `cachedContent` resource for the stable prefix when the aligner emits a Gemini context-cache hint and the adapter is configured with a cache handle, falling back to plain `contents` when no handle is present.
- **Orchestrator wiring**: build/reuse the `CacheAligner` per session, feed it the sealed `WorkingMemory`, and set both `stable_prefix_len` (cache-capable providers) and `cache_strategy` (per routed runner) on `CallOptions` instead of the current `if runner == "anthropic" { Some(...) } else { None }` branch.

Out of scope (referenced only): the Anthropic single-point hint itself (shipped by `wire-memory`); cold-stratum semantic retrieval (owned by `wire-cold-context`); persisting cache handles across daemon restarts.

## Capabilities

### New Capabilities

- `cache-alignment`: a `CacheAligner` tracks the stable-prefix boundary and a per-message digest across turns, detects drift (growth or mutation), selects a safe cache breakpoint, and emits a provider-neutral `CacheHint`. The orchestrator applies the hint per routed runner: Anthropic via `stable_prefix_len` (unchanged), OpenAI via stable byte-identical prefix ordering plus optional `prompt_cache_key`, and Gemini via an optional `cachedContent` reference.

## Impact

- `crates/smedja-adapter/src/types.rs`: add a provider-neutral `cache_strategy` field (and a `CacheStrategy` enum) to `CallOptions`; keep `stable_prefix_len` for the Anthropic path.
- `crates/smedja-adapter/src/openai.rs`: `build_body` keeps the leading stable prefix byte-identical and emits `prompt_cache_key` when configured.
- `crates/smedja-adapter/src/gemini.rs`: `build_contents`/`stream_chat` reference `cachedContent` when a Gemini context-cache hint and handle are present.
- `bin/smdjad/src/orchestrator/mod.rs`: introduce per-session `CacheAligner`; replace the Anthropic-only `stable_prefix_len` branch with aligner-driven `stable_prefix_len` + `cache_strategy`.
- `crates/smedja-memory/src/`: a new `CacheAligner` (built on `seal_prefix`/`stable_prefix`) and its module export.
- README: cross-provider caching and drift tracking become accurate.

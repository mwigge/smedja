## Context

`wire-memory` shipped the Anthropic stable-prefix hint end to end:

- `bin/smdjad/src/orchestrator/mod.rs` (~517) sets `stable_prefix_len: if runner == "anthropic" { Some(mem.stable_prefix()) } else { None }`.
- `crates/smedja-adapter/src/anthropic.rs` `build_body` reads `opts.stable_prefix_len`, marks the system block and last tool definition with `cache_control: ephemeral`, and marks the message at index `stable_prefix_len - 1` (`mark = cache && prefix_len > 0 && i + 1 == prefix_len`).
- `crates/smedja-memory/src/memory.rs` provides `seal_prefix()` (sets `stable_prefix = messages.len()`), `stable_prefix() -> usize`, and `build_prompt(budget)` (emits `messages[..stable_prefix]` then budgeted mutable turns).

The remaining surface this change builds on:

- The seal is taken **once** per turn (`mem.seal_prefix()` in the orchestrator after pre-turn context). Nothing observes how the sealed region relates to the previous turn, so as a session lengthens the cache breakpoint cannot follow a growing stable region, and there is no guard if a message inside the prior boundary mutated.
- `crates/smedja-adapter/src/openai.rs` `build_body` and `crates/smedja-adapter/src/gemini.rs` `build_contents` never read `opts.stable_prefix_len`. OpenAI prepends `opts.system` then the messages verbatim; Gemini injects `opts.system` as the first user turn then maps roles. Both already place the stable content first, but neither emits a cache key (OpenAI) nor a cache reference (Gemini), and neither guarantees byte-identical ordering across turns under drift.
- `CallOptions` (`crates/smedja-adapter/src/types.rs`) carries `stable_prefix_len: Option<usize>` and nothing provider-neutral.

OpenAI automatic prompt caching has **no explicit per-message flag** — caching is triggered server-side when a request shares a long, byte-identical leading prefix with a recent request, optionally keyed by `prompt_cache_key`. Gemini context caching is **explicit**: the stable content is uploaded once to create a `cachedContent` resource, and later requests reference it by name in place of resending those `contents`.

## Goals / Non-Goals

Goals:

- Add a `CacheAligner` that tracks the stable-prefix boundary and a per-message content digest across turns and reports drift (growth / mutation / unchanged).
- Select a safe cache breakpoint: never place it on a message whose digest changed since the last turn.
- Emit a provider-neutral `CacheHint`/`CacheStrategy` and apply it per routed runner: Anthropic (unchanged), OpenAI (stable prefix ordering + optional key), Gemini (optional `cachedContent`).

Non-Goals:

- **Re-doing the Anthropic single-point hint.** It is already shipped by `wire-memory`; this change only feeds it from the aligner and leaves the adapter behaviour identical.
- Cold-stratum semantic retrieval (owned by `wire-cold-context`).
- Creating/uploading/garbage-collecting Gemini `cachedContent` resources, or persisting cache handles across daemon restarts — the adapter consumes a handle when supplied and falls back otherwise; lifecycle is out of scope here.
- Changing `WorkingMemory::seal_prefix`/`stable_prefix` semantics — the aligner observes them, it does not replace them.

## Decisions

**Decision: `CacheAligner` is a per-session observer over the sealed `WorkingMemory`.**
The aligner holds the previous turn's boundary index plus a `Vec` of per-message digests for messages inside that boundary. On each turn it compares the new sealed prefix against the stored state and classifies drift: `Unchanged` (same boundary, same digests), `Grown` (boundary advanced, prior digests preserved), or `Mutated` (a message within the prior boundary changed). It then returns a `CacheHint { breakpoint, strategy }`.

- Rationale: drift is a cross-turn property; `WorkingMemory` is rebuilt per turn and has no memory of the prior boundary, so a dedicated component must carry that state.
- Alternative considered: extend `WorkingMemory` to remember prior seals. Rejected — `WorkingMemory` is per-turn and single-responsibility (assembly + budgeting); cross-turn cache state is a separate concern.

**Decision: digest-based mutation guard selects the breakpoint.**
The breakpoint is the longest leading run of messages whose digests are unchanged versus the prior turn (capped at the current `stable_prefix()`). On `Mutated`, the breakpoint is truncated to just before the first changed message; if that leaves a zero-length prefix the hint is `None` (no realignment this turn).

- Rationale: a cache hint on a mutated message both misses and wastes a write; the guard keeps hints on genuinely stable content.
- Alternative considered: always trust `stable_prefix()`. Rejected — that is exactly today's behaviour and the source of the silent degradation this change targets.

**Decision: per-provider cache strategy via a `CacheStrategy` enum on `CallOptions`.**
Add `cache_strategy: CacheStrategy` alongside the existing `stable_prefix_len`. Variants: `None`, `AnthropicEphemeral`, `OpenAiAutomatic { cache_key: Option<String> }`, `GeminiContext { cached_content: Option<String> }`. `stable_prefix_len` remains the Anthropic input and is still derived from the same boundary, so the Anthropic adapter and its tests are untouched.

- Rationale: keep the shipped Anthropic path byte-for-byte; give OpenAI/Gemini a typed, explicit instruction rather than overloading `stable_prefix_len`.
- Alternative considered: reuse `stable_prefix_len` for all providers. Rejected — OpenAI needs a key and Gemini needs a resource name; a single integer cannot carry that.

**Decision: OpenAI realises the hint as prefix stability, not a flag.**
`openai.rs` keeps the leading `stable_prefix_len` messages byte-identical and first in the array (it already prepends `system` + messages in order) and, when `CacheStrategy::OpenAiAutomatic { cache_key: Some(k) }`, sets `prompt_cache_key` on the body. There is no per-message `cache_control` for OpenAI.

- Rationale: OpenAI automatic caching is order/stability-driven; the aligner's job is to ensure the prefix the orchestrator sends is stable across turns and to supply an optional key.

**Decision: Gemini references `cachedContent` only when a handle is supplied.**
`gemini.rs` sets `body["cachedContent"]` and omits the cached leading turns from `contents` when `CacheStrategy::GeminiContext { cached_content: Some(name) }`; otherwise it builds `contents` exactly as today.

- Rationale: without a created cache resource there is nothing to reference; the safe default is the current full-`contents` behaviour.

**Decision: the orchestrator owns aligner lifecycle and sets both fields.**
The orchestrator builds (or reuses) the session `CacheAligner`, calls it after `seal_prefix()`, and sets `stable_prefix_len` (cache-capable providers) and `cache_strategy` (selected from the routed runner) on `CallOptions`, replacing the `if runner == "anthropic"` branch.

- Rationale: the orchestrator already knows the runner and holds the sealed `WorkingMemory`; it is the single place provider strategy is chosen.

## Risks / Trade-offs

- [Risk] A breakpoint placed on a mutated message both misses and wastes a cache write → Mitigation: the digest guard truncates the breakpoint before the first changed message and emits `None` when no stable prefix remains.
- [Risk] OpenAI prefix instability (e.g. non-deterministic ordering of injected context) defeats automatic caching → Mitigation: the aligner only advances the breakpoint over digest-stable messages; unstable content stays out of the cached prefix.
- [Risk] Referencing a stale/expired Gemini `cachedContent` name fails the request → Mitigation: the adapter falls back to full `contents` whenever the handle is absent; resource lifecycle is a non-goal, so only valid supplied handles are referenced.
- [Risk] Adding `cache_strategy` to `CallOptions` touches every adapter's struct literal → Mitigation: default to `CacheStrategy::None`; non-participating adapters ignore the field, mirroring how `stable_prefix_len` is already `None` for them.
- [Risk] Per-session aligner adds cross-turn state → Mitigation: state is two small fields (boundary index + digest vector); it is advisory and a wrong guess degrades to no-cache, never to a wrong response.

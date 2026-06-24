## 1. Provider-neutral cache strategy on CallOptions

- [x] 1.1 Add a failing test in `crates/smedja-adapter/src/types.rs` asserting `CallOptions` exposes a `cache_strategy` field defaulting to `CacheStrategy::None` and that `stable_prefix_len` is still present
- [x] 1.2 Add the `CacheStrategy` enum (`None`, `AnthropicEphemeral`, `OpenAiAutomatic { cache_key: Option<String> }`, `GeminiContext { cached_content: Option<String> }`) and the `cache_strategy` field; keep `stable_prefix_len`
- [x] 1.3 Update every existing `CallOptions { .. }` literal across `smedja-adapter` (anthropic/openai/gemini/codex_cli/claude_cli/subprocess test constructions) to set `cache_strategy: CacheStrategy::None`
- [x] 1.4 Run `cargo test -p smedja-adapter`; confirm the Anthropic cache tests still pass unchanged

## 2. CacheAligner — drift tracking

- [x] 2.1 Add a failing test in `crates/smedja-memory` asserting a fresh `CacheAligner` over a newly sealed `WorkingMemory` reports drift `Unchanged` and a breakpoint equal to `stable_prefix()`
- [x] 2.2 Add a failing test asserting that when the next turn's sealed prefix grows while prior messages are byte-identical, the aligner reports `Grown` and advances the breakpoint
- [x] 2.3 Add a failing test asserting that when a message inside the prior boundary mutates, the aligner reports `Mutated` and truncates the breakpoint to before the first changed message (and returns no hint when nothing stable remains)
- [x] 2.4 Implement `CacheAligner` (prior boundary index + per-message digest vector) and the `Drift` classification; export it from `crates/smedja-memory/src/lib.rs`
- [x] 2.5 Run `cargo test -p smedja-memory`; all aligner tests green

## 3. CacheAligner — breakpoint selection and CacheHint

- [x] 3.1 Add a failing test asserting the aligner produces a `CacheHint` carrying the safe breakpoint index and a provider-neutral strategy descriptor
- [x] 3.2 Add a failing test asserting the breakpoint never exceeds the current `stable_prefix()` and is `None` when the stable region is empty
- [x] 3.3 Implement `CacheHint` and breakpoint selection (longest leading digest-stable run, capped at `stable_prefix()`)
- [x] 3.4 Run `cargo test -p smedja-memory`; green

## 4. OpenAI automatic prompt caching

- [x] 4.1 Add a failing test in `crates/smedja-adapter/src/openai.rs` asserting that with `CacheStrategy::OpenAiAutomatic { cache_key: Some(k) }` the request body carries `prompt_cache_key = k`, and that the leading `stable_prefix_len` messages are emitted first in unchanged order
- [x] 4.2 Add a failing test asserting `CacheStrategy::None` produces no `prompt_cache_key` (body byte-compatible with today)
- [x] 4.3 Implement the `build_body` changes: emit `prompt_cache_key` when present; preserve byte-identical leading-prefix ordering
- [x] 4.4 Run `cargo test -p smedja-adapter`; green

## 5. Gemini explicit context caching

- [x] 5.1 Add a failing test in `crates/smedja-adapter/src/gemini.rs` asserting that with `CacheStrategy::GeminiContext { cached_content: Some(name) }` the body sets `cachedContent = name` and omits the cached leading turns from `contents`
- [x] 5.2 Add a failing test asserting that with no handle (`cached_content: None` or `CacheStrategy::None`) `build_contents` is unchanged from today
- [x] 5.3 Implement the `build_contents`/`stream_chat` changes for the cached-handle and fallback paths
- [x] 5.4 Run `cargo test -p smedja-adapter`; green

## 6. Orchestrator wiring

- [x] 6.1 Add a failing orchestrator test asserting that for the Anthropic runner `stable_prefix_len` still equals `mem.stable_prefix()` and `cache_strategy` is `AnthropicEphemeral` (parity with the shipped behaviour)
- [x] 6.2 Add a failing orchestrator test asserting that for the OpenAI runner `cache_strategy` is `OpenAiAutomatic` and for the Gemini runner `cache_strategy` is `GeminiContext`
- [x] 6.3 Build/reuse a per-session `CacheAligner`, run it after `seal_prefix()`, and set `stable_prefix_len` + `cache_strategy` on `CallOptions` from the aligner hint and routed runner, replacing the `if runner == "anthropic"` branch
- [x] 6.4 Add a failing orchestrator test asserting that when the aligner reports `Mutated` with no stable remainder, no cache hint is sent (`stable_prefix_len` falls back to a safe value and `cache_strategy` is `None`)
- [x] 6.5 Run `cargo test -p smdjad`; green

## 7. Verify

- [x] 7.1 Run `cargo test --workspace` — all green
- [x] 7.2 Run `cargo clippy -p smdjad -p smedja-adapter -p smedja-memory -- -D warnings` — clean for the touched crates
- [x] 7.3 Run `openspec validate cache-aligner --strict` — clean

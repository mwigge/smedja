## ADDED Requirements

### Requirement: CacheAligner tracks stable-prefix drift across turns

The system SHALL provide a `CacheAligner` that, given a sealed `WorkingMemory`, records the prior turn's stable-prefix boundary and a per-message content digest for messages within that boundary. On each subsequent turn it MUST classify the drift relative to the prior turn as `Unchanged`, `Grown`, or `Mutated`.

#### Scenario: unchanged prefix is detected

- **WHEN** a turn's sealed prefix has the same boundary and byte-identical messages as the prior turn
- **THEN** the aligner SHALL classify the drift as `Unchanged`
- **AND** the reported breakpoint SHALL equal `WorkingMemory::stable_prefix()`

#### Scenario: grown prefix advances the breakpoint

- **WHEN** the sealed prefix boundary advances while every message inside the prior boundary remains byte-identical
- **THEN** the aligner SHALL classify the drift as `Grown`
- **AND** the reported breakpoint SHALL advance to the new boundary

#### Scenario: mutated prefix truncates the breakpoint

- **WHEN** a message inside the prior boundary has changed since the previous turn
- **THEN** the aligner SHALL classify the drift as `Mutated`
- **AND** the reported breakpoint SHALL be truncated to before the first changed message

### Requirement: CacheAligner selects a safe breakpoint and emits a CacheHint

The `CacheAligner` SHALL emit a `CacheHint` carrying a breakpoint index and a provider-neutral strategy. The breakpoint MUST NOT exceed the current `WorkingMemory::stable_prefix()`, and MUST NOT fall on a message whose digest changed since the prior turn. When no stable leading region remains, the aligner MUST emit no breakpoint.

#### Scenario: breakpoint is capped at the sealed prefix

- **WHEN** the aligner computes a breakpoint
- **THEN** the breakpoint SHALL be less than or equal to `WorkingMemory::stable_prefix()`

#### Scenario: no hint when nothing stable remains

- **WHEN** the leading region has no digest-stable messages versus the prior turn
- **THEN** the emitted `CacheHint` SHALL carry no breakpoint
- **AND** the orchestrator SHALL send no cache hint for that turn

### Requirement: CallOptions carries a provider-neutral cache strategy

`CallOptions` SHALL expose a `cache_strategy` field of type `CacheStrategy` that defaults to `CacheStrategy::None`. The existing `stable_prefix_len` field MUST be retained so the Anthropic adapter behaviour is unchanged.

#### Scenario: default strategy is none

- **WHEN** a `CallOptions` is constructed without setting `cache_strategy`
- **THEN** `cache_strategy` SHALL be `CacheStrategy::None`
- **AND** adapters that do not participate in caching SHALL ignore the field

#### Scenario: Anthropic path retains stable_prefix_len

- **WHEN** the orchestrator targets the Anthropic runner
- **THEN** `stable_prefix_len` SHALL still be set from the aligner's breakpoint
- **AND** `cache_strategy` SHALL be `CacheStrategy::AnthropicEphemeral`
- **AND** the Anthropic adapter SHALL apply `cache_control: ephemeral` exactly as before

### Requirement: OpenAI automatic prompt caching is honoured

The OpenAI adapter SHALL realise an `OpenAiAutomatic` cache strategy by keeping the leading `stable_prefix_len` messages byte-identical and first in the request, and by emitting a `prompt_cache_key` when one is supplied. No per-message cache flag SHALL be added for OpenAI.

#### Scenario: cache key is forwarded

- **WHEN** the strategy is `CacheStrategy::OpenAiAutomatic { cache_key: Some(key) }`
- **THEN** the request body SHALL set `prompt_cache_key` to `key`
- **AND** the leading stable-prefix messages SHALL appear first in unchanged order

#### Scenario: no strategy leaves the body unchanged

- **WHEN** the strategy is `CacheStrategy::None`
- **THEN** the request body SHALL contain no `prompt_cache_key`
- **AND** the messages array SHALL be assembled exactly as without cache alignment

### Requirement: Gemini explicit context caching is honoured

The Gemini adapter SHALL realise a `GeminiContext` cache strategy that carries a cached-content handle by referencing that handle and omitting the cached leading turns from `contents`. When no handle is present, the adapter MUST fall back to building `contents` as without cache alignment.

#### Scenario: cached content is referenced

- **WHEN** the strategy is `CacheStrategy::GeminiContext { cached_content: Some(name) }`
- **THEN** the request body SHALL set `cachedContent` to `name`
- **AND** the cached leading turns SHALL be omitted from `contents`

#### Scenario: missing handle falls back to full contents

- **WHEN** the strategy carries no cached-content handle or is `CacheStrategy::None`
- **THEN** the request body SHALL omit `cachedContent`
- **AND** `contents` SHALL be assembled exactly as without cache alignment

### Requirement: Orchestrator applies cache hints per routed runner

The orchestrator SHALL build or reuse a per-session `CacheAligner`, run it after `WorkingMemory::seal_prefix()`, and set both `stable_prefix_len` (for cache-capable providers) and `cache_strategy` (selected from the routed runner) on `CallOptions`, replacing the prior Anthropic-only branch.

#### Scenario: strategy selected from runner

- **WHEN** a turn is routed to the OpenAI runner
- **THEN** `cache_strategy` SHALL be `CacheStrategy::OpenAiAutomatic`
- **AND** **WHEN** a turn is routed to the Gemini runner the `cache_strategy` SHALL be `CacheStrategy::GeminiContext`

#### Scenario: unsafe drift sends no hint

- **WHEN** the aligner reports `Mutated` with no stable remainder for the routed runner
- **THEN** the orchestrator SHALL set `cache_strategy` to `CacheStrategy::None`
- **AND** SHALL NOT direct any provider to cache a mutated prefix

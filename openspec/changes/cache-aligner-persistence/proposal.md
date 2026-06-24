## Why

The `cache-aligner` change (merged) shipped `CacheAligner` as a per-session, cross-turn drift observer: it carries the prior sealed-prefix boundary and a per-message digest across turns, classifies how the stable prefix drifted (`Unchanged` / `Grown` / `Mutated`), and emits a provider-neutral `CacheHint`. Its entire purpose is to notice that the genuinely-stable region grew (or mutated) between turns so the cache breakpoint advances with it.

But the aligner is currently **inert**. `TurnOrchestrator::run` constructs a fresh `CacheAligner::new()` every turn (`bin/smdjad/src/orchestrator/mod.rs:517`). A fresh aligner has `seen == false`, so on the very first — and therefore *every* — `align` call it takes the no-history branch and returns `CacheHint { breakpoint: prefix_len, drift: Drift::Unchanged }` (`crates/smedja-memory/src/aligner.rs:86`). It never observes a prior turn, so it can never report `Grown` or `Mutated`. Cross-turn drift detection — the one thing the type exists to do — never fires.

The shipped code already *narrows* toward this: `cache_options_for_runner` (`bin/smdjad/src/orchestrator/context.rs:63`) faithfully translates the hint into per-runner cache options, and the doc comment on `CacheAligner` (`aligner.rs:7`) describes it as "a per-session observer that carries the prior boundary across turns." That narrowing is documented but unfinished: nothing carries the observer across turns. This change finishes it by persisting the aligner so it actually observes drift.

## What Changes

- **Add a shared `CacheAligners` map, mirroring `ProviderSessions`.** `ProviderSessions = Arc<Mutex<HashMap<String, String>>>` is constructed once in `main()` (`bin/smdjad/src/main.rs:1146`) and threaded through `build_router` → `HandlerState` → `run_turn`/`spawn_worker`/loop runner → `TurnOrchestrator`. Add `CacheAligners = Arc<Mutex<HashMap<AlignerKey, CacheAligner>>>` the same way, so a single map outlives individual turns.
- **Key the map by `(session_id, runner)`, not session alone.** A cache hint targets one specific provider's warm KV-cache. Keying by session only would let one provider's prefix history bleed onto another after a `provider-failover` runner rotation. `(session_id, runner)` keeps each provider's prefix history separate; a failover to a new runner naturally finds no entry and starts a fresh aligner — correct, because the new provider's cache is cold.
- **Replace the per-turn `CacheAligner::new()` with get-or-insert on the key.** In the ring loop where the runner name is known (`mod.rs:564`, `entry_runner_name`), look up the persisted aligner for `(session_id, entry_runner_name)`, call `align(&mem)` on it, store it back, and feed the resulting hint into `cache_options_for_runner` exactly as today. The per-turn `align` at `mod.rs:517` is removed; alignment moves inside the ring loop because the runner identity is only known there.
- **Thread `CacheAligners` through the four `TurnOrchestrator::new` call sites** (`bin/smdjad/src/main.rs`, `bin/smdjad/src/loop_runner.rs`, and the orchestrator unit tests) plus `HandlerState`, matching the `ProviderSessions` wiring exactly.

## Capabilities

### New Capabilities

- `cross-turn-cache-alignment`: `smdjad` persists one `CacheAligner` per `(session_id, runner)` across turns, so the aligner observes the prior sealed prefix and reports real `Grown`/`Mutated` drift instead of always reporting a fresh `Unchanged` at full prefix. A `provider-failover` runner rotation starts a fresh aligner for the new runner.

## Impact

- `bin/smdjad/src/orchestrator/mod.rs`: add the `CacheAligners` type alias and `AlignerKey`; add the field + constructor parameter to `TurnOrchestrator`; remove the per-turn `CacheAligner::new().align(&mem)` at line 517 and replace it with get-or-insert-and-store inside the ring loop, keyed by `(session_id, entry_runner_name)`; update the test constructor at line ~1490.
- `bin/smdjad/src/main.rs`: construct the `CacheAligners` map alongside `provider_sessions` (~line 1146); thread it through `build_router`, `HandlerState`, `run_turn`, and `spawn_worker`.
- `bin/smdjad/src/handlers/mod.rs`: add `cache_aligners` to `HandlerState`.
- `bin/smdjad/src/loop_runner.rs`: thread `cache_aligners` to the loop's `TurnOrchestrator::new`.
- `bin/smdjad/src/handlers/loops.rs`: forward the map into the loop runner alongside `provider_sessions`.
- Token-economy: persisted alignment produces a `Grown` hint when the stable region grows, advancing the cache breakpoint and yielding more provider cache-read hits — recorded as savings by the existing token-economy accounting (no new accounting code here).

Notes:
- The `cache-aligner` change already shipped the `CacheAligner` type, `cache_options_for_runner`, and the per-runner hint translation. This change adds **only** the cross-turn persistence and keying; it does not change the aligner's classification logic or the cache-option mapping.
- Interaction with `provider-failover` (merged): runner rotation is the explicit motivation for the `(session_id, runner)` key — see `design.md`.

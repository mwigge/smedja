## Context

`CacheAligner` (`crates/smedja-memory/src/aligner.rs`) is a stateful, per-session observer. Its `align(&mut self, memory: &WorkingMemory) -> CacheHint` method (line 81) records `prior_digests` and a `seen` flag and uses them on the next call to classify drift:

- On the first call (`seen == false`, line 86) it returns `CacheHint { breakpoint: prefix_len, drift: Drift::Unchanged }` — the whole sealed prefix is treated as stable, with no comparison.
- On a later call it runs `classify` (line 102): the longest leading digest-stable run versus `prior_digests` decides between `Drift::Grown` (boundary advanced, all prior messages byte-identical), `Drift::Mutated` (a message inside the prior boundary changed), or `Drift::Unchanged`.

So the drift classification is only reachable on the **second and subsequent** `align` calls *against the same aligner instance*.

The daemon side defeats this. `TurnOrchestrator::run` (`bin/smdjad/src/orchestrator/mod.rs`) builds a `WorkingMemory`, pushes the system prompt and pre-turn context, pushes the first user message, calls `seal_prefix()`, and then at line 517 does:

```
let cache_hint = smedja_memory::CacheAligner::new().align(&mem);
```

A fresh aligner per turn means `seen` is always `false`, so the hint is always `Unchanged` at the full prefix. The hint then feeds `cache_options_for_runner` (`bin/smdjad/src/orchestrator/context.rs:63`) inside the ring loop, once per candidate runner. The mapping logic is correct; the input is permanently first-turn.

The existing `ProviderSessions` map shows the established pattern for cross-turn shared state: `Arc<Mutex<HashMap<String, String>>>` (`mod.rs:48`), constructed once in `main()` (`main.rs:1146`), and threaded explicitly through `build_router` (`main.rs:434`), `HandlerState` (`handlers/mod.rs:47`), `run_turn` (`main.rs:269`), `spawn_worker` (`main.rs:300`), the loop runner (`loop_runner.rs:247`), and `TurnOrchestrator::new` (`mod.rs:64`).

## Goals / Non-Goals

Goals:
- Persist one `CacheAligner` across turns so it observes the prior sealed prefix and reports real `Grown`/`Mutated` drift.
- Key persistence by `(session_id, runner)` so one provider's prefix history never contaminates another's.
- Mirror the `ProviderSessions` threading exactly so the wiring is reviewable against a known pattern.
- Leave `CacheAligner`, `CacheHint`, `Drift`, and `cache_options_for_runner` semantics unchanged.

Non-Goals:
- Persisting aligners across daemon restarts. The map is in-memory, like `ProviderSessions`; a restart legitimately starts cold (the provider caches are cold too).
- Changing the aligner's classification algorithm or the per-runner cache-option mapping (owned by the merged `cache-aligner` change).
- New token-economy accounting. Improved cache-read hits flow through the existing accounting unchanged.
- Evicting stale aligner entries. Bounded growth is acceptable at current scale; eviction can be a follow-up if the map is observed to grow unbounded.

## Decisions

**Decision 1: Mirror the `ProviderSessions` pattern.**
Add `pub(crate) type CacheAligners = Arc<Mutex<HashMap<AlignerKey, CacheAligner>>>;` next to `ProviderSessions` in `orchestrator/mod.rs`. Construct it once in `main()` beside `provider_sessions` (`main.rs:1146`) and thread it through the identical chain: `build_router` → `HandlerState` → `run_turn` / `spawn_worker` → loop runner → `TurnOrchestrator::new`. Add it as a `TurnOrchestrator` field and constructor parameter.
- Rationale: the pattern is already proven and reviewed for exactly this lifetime (per-session state outliving a turn, shared across worker tasks via `Arc<Mutex<_>>`). Reusing it minimises review surface and avoids inventing a second mechanism for the same problem.
- Alternative considered: store the aligner inside the per-session resume state. Rejected — that state is keyed by session only and would force the session-only-key mistake (Decision 2); the dedicated map keeps the key under this change's control.

**Decision 2: Key by `(session_id, runner)`, not session alone.**
`AlignerKey` is `(String, String)` of `(session_id, runner_name)`. A `CacheHint` is realised against one specific provider's warm KV-cache by `cache_options_for_runner`; the breakpoint is only meaningful relative to *that* provider's previously-cached prefix.
- A session-only key would smear one provider's prefix-digest history onto another. After a `provider-failover` (merged) runner rotation mid-session, the next turn on the new runner would be compared against the old runner's `prior_digests`, producing a bogus `Grown`/`Unchanged` hint pointed at a cache the new provider never populated — a cache miss dressed up as a hit, and potentially a hint placed past content the new provider has not seen.
- Keying by `(session_id, runner)` means a failover to a new runner finds no entry for `(session, new_runner)` and constructs a fresh aligner via `Default`. That fresh aligner correctly reports first-turn `Unchanged` for the new provider — which is right, because the new provider's cache is cold. When traffic later rotates *back* to the original runner, its preserved entry resumes observing drift from where it left off.
- This is the explicit interaction point with `provider-failover`: the per-runner key is what makes persistence safe under runner rotation.

**Decision 3: Get-or-insert inside the ring loop, then store back.**
The runner name is only known inside the provider ring loop (`mod.rs:564`, `entry_runner_name`), so alignment must move there from its current pre-loop position at line 517. Per candidate runner:
1. Lock the `CacheAligners` map.
2. `remove`/`take` (or get-mut) the aligner for `(session_id.clone(), entry_runner_name.clone())`, defaulting to `CacheAligner::new()` when absent.
3. Call `let cache_hint = aligner.align(&mem);`.
4. Insert the (now-mutated) aligner back under the same key.
5. Release the lock and feed `cache_hint` into `cache_options_for_runner` exactly as today.
- Rationale: `align` takes `&mut self` and must observe the *same* instance across turns. Taking ownership under the lock, aligning, and re-inserting keeps the mutation localised and avoids holding the lock across the provider round-trip.
- Note on retries within a turn: the ring loop iterates runners on failover *within* a single turn. The map is keyed by runner, so re-attempting the same runner in the same turn would `align` the same aligner twice against the same sealed `mem`; the second `align` sees identical digests and reports `Unchanged` — harmless (the breakpoint is unchanged), and the common case is one alignment per runner per turn.

**Decision 4: Ties into token-economy via more cache-read hits.**
Once persisted, a turn whose stable region grew reports `Drift::Grown` with an advanced breakpoint instead of a fixed first-turn breakpoint. `cache_options_for_runner` turns that into a longer `stable_prefix_len` / cache strategy, so the provider serves more of the prompt from its warm cache. Those cache-read tokens are already priced and recorded by the existing token-economy accounting; no accounting code changes here. The value of this change is realised entirely as the difference between a perpetual first-turn hint and an accurate cross-turn one.

## Risks / Trade-offs

- [Risk] Unbounded map growth across many sessions → Mitigation: entries are two small strings plus a `Vec<u64>` of prefix digests; this matches `ProviderSessions`' unbounded-but-small footprint. Eviction is an explicit non-goal/follow-up.
- [Risk] Holding the map lock across the provider call would serialise turns → Mitigation: Decision 3 takes/aligns/re-inserts under the lock and releases before the round-trip, identical to how `provider_sessions` is locked only for the get/put.
- [Risk] A wrong key (session-only) could place a cache hint past content a freshly-rotated provider never cached → Mitigation: this is precisely why Decision 2 keys by `(session_id, runner)`; the failover scenario is covered by a spec scenario.
- [Risk] Moving `align` into the ring loop changes where the hint is computed → Mitigation: the hint already feeds `cache_options_for_runner` inside the ring loop; only the `align` call relocates, and the per-runner key makes computing it per-runner correct rather than redundant.

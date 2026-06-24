## 1. Add the CacheAligners type and key (TDD: red first)

- [x] 1.1 In `bin/smdjad/src/orchestrator/mod.rs`, define `pub(crate) type AlignerKey = (String, String);` (`(session_id, runner_name)`) and `pub(crate) type CacheAligners = Arc<Mutex<HashMap<AlignerKey, smedja_memory::CacheAligner>>>;` directly beneath the `ProviderSessions` alias
- [x] 1.2 Write a failing unit test (in the `orchestrator::mod` test module) that aligns the same persisted aligner twice against two sealed `WorkingMemory` snapshots for the same `(session, runner)` key and asserts the second `align` reports `Drift::Grown` (not a fresh `Drift::Unchanged`) — this fails today because no persistence exists

## 2. Thread CacheAligners into TurnOrchestrator

- [x] 2.1 Add a `cache_aligners: CacheAligners` field to the `TurnOrchestrator` struct
- [x] 2.2 Add `cache_aligners: CacheAligners` as the final parameter of `TurnOrchestrator::new` and assign it in the constructed `Self`
- [x] 2.3 In `TurnOrchestrator::run`, bind `let cache_aligners = &self.cache_aligners;` alongside the existing `let provider_sessions = &self.provider_sessions;`
- [x] 2.4 Update the orchestrator unit-test constructor (`orchestrator/mod.rs` ~line 1490) to build an empty `CacheAligners` map and pass it to `TurnOrchestrator::new`

## 3. Replace the per-turn aligner with get-or-insert keyed by (session, runner)

- [x] 3.1 Remove the per-turn `let cache_hint = smedja_memory::CacheAligner::new().align(&mem);` at `orchestrator/mod.rs:517`
- [x] 3.2 Inside the ring loop, where `entry_runner_name` is known, build the key `(session_id.clone(), entry_runner_name.clone())`, lock `cache_aligners`, take-or-default (`CacheAligner::new()`) the aligner for that key, call `let cache_hint = aligner.align(&mem);`, re-insert the mutated aligner under the same key, and drop the lock before the provider call
- [x] 3.3 Confirm `cache_hint` continues to feed `context::cache_options_for_runner(&entry_runner_name, cache_hint, openai_cache_key, None)` unchanged

## 4. Thread CacheAligners through main.rs

- [x] 4.1 In `bin/smdjad/src/main.rs`, construct `let cache_aligners: orchestrator::CacheAligners = Arc::new(Mutex::new(HashMap::new()));` beside `provider_sessions` (~line 1146)
- [x] 4.2 Add a `cache_aligners` parameter to `build_router` and include `cache_aligners: Arc::clone(cache_aligners)` in the `HandlerState` literal
- [x] 4.3 Add a `cache_aligners` parameter to `run_turn` and forward it to `TurnOrchestrator::new`
- [x] 4.4 Add a `cache_aligners` parameter to `spawn_worker`, clone it per spawned task (`let ca = Arc::clone(&cache_aligners);`), and pass it to `run_turn`
- [x] 4.5 Pass `&cache_aligners` into the `build_router` and `spawn_worker` calls in `main()`

## 5. Thread CacheAligners through HandlerState and the loop runner

- [x] 5.1 In `bin/smdjad/src/handlers/mod.rs`, add `pub(crate) cache_aligners: ProviderSessions`-style field `pub(crate) cache_aligners: crate::orchestrator::CacheAligners` to `HandlerState`
- [x] 5.2 In `bin/smdjad/src/handlers/loops.rs`, forward `Arc::clone(&state.cache_aligners)` into the loop runner alongside `provider_sessions`
- [x] 5.3 In `bin/smdjad/src/loop_runner.rs`, add the `cache_aligners` field/parameter and pass it to the loop's `TurnOrchestrator::new`

## 6. Failover key behaviour

- [x] 6.1 Add a unit test asserting that aligning under `(session, "anthropic")` then under `(session, "openai")` does not share digest history: the `"openai"` first turn reports `Drift::Unchanged` (fresh aligner) even though `"anthropic"` already observed a grown prefix for the same session
- [x] 6.2 Add a unit test for the mutated case: two turns under the same `(session, runner)` where a message inside the prior boundary changed → second turn reports `Drift::Mutated`

## 7. Verify

- [x] 7.1 Run `cargo test -p smdjad` — all green, including the new persistence, failover, and mutation tests
- [x] 7.2 Run `cargo clippy -p smdjad -- -D warnings` — clean for the touched code
- [x] 7.3 Run `openspec validate cache-aligner-persistence --strict` — clean

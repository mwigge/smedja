## Why

The README and CHANGELOG no longer match the code. Two opposite failures coexist, and both mislead a reader or an agent deciding what they can rely on.

**Delivered work is still labelled roadmap or "in progress."** Recently-merged changes (`wire-loop`, `wire-memory`, `wire-methodology`, `sre-hardening`, `smdjad-decompose`, `async-correctness`) shipped behaviour the docs still hedge on:

- `loop.run` now drives the real `smedja-loop` engine — it loads `.smedja/loop.json`, verifies the policy hash, routes roles by tier, runs the verification gate, and spawns `crate::loop_runner::run` (`bin/smdjad/src/handlers/loops.rs:158`). The README Loop Pipeline section describes this as the intended design without stating it is now wired; the CHANGELOG `0.10.1` does not record it at all.
- `WorkingMemory` is wired into the orchestrator: per-tier `set_strata`, `seal_prefix()`, `build_prompt(budget)`, real `stable_prefix_len` for Anthropic (`Some(mem.stable_prefix())`, not the old `Some(0)`), `inject_conciseness`, and `compress_tool_result` all run on the live turn path (`bin/smdjad/src/orchestrator/mod.rs:434,502,517,563,566,822`). The README still says the stable-prefix/CacheAligner provider integration "is on the roadmap" (`README.md:170`).
- `smj session rollback` is wired end to end: the daemon registers `session.rollback` (`bin/smdjad/src/handlers/checkpoint.rs:53`) and the CLI calls it (`bin/smj/src/main.rs:1649`). The README still says "`smj session rollback` is on the roadmap" (`README.md:152`), and the CHANGELOG lists it under "Roadmap (not yet implemented)" (`CHANGELOG.md:51`).
- `sd_notify(READY=1)` and an unauthenticated `/health` readiness probe ship (`bin/smdjad/src/main.rs:852,1199`; `bin/smdjad/src/acp.rs:45,50`). Neither the README Observability section nor the CHANGELOG mentions readiness/health.

**A stub-dependent caveat is now over-pessimistic and self-contradictory.** The README warns that `smedja_vault_search` "currently returns empty results — daemon wiring is in progress" (`README.md:156`) and the CHANGELOG repeats it (`CHANGELOG.md:37,45`). The executor actually embeds the query, calls `Vault::search`, and returns ranked `{ "results": [...] }` (`bin/smdjad/src/executor/mod.rs:248`), with a passing test that the tool returns results for a populated vault (`bin/smdjad/src/executor/mod.rs:858`). The genuinely-incomplete piece is different and narrower: automatic cold-stratum recall into the prompt. `WorkingMemory::cold_context()` is still a hard stub returning `Vec::new()` (`crates/smedja-memory/src/memory.rs:226`), `build_prompt` explicitly skips cold turns (`memory.rs:204`), and the orchestrator injects no cold-context block. The docs conflate "the tool is broken" with "automatic recall is not wired," and as written they advertise the wrong half: a reader avoids a working tool while assuming history is silently recalled when it is not.

This change is a documentation-accuracy pass only: align the README and CHANGELOG with the code, mark genuinely-incomplete items as roadmap, and stop advertising a stubbed capability (automatic cold-stratum recall) as available while correcting the unfair caveat on the working `smedja_vault_search` tool.

## What Changes

- **Loop Pipeline (`README.md:74-140`)**: state that `loop.run` now drives the `smedja-loop` engine (policy-hash verification, role tier routing, verification gate, bounded fix retries) rather than describing it only as intended design. Add a CHANGELOG entry recording the wiring.
- **Context Budget Control (`README.md:166-173`)**: drop the "CacheAligner / BuildPrompt integration is on the roadmap" caveat — `stable_prefix_len` is derived from the sealed prefix and the Anthropic adapter applies the cache hints on the live path. Keep SmartCrusher and verbosity steering described as implemented (already accurate).
- **Session Memory (`README.md:152`)**: remove the "`smj session rollback` is on the roadmap" note; the command and its `session.rollback` RPC are implemented.
- **Vault / cold stratum (`README.md:156`)**: rewrite the caveat. State that the `smedja_vault_search` tool is available and returns ranked results from a populated vault. State separately and clearly that **automatic cold-stratum recall into the prompt is roadmap** — `cold_context()` is a stub, the orchestrator injects no cold block, so history beyond the warm window is not silently recalled. Do not advertise automatic recall as available.
- **Observability (`README.md:222-231`)**: note the `sd_notify(READY=1)` startup signal and the unauthenticated `/health` readiness probe now shipped by `sre-hardening`.
- **CHANGELOG (`CHANGELOG.md`)**: move `smj session rollback` out of "Roadmap"; correct the `smedja_vault_search` line (`:37,:45`) and the `--sock` claim (`:15`, which asserts the flag "does not exist" — it is a `clap` arg on both `smj` and `smedja-tui`, `bin/smj/src/main.rs:32`, `bin/smedja-tui/src/main.rs:42`); add entries for the loop-engine wiring, the stable-prefix cache-hint wiring, and the readiness/health probe; keep automatic cold-stratum recall and the items still genuinely unbuilt (Glyph Protocol PUA, background-image blit, MCP OAuth, inline cowork widget) under "Roadmap."
- **No prose claim of an available tool or feature may rest on a handler that returns empty/stub results.** Every "available" claim in the README is re-checked against its handler; the only stub-backed capability is automatic cold-stratum recall, which moves to roadmap.

## Capabilities

### Modified Capabilities

- `documentation-accuracy`: the README and CHANGELOG must reflect the actually-delivered code — delivered features are not labelled roadmap, and no tool or capability backed by a stub handler is advertised as available.

## Impact

- `README.md`: Loop Pipeline, Context Budget Control, Session Memory, vault/cold-stratum, and Observability sections corrected. No code changes.
- `CHANGELOG.md`: roadmap list pruned of delivered items; stale `smedja_vault_search`/`--sock` claims corrected; loop-engine, stable-prefix, and readiness/health entries added.

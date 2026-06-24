## 1. README — Loop Pipeline

- [x] 1.1 In the Loop Pipeline section (`README.md:74-140`), state that `loop.run` now drives the real `smedja-loop` engine (`bin/smdjad/src/handlers/loops.rs:158`): it loads `.smedja/loop.json`, verifies the policy hash, routes roles by configured runner/tier, runs the verification gate per slice, and applies bounded fix retries — not just intended design. (Already delivered in current README; verified `crate::loop_runner::run` at `loops.rs:194`.)
- [x] 1.2 Confirm the `.smedja/` workspace-layout and `loop.json` policy tables (`README.md:94-140`) match the engine the handler invokes; correct any field that no longer matches. (Verified accurate; no field drift.)

## 2. README — Context Budget Control

- [x] 2.1 In the Stable-prefix / CacheAligner paragraph (`README.md:170`), remove the "adapter-side `BuildPrompt` integration … is on the roadmap" claim; state that `stable_prefix_len` is derived from the sealed prefix (`bin/smdjad/src/orchestrator/mod.rs:517`) and the Anthropic adapter applies the cache hints on the live path. (Already corrected in current README; verified `stable_prefix_len: if entry_runner_name == "anthropic" { Some(mem.stable_prefix()) }` at `mod.rs:606`.)
- [x] 2.2 Verify SmartCrusher (`README.md:168`) and verbosity steering (`README.md:172`) remain described as implemented — both run live (`orchestrator/mod.rs:822,566`); leave accurate text unchanged. (Verified `inject_conciseness` at `mod.rs:633`, `compress_tool_result` at `mod.rs:896`; text left unchanged.)

## 3. README — Session Memory

- [x] 3.1 Remove the "`smj session rollback` is on the roadmap" note (`README.md:152`); the `session.rollback` RPC (`bin/smdjad/src/handlers/checkpoint.rs:53`) and `smj session rollback` CLI (`bin/smj/src/main.rs:1649`) are implemented. Keep the structured-compaction-format sentence (accurate). (Already corrected in current README; verified RPC at `checkpoint.rs:53` and CLI at `main.rs:1968`.)

## 4. README — Vault / cold stratum (do not advertise a stub)

- [x] 4.1 Rewrite the parallel-memory caveat (`README.md:156`): state that the `smedja_vault_search` tool is available and returns ranked results from a populated vault (`bin/smdjad/src/executor/mod.rs:248`, test `:858`); drop "currently returns empty results — daemon wiring is in progress." (Already corrected in current README; verified `guard.search(...)` at `executor/mod.rs:326` and test at `:1275`.)
- [x] 4.2 SUPERSEDED by wire-cold-context (#57) merge. The task asked to mark automatic cold-stratum recall as roadmap and call `cold_context()` a stub. That is now WRONG: `cold_context()` dispatches to a `ColdStore` (`crates/smedja-memory/src/memory.rs:291`), the daemon supplies a vault-backed `VaultColdStore` (`bin/smdjad/src/orchestrator/cold.rs`), and `TurnOrchestrator` injects a per-tier, budget-capped `<cold_context>` block (`bin/smdjad/src/orchestrator/mod.rs:419-508`, `cold_k_for_tier`, `assemble_cold_block`). Current README (`:158`) already describes cold recall as delivered. Left as delivered; no roadmap caveat re-added.

## 5. README — Observability

- [x] 5.1 In the Observability section (`README.md:222-231`), add that the daemon signals readiness via `sd_notify(READY=1)` (`bin/smdjad/src/main.rs:852,1199`) and exposes an unauthenticated `/health` readiness probe returning `200` (`bin/smdjad/src/acp.rs:45,50`). (Already present in current README `:234`; verified `sd_notify_ready()` at `main.rs:864/1229` and `/health` → `StatusCode::OK` at `acp.rs:59`.)

## 6. CHANGELOG corrections

- [x] 6.1 Move `smj session rollback` out of "Roadmap (not yet implemented)" into a delivered "Added" entry. (Already done in current CHANGELOG `[0.10.1]` Added `:32`; no longer in Roadmap.)
- [x] 6.2 Correct the `smedja_vault_search` lines: the tool returns ranked results; cold-stratum recall split. (Vault-search Added/Changed entries already accurate at CHANGELOG `:27,:41`.) PARTIALLY SUPERSEDED by wire-cold-context (#57): automatic cold-stratum recall is no longer roadmap — added a delivered `[0.13.0]` Added entry for it.
- [x] 6.3 Correct the `--sock` claim (`CHANGELOG.md:15`) — the flag exists as a `clap` arg on `smj` (`bin/smj/src/main.rs:32`) and `smedja-tui` (`bin/smedja-tui/src/main.rs:42`); `SMEDJA_SOCK` is the override, not the only path. (Already corrected in current CHANGELOG `:27`.)
- [x] 6.4 Add "Added" entries for: `loop.run` driving the `smedja-loop` engine; stable-prefix KV-cache hints wired to the Anthropic adapter; `sd_notify(READY=1)` + `/health` readiness probe. (Already present in current CHANGELOG `[0.10.1]` Added `:30,:31,:33`.)
- [x] 6.5 Reconcile the remaining "Roadmap" list. SUPERSEDED in part by merges #57 (cold recall now delivered) and #62 (MCP OAuth now delivered) — neither appears in the current CHANGELOG Roadmap, which is correct. Verified the remaining Roadmap items are genuinely unbuilt and kept them: inline cowork approval widget (y/n/m), Glyph Protocol PUA registration, background-image GPU blit, session-resume `--session` picker, cross-provider CacheAligner, metrics rollup dashboard, local-model install/swap UX, `@file`/`@git`/`@shell` context fragments. Added a `[0.13.0]` Added section documenting the newly-delivered cold recall, provider failover, security plane, repo auditor, MCP server mode, tool sandbox, and eval harness.

## 7. Verify

- [x] 7.1 Re-read each corrected README claim against the cited file:line and confirmed the code matches on CURRENT main (loop engine `loops.rs:194`; `stable_prefix_len` `mod.rs:606`; `session.rollback` `checkpoint.rs:53` + `main.rs:1968`; `smedja_vault_search` `executor/mod.rs:326`; cold recall delivered via `memory.rs:291` + `orchestrator/cold.rs`; `sd_notify_ready` `main.rs:864`; `/health` `acp.rs:59`).
- [x] 7.2 Confirmed no "available"/"implemented" claim in README or CHANGELOG is backed by a stub handler. `cold_context()` is no longer a stub (returns ranked `ColdStore` results); remaining roadmap items (cross-provider CacheAligner, inline cowork widget, background-image blit, Glyph PUA) are correctly described as roadmap.
- [x] 7.3 Ran `openspec validate readme-truth-up --strict` — clean.

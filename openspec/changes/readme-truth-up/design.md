## Context

The README and CHANGELOG drifted from the code in two directions at once. The drift is not random: each individual caveat was true at the moment it was written, but the merged changes (`wire-loop`, `wire-memory`, `wire-methodology`, `sre-hardening`, `smdjad-decompose`, `async-correctness`) closed the gaps the caveats described, and the docs were not updated in lockstep. The result is a document that simultaneously under-promises (delivered features still marked roadmap) and over-promises (a stub-dependent capability implied to be available).

The concrete code state, verified against the tree:

- `loop.run` → `crate::loop_runner::run` over the real engine (`bin/smdjad/src/handlers/loops.rs:158`); the old inline checkbox iterator is gone.
- Orchestrator drives `WorkingMemory`: `set_strata`/`seal_prefix`/`build_prompt` and `stable_prefix_len: if runner == "anthropic" { Some(mem.stable_prefix()) }` (`bin/smdjad/src/orchestrator/mod.rs:434,502,517,563`); `inject_conciseness` (`:566`) and `compress_tool_result` (`:822`) run live.
- `session.rollback` RPC (`bin/smdjad/src/handlers/checkpoint.rs:53`) and `smj session rollback` CLI (`bin/smj/src/main.rs:1649`) both exist.
- `sd_notify_ready()` emits `READY=1` (`bin/smdjad/src/main.rs:852,1199`); `/health` returns `200` unauthenticated (`bin/smdjad/src/acp.rs:45,50`).
- `--sock` is a real `clap` arg on `smj` (`bin/smj/src/main.rs:32`) and `smedja-tui` (`bin/smedja-tui/src/main.rs:42`).
- `smedja_vault_search` executor calls `Vault::search` and returns ranked `{ "results": [...] }` (`bin/smdjad/src/executor/mod.rs:248`), test-proven for a populated vault (`:858`).
- `WorkingMemory::cold_context()` is still `Vec::new()` (`crates/smedja-memory/src/memory.rs:226`); `build_prompt` skips cold turns (`:204`); the orchestrator injects no cold-context block. Automatic recall is genuinely absent and is the subject of the separate, still-proposed `wire-cold-context` change.

## Goals / Non-Goals

Goals:
- Make every "available"/"implemented" claim in the README and CHANGELOG match a real, reachable handler.
- Move delivered features (loop engine, stable-prefix cache hints, `smj session rollback`, readiness/health) out of roadmap framing.
- Replace the unfair `smedja_vault_search` "returns empty" caveat with an accurate split: the tool is available; automatic cold-stratum recall is roadmap.
- Keep genuinely-unbuilt items (automatic cold recall, Glyph Protocol PUA, background-image blit, MCP OAuth, inline cowork widget) clearly marked as roadmap.

Non-Goals:
- No code changes — README/CHANGELOG/Rust are untouched by this proposal; this artifact set is the proposal only.
- Not implementing automatic cold-stratum recall (owned by `wire-cold-context`); this change only stops mis-advertising it.
- Not redrawing diagrams or rewriting marketing prose beyond the specific inaccurate claims.

## Decisions

**Decision: a single source-of-truth principle — the code is canonical; the docs describe it, never ahead of it.**
Every capability statement in the README and CHANGELOG falls into exactly one of two buckets, and the bucket is decided by the handler, not by intent:

- **Available** — there is a reachable, non-stub handler/command on the live path. The reader may rely on it. Delivered features that close a prior caveat move here.
- **Roadmap** — the handler is a stub (returns empty/fixed/`Vec::new()`), absent, or behind an unimplemented path. The reader must not rely on it. Such an item may be described as planned but MUST NOT be presented as available.

The disqualifying test for "available" is whether the backing handler returns real results. `cold_context()` returning `Vec::new()` fails that test, so automatic cold-stratum recall is roadmap even though adjacent machinery (the vault, the `smedja_vault_search` tool) is available. Conversely, `smedja_vault_search` passes the test, so its over-pessimistic caveat is removed.

- Rationale: an agent or user that calls a tool which silently returns empty is worse off than one told the tool does not exist — it builds on absent data. The principle makes the doc safe to act on.
- Alternative considered: a coarse per-section "mostly done" disclaimer. Rejected — it neither tells the reader which specific calls are safe nor satisfies the spec requirement that stub-backed capabilities not be advertised as available.

**Decision: distinguish the working tool from the missing automatic behaviour in the same paragraph.**
The vault correction must not over-correct into claiming history is recalled automatically. The doc states two facts side by side: (1) `smedja_vault_search` is callable and returns ranked results; (2) automatic cold-stratum recall into the prompt is roadmap. Keeping both in view prevents swapping one inaccuracy for its opposite.

## Risks / Trade-offs

- [Risk] Over-correcting the vault caveat into implying automatic recall works → Mitigation: the requirement and the doc text explicitly keep cold-context recall on the roadmap and name `cold_context()` as a stub.
- [Risk] The proposal lands before a referenced feature is actually merged, re-introducing drift → Mitigation: each "available" claim cites a file:line on `main`; the verification group re-reads each corrected claim against the code before this change is considered correct.
- [Risk] Future merges re-open a caveat this change closed → Mitigation: the `documentation-accuracy` requirement is durable — it binds future doc edits to the same handler-backed test, not just this snapshot.

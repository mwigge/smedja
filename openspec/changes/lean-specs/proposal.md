## Why

Today a spec restates its full context inside itself. When a single idea fans out into several related changes, the Why, the design rationale, and the contract are re-stated once per change. The model re-reads that shared context N times — once per change — even though it is the *same* context. The cost is paid N times for a payload that is identical N−1 times over.

The smedja memory architecture already solves this exact shape of problem for conversation history: a durable, rarely-changing core is sealed into a cached stable prefix (`WorkingMemory::seal_prefix`, `crates/smedja-memory/src/memory.rs:139`) and re-sent cheaply each turn, while the mutating tail rotates and overflow is pushed to a vault-backed cold stratum recalled on demand (`WorkingMemory::cold_context`, `crates/smedja-memory/src/memory.rs:291`; `Vault::search`, `crates/smedja-vault/src/vault.rs:289`). The loop already consumes a change as a list of thin work units — the unchecked `- [ ]` lines of `tasks.md` (`bin/smdjad/src/loop_runner.rs:175-187`).

This change applies that same hot/warm/cold economy to specs themselves. A spec is split into an **umbrella** and its **slices**:

- The **umbrella** holds the durable trail of thought — idea, intent, rough direction — across `proposal.md` + `design.md` + `tasks.md`. It is read once. Its `tasks.md` lists the slices at a coarse level, not as granular steps.
- Each **slice** is a thin child unit carrying only its own delta and acceptance criteria plus a pointer back to the umbrella. A slice does NOT restate the umbrella's Why or design.

The token win is *where context lives*: N related changes re-state full context N times; an umbrella plus slices pays the big context once and each slice is a thin delta on top of it.

## What Changes

- **Introduce the umbrella spec.** An umbrella is an ordinary change whose `tasks.md` slice list (the coarse `- [ ]` groups read by `bin/smdjad/src/loop_runner.rs:175`) enumerates its slices. Its durable content — intent/contract (small, stable) and design detail (large, variable) — is the shared context every slice draws on.
- **Introduce spec slices.** A slice is a thin spec unit carrying its own delta plus acceptance criteria and a pointer to its umbrella. The pointer is metadata (`umbrella_id`, `slice_n`), modelled on the existing vault payload convention (`payload {"kind":...}`, `bin/smdjad/src/executor/mod.rs:182`) — not bespoke OpenSpec parent machinery, because the change manifest is flat (`openspec/changes/<name>/.openspec.yaml` carries only `schema` + `created`, no parent field).
- **Store the umbrella in the vault for retrieval.** The umbrella's design detail is chunked into vault entries under an `umbrella:<id>` namespace via the existing `Vault::insert`/`upsert` path (`crates/smedja-vault/src/vault.rs:149`), each entry's `payload` recording `{"kind":"umbrella","umbrella_id":...}`, so a slice can resolve and recall its umbrella by id.
- **Load context hybrid (the crux).** A slice loads the umbrella *intent/contract* from the sealed stable prefix (KV-cached, cheap to re-send each slice via `seal_prefix`/`stable_prefix`, `crates/smedja-memory/src/memory.rs:139`) and the umbrella *design detail* from the vault on demand via `cold_context`/`Vault::search` — the cold stratum shipped by `wire-cold-context`. This is smedja's hot/warm/cold strata applied to specs, and it is the same idea as cache alignment (`crates/smedja-memory/src/aligner.rs:46`): the umbrella stays put in the cached prefix; slices rotate as the mutable tail.
- **The loop consumes umbrella-once + slice-each.** Formalise the existing `tasks.md` slice consumption: an umbrella's `tasks.md` is the slice list; each slice expands to a thin spec; the loop loads the umbrella intent (cached) once and the slice (thin) each iteration.
- **Self-measure.** lean-specs records its own savings (full-spec-paste tokens − umbrella-retrieved tokens) on the existing tokens-saved ledger (`TokensSavedEntry`, `bin/smdjad/src/executor/mod.rs:226`; `insert_tokens_saved`, `crates/smedja-ingot/src/handle.rs:631`) tagged `source=lean-spec`, feeding the token-economy ledger owned by a sibling proposal.

Out of scope (referenced only): replacing the FNV-1a bag-of-words embedder (`bin/smdjad/src/embedder.rs:3`, `DIM = 128`) with a learned model — recall stays weak and is partially compensated by the hybrid keyword boost in `Vault::search`; the token-economy ledger aggregation surface, owned by a sibling proposal; persisting `WorkingMemory` across daemon restarts.

## Capabilities

### New Capabilities

- `umbrella-spec`: a change MAY be authored as an umbrella whose durable intent/contract and design detail are the shared context for its slices, and whose `tasks.md` lists the slices coarsely. The umbrella's content is stored as chunked vault entries under an `umbrella:<id>` namespace so slices can resolve it by id.
- `spec-slices`: a slice is a thin child spec carrying only its own delta and acceptance criteria plus a pointer (`umbrella_id`, `slice_n`) to its umbrella, modelled on the vault payload convention. A slice MUST NOT restate the umbrella's Why or design.
- `slice-context-loading`: a slice loads umbrella intent/contract from the sealed, KV-cached stable prefix and umbrella design detail from the vault on demand via cold retrieval — the hybrid prefix+vault loading that is smedja's hot/warm/cold strata applied to specs.

## Impact

- `crates/smedja-vault/src/vault.rs`: umbrella content stored as chunked entries under an `umbrella:<id>` namespace via `insert`/`upsert`; slices resolved via `search`/`query`. No API change — uses the existing `payload`/`namespace` convention.
- `crates/smedja-memory/src/memory.rs`: umbrella intent sealed into the stable prefix (`seal_prefix`/`stable_prefix`); umbrella detail recalled via `cold_context` against an `umbrella:<id>` cold-query namespace (`set_cold_query`).
- `bin/smdjad/src/loop_runner.rs`: the existing `read_pending_slices` coarse `- [ ]` reading is the slice list; the loop loads umbrella-once + slice-each.
- `bin/smdjad/src/executor/mod.rs`: the `payload {"kind":...}` pointer convention is reused for the umbrella link; lean-specs savings recorded on the tokens-saved ledger tagged `source=lean-spec`.
- OpenSpec authoring convention: umbrella `tasks.md` is a coarse slice list; each slice is a thin delta that points to its umbrella rather than restating it.

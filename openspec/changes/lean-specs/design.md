## Context

smedja already pays the "shared context once" economy for conversation history; this change applies the same machinery to specs.

The relevant existing surface:

- **Vault payload convention.** `VaultEntry` carries an arbitrary JSON `payload` and a `namespace` (`crates/smedja-vault/src/vault.rs:8-16`). The daemon already uses this to tag stored content with a kind discriminator: the output-filter recovery tee writes `payload: {"kind":"filter-recovery"}` under `FILTER_RECOVERY_NAMESPACE` (`bin/smdjad/src/executor/mod.rs:111`, `:182`), and the generic vault-store tool path threads `namespace` + `payload` straight through (`bin/smdjad/src/executor/mod.rs:490-517`). `Vault::insert`/`upsert` (`crates/smedja-vault/src/vault.rs:149`, `:245`) and `Vault::search`/`query` (`:289`, `:504`) are namespace-scoped.
- **The flat change manifest.** Each change's `openspec/changes/<name>/.openspec.yaml` carries only `schema` and `created` — there is no `parent` field. OpenSpec changes are siblings in a flat directory; there is no native parent/child link.
- **The loading machinery.** `WorkingMemory::seal_prefix()` freezes the leading message count as the stable prefix (`crates/smedja-memory/src/memory.rs:139`); `stable_prefix()` returns it (`:146`) and drives provider KV-cache hints. `cold_context(query)` (`:291`) dispatches to an attached `ColdStore` over a configured namespace + top-K (`set_cold_query`, `:108`); the vault-backed adapter and end-to-end wiring shipped in `wire-cold-context`. `CacheAligner` (`crates/smedja-memory/src/aligner.rs:46`) tracks stable-prefix drift across turns and emits a safe cache breakpoint.
- **The loop's slice consumption.** `read_pending_slices` (`bin/smdjad/src/loop_runner.rs:175-187`) reads a change's `tasks.md`, keeps lines starting with `- [ ] `, and feeds them as the `slices` vector to `smedja_loop::drive` (`:233`, `:254`). The engine runs one role per slice (`crates/smedja-loop/src/engine.rs:138`, `:178`).
- **The savings ledger.** `record_tokens_saved` writes a `TokensSavedEntry` keyed by session/command (`bin/smdjad/src/executor/mod.rs:209-234`) through `insert_tokens_saved` (`crates/smedja-ingot/src/handle.rs:631`) into `tokens_saved_ledger` (`crates/smedja-ingot/src/lib.rs:177`). The entry today carries `command` but no dedicated `source` column.
- **The embedder.** `embed(text)` is FNV-1a bag-of-words, fixed `DIM = 128`, L2-normalised (`bin/smdjad/src/embedder.rs:3`, `:9`, `:19`). `Vault::search` adds a keyword + recency boost over cosine (`crates/smedja-vault/src/vault.rs:289`).

## Goals / Non-Goals

Goals:

- Define the umbrella + slice authoring shape so shared context is written and paid for once.
- Link a slice to its umbrella with a pointer (`umbrella_id`, `slice_n`) reusing the vault payload convention — no new OpenSpec parent machinery.
- Store the umbrella as chunked vault entries under an `umbrella:<id>` namespace so a slice can resolve and recall it.
- Load umbrella intent from the cached stable prefix and umbrella detail from the vault on demand (the hybrid).
- Have the loop consume umbrella-once + slice-each over the existing `tasks.md` slice list.
- Measure lean-specs' own savings and tag them `source=lean-spec` for the token-economy ledger.

Non-Goals:

- Replacing the FNV-1a embedder with a learned model (future; recall stays weak).
- Building the token-economy ledger aggregation surface (sibling proposal; this change only emits tagged rows).
- Adding a `parent` field to the OpenSpec manifest or change schema.
- Persisting `WorkingMemory` across daemon restarts.

## Decisions

**Decision 1: Linking is a pointer, not bespoke OpenSpec parent machinery.**
A slice's link to its umbrella is metadata — an `umbrella_id` and a `slice_n` — modelled on the smedja-vault payload convention already in use. The recovery tee stores `payload: {"kind":"filter-recovery"}` under a named namespace (`bin/smdjad/src/executor/mod.rs:182`); the umbrella link reuses exactly that shape: the umbrella's content is stored as chunked vault entries in an `umbrella:<id>` namespace, each entry's `payload` recording `{"kind":"umbrella","umbrella_id":<id>}`, and each slice records its own `umbrella_id` + `slice_n`.
- Rationale: the change manifest (`.openspec.yaml`) is flat — `schema` + `created` only, no parent field. Inventing a parent edge in the OpenSpec schema would be heavy and non-portable; a payload pointer rides machinery that already exists and is already tested.
- Alternative considered: a native OpenSpec `parent` field. Rejected — it would fork the manifest schema for one workflow; the vault pointer is sufficient and reuses the proven `payload`/`namespace` convention.

**Decision 2: Loading is HYBRID — the crux.**
Umbrella *intent/contract* (small, stable) is pinned in the sealed stable prefix: it is pushed before `seal_prefix()` (`crates/smedja-memory/src/memory.rs:139`) so it is KV-cached and cheap to re-send on every slice. Umbrella *design detail* (large, variable) is stored as vault chunks and retrieved per slice on demand via `cold_context`/`Vault::search` (`crates/smedja-memory/src/memory.rs:291`; `crates/smedja-vault/src/vault.rs:289`) — the cold stratum shipped by `wire-cold-context`, with the slice's cold-query namespace set to `umbrella:<id>` (`set_cold_query`, `:108`).
- Rationale: this is smedja's hot/warm/cold strata applied to specs, and it is the same idea as cache alignment (`crates/smedja-memory/src/aligner.rs:46`) — the umbrella stays put in the cached prefix; slices rotate as the mutable tail. Small-stable goes in the cached prefix; large-variable goes to cold recall, so each slice re-sends only the cheap intent and pulls detail only when needed.
- Alternative considered: paste the whole umbrella into each slice's prompt. Rejected — that is precisely the N-times restatement this change removes.

**Decision 3: The loop consumes umbrella-once + slice-each.**
`smedja-loop` already reads a change's `tasks.md` unchecked `- [ ]` lines as work slices (`bin/smdjad/src/loop_runner.rs:175-187`, fed to `smedja_loop::drive` at `:254`). Formalise it: the umbrella's `tasks.md` is the coarse slice list; each slice expands to a thin spec; the loop loads umbrella intent (cached, once) and the slice (thin, each iteration).
- Rationale: no new consumption mechanism is needed — the existing per-slice engine loop (`crates/smedja-loop/src/engine.rs:178`) already iterates slices one at a time, which is exactly the umbrella-once + slice-each cadence.
- Alternative considered: a new umbrella-aware loop driver. Rejected — the coarse `tasks.md` list already is the slice list.

**Decision 4: Self-measuring.**
lean-specs records its own savings — full-spec-paste tokens minus umbrella-retrieved tokens — on the existing tokens-saved ledger (`record_tokens_saved`, `bin/smdjad/src/executor/mod.rs:209`; `insert_tokens_saved`, `crates/smedja-ingot/src/handle.rs:631`), tagged `source=lean-spec` so the token-economy ledger (a sibling proposal) can attribute the saving to this mechanism.
- Rationale: the saving must be observable to justify the mechanism; the ledger already exists and `estimate_tokens` (`crates/smedja-memory`) already computes the delta. The `TokensSavedEntry` carries no dedicated `source` column today (`bin/smdjad/src/executor/mod.rs:226`), so the `source=lean-spec` tag is carried in the entry's tagging field and the dedicated column is owned by the token-economy sibling proposal.
- Alternative considered: a separate lean-specs metrics store. Rejected — duplicates the ledger that already records the same class of saving.

**Decision 5: Caveat — retrieval recall is a good first cut, not precision.**
The vault embedder is FNV-1a bag-of-words at `DIM = 128` (`bin/smdjad/src/embedder.rs:3`, `:9`) — weak semantic recall. The hybrid keyword + recency boost in `Vault::search` (`crates/smedja-vault/src/vault.rs:289`) partially compensates, and the umbrella intent that matters most is in the always-present cached prefix, not in cold recall. A better embedder is a future. So retrieval-linking the umbrella detail is a good first cut, stated honestly, not precision recall.
- Rationale: the design must not over-claim. The cached-prefix half of the hybrid is exact; only the cold-detail half depends on weak embeddings, and it is supplementary.

## Risks / Trade-offs

- [Risk] Weak FNV-1a recall fetches the wrong umbrella detail chunk → Mitigation: the umbrella intent/contract a slice depends on lives in the exact, always-present cached prefix; cold recall only supplies supplementary detail, and the `Vault::search` keyword boost biases toward literal term overlap.
- [Risk] A slice silently restates the umbrella anyway, defeating the saving → Mitigation: the `spec-slices` capability makes "MUST NOT restate the umbrella's Why or design" a requirement; the saving is measured (`source=lean-spec`) so regression is observable on the ledger.
- [Risk] An `umbrella_id` pointer dangles if the umbrella is archived or never stored → Mitigation: cold retrieval returns an empty `Vec` (not an error) when the namespace has no entries (`cold_context`, `crates/smedja-memory/src/memory.rs:291`), so a missing umbrella degrades to "slice intent only" rather than failing the loop.
- [Risk] The flat manifest gives no enforcement that a slice's `umbrella_id` is valid → Mitigation: the pointer is a convention validated at authoring time; this matches how the vault `payload` kind discriminator is already an untyped convention rather than a schema-enforced field.

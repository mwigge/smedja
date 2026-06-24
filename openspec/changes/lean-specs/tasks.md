## 1. Umbrella storage in the vault

- [x] 1.1 Write a failing test asserting an umbrella's design detail is chunked and stored under an `umbrella:<id>` namespace via `Vault::insert`/`upsert` (`crates/smedja-vault/src/vault.rs:149`, `:245`), each entry's `payload` carrying `{"kind":"umbrella","umbrella_id":<id>}` modelled on `bin/smdjad/src/executor/mod.rs:182`
- [x] 1.2 Implement the umbrella-store path so the entries land in the `umbrella:<id>` namespace with the `{"kind":"umbrella",...}` payload, reusing the existing namespace + payload threading (`bin/smdjad/src/executor/mod.rs:490-517`)
- [x] 1.3 Write a failing test asserting `Vault::search`/`query` over the `umbrella:<id>` namespace (`crates/smedja-vault/src/vault.rs:289`, `:504`) returns only that umbrella's chunks
- [x] 1.4 Make 1.3 pass; assert no other namespace's entries leak into the result

## 2. Slice pointer (umbrella_id, slice_n)

- [x] 2.1 Write a failing test asserting a slice records `umbrella_id` and `slice_n` as pointer metadata (the vault `payload` convention), not a manifest `parent` field — `.openspec.yaml` stays flat (`schema` + `created` only)
- [x] 2.2 Implement the slice pointer so a slice resolves its umbrella id from its own metadata
- [x] 2.3 Write a failing test asserting a slice resolves its umbrella via the pointer: given a slice carrying `umbrella_id`, the umbrella's chunks are retrieved from the `umbrella:<id>` namespace
- [x] 2.4 Make 2.3 pass; assert a dangling `umbrella_id` (no stored chunks) yields an empty result, not an error (matching `cold_context`, `crates/smedja-memory/src/memory.rs:291`)

## 3. Hybrid slice context loading (prefix + vault)

- [x] 3.1 Write a failing test asserting umbrella intent/contract is pushed before `seal_prefix()` so it falls inside `stable_prefix()` (`crates/smedja-memory/src/memory.rs:139`, `:146`) and is re-sent on every slice from the cached prefix
- [x] 3.2 Make 3.1 pass; assert the umbrella intent is within the sealed prefix and the slice delta is in the mutable window after the boundary
- [x] 3.3 Write a failing test asserting umbrella design detail is loaded on demand via `cold_context` with the cold-query namespace set to `umbrella:<id>` (`set_cold_query`, `crates/smedja-memory/src/memory.rs:108`; `Vault::search`, `crates/smedja-vault/src/vault.rs:289`)
- [x] 3.4 Make 3.3 pass; assert a slice loads umbrella intent from the cached prefix AND detail from the vault in one assembly
- [x] 3.5 Write a failing test asserting a slice does NOT restate the umbrella: the slice's own content excludes the umbrella's Why/design and the umbrella appears only via the cached prefix + cold recall
- [x] 3.6 Make 3.5 pass

## 4. Loop consumes umbrella-once + slice-each

- [x] 4.1 Write a failing test asserting the umbrella's `tasks.md` coarse `- [ ]` lines are read as the slice list (`read_pending_slices`, `bin/smdjad/src/loop_runner.rs:175-187`) and fed to `smedja_loop::drive` (`:254`)
- [x] 4.2 Make 4.1 pass; assert each `- [ ]` group maps to exactly one slice the engine iterates (`crates/smedja-loop/src/engine.rs:178`)
- [x] 4.3 Write a failing test asserting the loop loads the umbrella intent once (cached prefix sealed once before the slice iteration) and the slice content each iteration
- [x] 4.4 Make 4.3 pass; assert `seal_prefix()` is called once and not re-sealed per slice

## 5. Self-measured savings (source=lean-spec)

- [x] 5.1 Write a failing test asserting a `TokensSavedEntry` is recorded with `saved = full_spec_paste_tokens − umbrella_retrieved_tokens` via `insert_tokens_saved` (`bin/smdjad/src/executor/mod.rs:226`; `crates/smedja-ingot/src/handle.rs:631`)
- [x] 5.2 Make 5.1 pass; assert the saving is recorded only when positive (matching `record_tokens_saved`, `bin/smdjad/src/executor/mod.rs:209`)
- [x] 5.3 Write a failing test asserting the recorded saving is tagged `source=lean-spec` so the token-economy sibling proposal can attribute it
- [x] 5.4 Make 5.3 pass

## 6. Verify

- [x] 6.1 Run `cargo test --workspace` — all green
- [x] 6.2 Run `cargo clippy -p smedja-vault -p smedja-memory -p smdjad -- -D warnings` — clean for touched code
- [x] 6.3 Run `openspec validate lean-specs --strict` — clean

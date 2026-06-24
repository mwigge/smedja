## Why

The cold stratum of the smedja memory architecture is advertised but not wired. Three facts establish the gap:

- `WorkingMemory::cold_context()` (`crates/smedja-memory/src/memory.rs:226`) is a stub that returns `Vec::new()` unconditionally. It holds no vault handle and never queries anything. Its own test `cold_context_stub_returns_empty` (memory.rs:641) asserts the empty result.
- `WorkingMemory::build_prompt()` (memory.rs:178) explicitly omits cold turns: the `Stratum::Cold | Stratum::Archive` arm is a comment that skips them (memory.rs:203). Beyond the warm window, history simply disappears from the prompt — there is no semantic recall path.
- The README documents the consequence directly: "`smedja-vault` storage and cosine-similarity retrieval are implemented; the `smedja_vault_search` tool exposed to agents currently returns empty results — daemon wiring is in progress." (`README.md:156`). The diagram at `README.md:149` shows a hot/warm/cold/archive strata flow whose cold tier never feeds back into context.

The underlying machinery is real and tested. `smedja-vault` is a SQLite-backed store: `Vault::search(query_vec, query_text, namespace, k)` (`crates/smedja-vault/src/vault.rs:289`) performs hybrid cosine + keyword + recency ranking, and `Vault::query(query_embedding, k)` (vault.rs:504) performs pure top-K cosine retrieval over `f32` BLOB embeddings via `cosine_sim` (`crates/smedja-vault/src/similarity.rs:12`). `bin/smdjad/src/embedder.rs` produces fixed-`DIM` (128) L2-normalised embeddings through `embed(text)` (embedder.rs:19). The vault is already populated on the live path: `session.compact` indexes its summary into the `"compact"` namespace (`bin/smdjad/src/handlers/checkpoint.rs:187`), and task completion merges vault writes (`bin/smdjad/src/handlers/task.rs:235`). The `smedja_vault_search` executor (`bin/smdjad/src/executor/mod.rs:248`) already embeds the query and calls `Vault::search`, so its empty-result behaviour reported in the README is a population/integration symptom, not a missing executor.

What is missing is the connective tissue: a cold-retrieval function that embeds a query, searches the vault, and returns ranked context that `WorkingMemory` can surface, plus an explicit, documented contract for the `smedja_vault_search` tool that matches what the executor returns. This change wires cold retrieval end-to-end and makes the README claim true.

## What Changes

- **Introduce a cold-retrieval port in `smedja-memory`.** Define a `ColdStore` trait with an async `retrieve(query, namespace, k) -> Vec<ColdResult>` method. `WorkingMemory` gains an optional `cold_store: Option<Arc<dyn ColdStore>>` field, set via a constructor or `with_cold_store` builder. `smedja-memory` does not depend on `smedja-vault` or the embedder — it depends only on the abstraction.
- **Implement `cold_context()` against the port.** Replace the stub with a real body: when a cold store is attached, embed the query (via the store) and return the ranked `Message` results; when none is attached, return `Vec::new()` (preserving current behaviour for callers that did not opt in). The signature stays `async`.
- **Provide a vault-backed `ColdStore` adapter in `smdjad`.** A new adapter wraps `Arc<Mutex<Vault>>` and the daemon's `embedder::embed`, embeds the query, calls `Vault::search`, maps `VaultEntry` rows to `ColdResult`/`Message`, and ranks by score. Vault I/O is dispatched through `tokio::task::spawn_blocking` because `Vault` is synchronous (`crates/smedja-vault/src/vault.rs:75`).
- **Wire cold retrieval into the orchestrator prompt assembly.** In `TurnOrchestrator` (`bin/smdjad/src/orchestrator/mod.rs:434`), construct `WorkingMemory` with the vault-backed cold store, retrieve the top-K cold results for the user turn, and inject them as a bounded, clearly-delimited context block ahead of the user message — gated by the per-tier budget so cold context never displaces hot turns.
- **Make the `smedja_vault_search` tool contract explicit.** Specify the executor's behaviour (`bin/smdjad/src/executor/mod.rs:248`): embed `query`, call `Vault::search` over `namespace` with limit `k` (default 5), return a JSON object `{ "results": [...] }` whose entries carry `id`, `content`, `namespace`, and `payload`, ranked by descending score; return an empty `results` array (not an error) when the vault has no match. Update `README.md:156` to drop the "returns empty results — daemon wiring is in progress" caveat once retrieval is live.

Out of scope (referenced only): replacing the FNV-1a bag-of-words embedder with a learned embedding model; persisting `WorkingMemory` across daemon restarts; changing the vault dedup or recency-boost heuristics in `Vault::search`/`Vault::insert`.

## Capabilities

### New Capabilities

- `cold-retrieval`: `WorkingMemory` retrieves semantically-relevant context from the cold stratum through a `ColdStore` port — embedding the query, cosine-searching a vault-backed store, and returning ranked results — so context beyond the warm window is recalled on demand instead of being silently dropped.

### Modified Capabilities

- `vault-search-tool`: the `smedja_vault_search` agent tool has a defined contract — it embeds the query, runs hybrid cosine search over the named namespace, and returns ranked results as a JSON `results` array, returning empty (not an error) on no match. Supersedes the prior undocumented behaviour and the README claim that the tool "returns empty results".

## Impact

- `crates/smedja-memory/src/`: add a `cold` module (the `ColdStore` trait + `ColdResult`); add an optional cold-store field and builder to `WorkingMemory` (`memory.rs`); replace the `cold_context()` stub body (memory.rs:226) and update `cold_context_stub_returns_empty` (memory.rs:641) to a no-store case; re-export the new types from `lib.rs`.
- `bin/smdjad/src/`: add a vault-backed `ColdStore` adapter (embedder + `Arc<Mutex<Vault>>`, `spawn_blocking`); wire it into `TurnOrchestrator` prompt assembly (`orchestrator/mod.rs:434`) with a budgeted cold-context block.
- `bin/smdjad/src/executor/mod.rs`: no behavioural change required; the `smedja_vault_search` arm (executor/mod.rs:248) is brought under an explicit specified contract and covered by the existing/extended tests.
- `README.md:156`: the cold-retrieval and `smedja_vault_search` claims become accurate; the "wiring in progress" caveat is removed.

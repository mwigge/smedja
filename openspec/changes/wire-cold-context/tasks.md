## 1. Define the ColdStore port in smedja-memory

- [x] 1.1 Add `async-trait` to `crates/smedja-memory/Cargo.toml` dependencies (no new edge to `smedja-vault`)
- [x] 1.2 Write a failing test `cold_store_retrieve_is_invoked_with_query_namespace_and_k` in a new `crates/smedja-memory/src/cold.rs` test module using a hand-written fake `ColdStore` that records its arguments
- [x] 1.3 Add `crates/smedja-memory/src/cold.rs`: the `#[async_trait] pub trait ColdStore: Send + Sync { async fn retrieve(&self, query: &str, namespace: &str, k: usize) -> Vec<ColdResult>; }` and `pub struct ColdResult { content, score, namespace }`
- [x] 1.4 Re-export `cold::{ColdStore, ColdResult}` from `crates/smedja-memory/src/lib.rs`

## 2. Attach a cold store to WorkingMemory

- [x] 2.1 Write a failing test `with_cold_store_attaches_store_and_default_query_config` asserting the builder sets the store and the default cold-query config (`namespace = "compact"`, `k = 3`)
- [x] 2.2 Add fields to `WorkingMemory` (`crates/smedja-memory/src/memory.rs:19`): `cold_store: Option<Arc<dyn ColdStore>>` and a small `ColdQuery { namespace, k }` config; default both in `WorkingMemory::new` (memory.rs:36) to `None` / `("compact", 3)`
- [x] 2.3 Add `pub fn with_cold_store(self, store: Arc<dyn ColdStore>) -> Self` and `pub fn set_cold_query(&mut self, namespace: impl Into<String>, k: usize)`

## 3. Implement cold_context against the port

- [x] 3.1 Rewrite the existing `cold_context_stub_returns_empty` test (memory.rs:641) as `cold_context_returns_empty_without_store` — `WorkingMemory::new(..)` with no store still yields `Vec::new()`
- [x] 3.2 Write a failing test `cold_context_returns_ranked_messages_from_store` using a fake `ColdStore` that returns two scored `ColdResult`s; assert `cold_context(query).await` returns them as `Message`s in descending-score order
- [x] 3.3 Replace the `cold_context` body (memory.rs:226): when `cold_store` is `Some`, call `retrieve(query, &self.cold_query.namespace, self.cold_query.k).await`, map each `ColdResult` to a `Message`, preserve order; when `None`, return `Vec::new()`
- [x] 3.4 Run `cargo test -p smedja-memory`; confirm all strata/budget tests still pass and the two new tests are green

## 4. Vault-backed ColdStore adapter in smdjad

- [x] 4.1 Write a failing test `vault_cold_store_retrieves_ranked_entries` in `bin/smdjad`: open an in-memory `Vault`, upsert two entries with embeddings from `embedder::embed`, build the adapter, and assert `retrieve("…", ns, 5).await` returns them ranked by descending score
- [x] 4.2 Add the adapter (e.g. `bin/smdjad/src/orchestrator/cold.rs` or a `cold_store` module): a struct holding `Arc<Mutex<Vault>>`, implementing `ColdStore::retrieve` by embedding the query via `crate::embedder::embed`, calling `Vault::search(&qv, query, namespace, k)` inside `tokio::task::spawn_blocking`, and mapping `VaultEntry` → `ColdResult`
- [x] 4.3 Apply a minimum-score floor: discard results below the floor; return an empty `Vec` when none clear it
- [x] 4.4 Run `cargo test -p smdjad` for the adapter test

## 5. Wire cold retrieval into the orchestrator prompt

- [x] 5.1 Write a failing orchestrator test `cold_context_block_injected_and_budget_capped` asserting that, given a populated vault, a `<cold_context>` block is added ahead of the sealed user turn and its estimated token cost does not exceed the per-tier cold-budget fraction
- [x] 5.2 In `TurnOrchestrator` (`bin/smdjad/src/orchestrator/mod.rs:434`), construct `WorkingMemory::new(budget_tokens).with_cold_store(adapter)` and set the per-tier cold `k` (fast 1 / local 3 / deep 5) via `set_cold_query`
- [x] 5.3 Before `mem.seal_prefix()` (orchestrator/mod.rs:502), call `mem.cold_context(&task.title).await`; if non-empty, assemble a single delimited `<cold_context>…</cold_context>` system block, drop lowest-scored entries until the estimated cost fits the cold-budget fraction, and push it ahead of the user turn so it falls inside the sealed prefix
- [x] 5.4 Emit a debug span field for the count of cold results injected (mirroring the `graph_symbols_injected` field at orchestrator/mod.rs:495)

## 6. Specify and verify the smedja_vault_search tool contract

- [x] 6.1 Confirm the executor arm (`bin/smdjad/src/executor/mod.rs:248`) matches the specified contract: embed `query`, `Vault::search(qv, query, namespace, k)` with `k` default 5 / `namespace` default `"default"`, return `{ "results": [ { id, content, namespace, payload } ] }`
- [x] 6.2 Ensure coverage: `smedja_vault_search_returns_results_when_vault_has_matching_entries` (executor/mod.rs:857) and `vault_search_returns_empty_when_no_entries` (executor/mod.rs:761) cover the populated and empty-result cases; add a `k`-limit/ranking assertion if missing
- [x] 6.3 Update `README.md:156` to remove the "returns empty results — daemon wiring is in progress" caveat and state that cold retrieval and `smedja_vault_search` are wired

## 7. Verify

- [x] 7.1 `cargo fmt -p smedja-memory -p smdjad` (package-scoped to avoid churning unrelated crates)
- [x] 7.2 `cargo clippy -p smedja-memory -p smdjad --all-targets -- -D warnings -W clippy::pedantic` clean for touched crates (`smedja-memory`, `smdjad`); pre-existing debt in untouched crates noted, not introduced
- [x] 7.3 `cargo test -p smedja-memory -p smdjad` — all green; `cargo build --workspace` clean
- [x] 7.4 `openspec validate wire-cold-context --strict` — clean

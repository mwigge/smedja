## Context

The smedja memory architecture defines four strata: hot (verbatim recent turns), warm (budgeted recent turns), cold (semantically retrievable durable store), and archive. Today only hot and warm are real on the live prompt path:

- `WorkingMemory::build_prompt(budget_tokens)` (`crates/smedja-memory/src/memory.rs:178`) emits the sealed prefix, all hot turns verbatim, and warm turns until the budget is exhausted. The `Stratum::Cold | Stratum::Archive` match arm (memory.rs:203) is an empty skip — cold turns never re-enter the prompt.
- `WorkingMemory::cold_context(&self, _query: &str)` (memory.rs:226) is a stub returning `Vec::new()`. `WorkingMemory` holds no reference to any store; the crate has no dependency that could perform retrieval.

The retrieval substrate is complete and tested in two lower layers:

- `smedja-vault` is a synchronous SQLite store (`crates/smedja-vault/src/vault.rs:75`). `Vault::search(query_vec, query_text, namespace, k)` (vault.rs:289) returns the top-`k` `VaultEntry` rows by a hybrid score (cosine via `similarity::cosine_sim` at similarity.rs:12, plus a per-term keyword boost and a 24-hour recency boost). `Vault::query(query_embedding, k)` (vault.rs:504) is the pure cosine variant returning `QueryResult { id, score, payload }`. Embeddings are stored as little-endian `f32` BLOBs.
- `bin/smdjad/src/embedder.rs` produces a fixed `DIM = 128` L2-normalised bag-of-words vector via `embed(text)` (embedder.rs:19). It lives in the daemon binary, not in a shared crate.

The vault is populated on the live path: `session.compact` indexes its summary into the `"compact"` namespace (`bin/smdjad/src/handlers/checkpoint.rs:187`) and task completion merges entries (`bin/smdjad/src/handlers/task.rs:235`). The `smedja_vault_search` executor (`bin/smdjad/src/executor/mod.rs:248`) already embeds the query, calls `Vault::search` inside `spawn_blocking`, and returns `{ "results": [...] }`; its tests (`vault_store_then_search_finds_entry`, `smedja_vault_search_returns_results_when_vault_has_matching_entries`) pass. The remaining gap is that nothing connects `WorkingMemory::cold_context` to this substrate, and the tool contract is undocumented.

## Goals / Non-Goals

Goals:

- Make `WorkingMemory::cold_context(query)` return real, ranked results from a vault-backed cold store.
- Keep `smedja-memory` free of a direct dependency on `smedja-vault` and the embedder, via a `ColdStore` port owned by the memory crate.
- Surface cold results into the orchestrator's prompt assembly under the existing per-tier token budget so cold context never displaces hot turns.
- Pin down the `smedja_vault_search` tool contract (inputs, ranking, empty-result semantics) and align it with the executor.

Non-Goals:

- Replacing the FNV-1a bag-of-words embedder with a learned model — the embedder is treated as a fixed dependency of the adapter.
- Adding a vector index; full-scan cosine over the documented sub-10k-entry vault is sufficient (vault.rs:493).
- Persisting `WorkingMemory` across daemon restarts; it is built per turn as it is today.
- Modifying vault dedup, recency-boost, or `Vault::insert` behaviour.
- Auto-writing turns into the cold store; population stays owned by `session.compact` and task completion.

## Decisions

**Decision: define a `ColdStore` port in `smedja-memory`, injected into `WorkingMemory`.**
Add a trait to a new `cold` module:

```
#[async_trait::async_trait]
pub trait ColdStore: Send + Sync {
    async fn retrieve(&self, query: &str, namespace: &str, k: usize) -> Vec<ColdResult>;
}
pub struct ColdResult { pub content: String, pub score: f32, pub namespace: String }
```

`WorkingMemory` gains `cold_store: Option<Arc<dyn ColdStore>>`, set by `WorkingMemory::with_cold_store(store)` (a builder returning `Self`) or left `None` by `new`.

- Rationale: `smedja-memory` stays a pure context-assembly crate. The dependency arrow points from `smdjad` (which owns both the vault and the embedder) into `smedja-memory`, not the reverse — no new edge from `smedja-memory` to `smedja-vault`. This matches the existing pattern where the memory crate depends only on `smedja-adapter` for types.
- Alternative considered: pass `Arc<Mutex<Vault>>` directly into `WorkingMemory`. Rejected — it forces `smedja-memory` to depend on `smedja-vault` and the embedder, inverting the layering and making the crate untestable without SQLite.

**Decision: `cold_context()` delegates to the port; no store means empty.**
The new body is: `match &self.cold_store { Some(store) => store.retrieve(query, namespace, k).await.into_iter().map(into_message).collect(), None => Vec::new() }`. The namespace and `k` come from a small `ColdQuery` config on `WorkingMemory` (defaulting to the `"compact"` namespace and `k = 3`).

- Rationale: callers that never attach a store (e.g. unit tests of strata logic) keep today's behaviour; the signature stays `async` so no caller breaks. The existing `cold_context_stub_returns_empty` test is repurposed as the no-store case.

**Decision: a vault-backed `ColdStore` adapter lives in `smdjad`.**
The adapter holds `Arc<Mutex<Vault>>` and uses `embedder::embed`. `retrieve` embeds the query, then runs `Vault::search(&query_vec, query, namespace, k)` inside `tokio::task::spawn_blocking` (because `Vault` is synchronous and `blocking_lock` would stall the executor — vault.rs:75 documents this), and maps each `VaultEntry` to `ColdResult { content, score, namespace }`.

- Rationale: reuses the exact retrieval path the `smedja_vault_search` executor already uses (`Vault::search`, executor/mod.rs:265), so cold context and the agent tool rank identically. `spawn_blocking` keeps the async runtime unblocked.
- Note: `Vault::search` already orders by descending total score and truncates to `k` (vault.rs:401); the adapter preserves that order.

**Decision: cold context is injected as a bounded block, after hot, under the tier budget.**
In `TurnOrchestrator` (orchestrator/mod.rs:434) the orchestrator constructs `WorkingMemory::new(budget).with_cold_store(adapter)`, then before sealing retrieves cold results for the user-turn text and, if any, appends a single delimited `<cold_context>...</cold_context>` system block whose total estimated token cost is capped at a fraction of the tier budget (drop lowest-scored results until it fits). Cold context is supplementary recall, never a replacement for hot turns.

- Rationale: hot turns are always verbatim and must not be evicted by recall; bounding the cold block by a budget fraction guarantees that. Delimiting the block lets the model distinguish recalled context from the live turn.
- Top-K choice: `k = 3` for cold retrieval by default, scaled by tier (`fast` may use `k = 1`, `deep` up to `k = 5`), mirroring the per-tier budgeting already applied to the warm window via `strata_for_tier` (orchestrator/mod.rs:428).

**Decision: cosine ranking semantics are inherited, not reimplemented.**
Ranking is whatever `Vault::search` produces (hybrid cosine + keyword + recency). The cold-retrieval capability specifies "ranked by descending relevance score" and defers the exact score composition to the vault, which is already specified and tested.

- Rationale: a single ranking implementation; no drift between the cold path and the agent tool.

**Decision: the `smedja_vault_search` contract is specified, executor unchanged.**
The executor (executor/mod.rs:248) already matches the target contract: embed `query`, `Vault::search(query_vec, query_text, ns, k)` with `k` default 5 and `namespace` default `"default"`, return `{ "results": [ { id, content, namespace, payload } ] }`, empty array on no match. This change writes that contract down as a requirement and ensures test coverage; no executor code change is required unless a test reveals a gap.

## Risks / Trade-offs

- [Risk] Injecting cold context could push the prompt over the model window → Mitigation: the cold block is capped at a fraction of the per-tier budget and assembled after hot/warm accounting; lowest-scored results are dropped to fit. Add a test asserting the cold block never exceeds its cap.
- [Risk] The bag-of-words embedder yields weak semantic recall (synonyms miss) → Mitigation: out of scope to replace; the hybrid `Vault::search` keyword boost partially compensates, and the port lets a better embedder drop in later without touching `smedja-memory`.
- [Risk] `blocking_lock` on the vault from an async context would stall the runtime → Mitigation: all adapter vault I/O runs inside `spawn_blocking`, matching the executor's existing pattern (executor/mod.rs:262).
- [Risk] Low-relevance cold hits add noise to the prompt → Mitigation: a minimum-score floor below which results are discarded; the block is omitted entirely when no result clears the floor.
- [Risk] Per-turn cold retrieval adds a vault scan each turn → Mitigation: full-scan cosine over a sub-10k-entry store is cheap relative to a provider round-trip; retrieval runs once per turn before the tool loop, not per iteration.

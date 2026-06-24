## 1. Embedder port + FNV default

- [x] 1.1 Write a failing test for an `Embedder` trait contract: a `FnvEmbedder` reports `model_id() == "fnv-bow-128"`, `dim() == 128`, and `embed("hello world").len() == 128` (new module beside `bin/smdjad/src/embedder.rs`)
- [x] 1.2 Define the `Embedder` port — `embed(&self, text: &str) -> Vec<f32>` (sync core), `model_id(&self) -> &str`, `dim(&self) -> usize` — and an async `embed_query` used on the live path; keep `embedder::embed`/`DIM` and wrap them in `FnvEmbedder` to make 1.1 pass
- [x] 1.3 Write a failing test asserting `FnvEmbedder::embed` is byte-identical to the existing `embedder::embed` for the same input (no behaviour change to the default)
- [x] 1.4 Make 1.3 green by delegating `FnvEmbedder::embed` to `embedder::embed`

## 2. Per-row model/dim tagging in the vault

- [x] 2.1 Write a failing test in `crates/smedja-vault/src/vault.rs` asserting a `VaultEntry` round-trips its `embedder_model_id` and `dim` through `insert`/`upsert` and back out of `search`
- [x] 2.2 Add `embedder_model_id: String` and `dim: usize` to `VaultEntry` (`vault.rs:8`); add `embedder_model_id TEXT` and `dim INTEGER` to the `vault_entries` schema and to the idempotent `ALTER TABLE ... ADD COLUMN` block (`vault.rs:637`); persist/read them in `insert`, `upsert`, `search`, `query`
- [x] 2.3 Write a failing test asserting a legacy row (model id absent) reads back with `embedder_model_id` defaulted to `"fnv-bow-128"` and `dim` derived from the BLOB length / 4
- [x] 2.4 Make 2.3 green with column defaults / read-time backfill of the legacy default

## 3. Same-model-only comparison

- [x] 3.1 Write a failing test: a `search` (and `query`) over a namespace holding two `model_id`s returns only rows matching the query's model/dim, and a mismatched-dim row does NOT error and is NOT ranked (replaces the `DimensionMismatch`-crash expectation at `vault.rs:527` for the mixed-corpus case)
- [x] 3.2 Thread the query's `model_id`/`dim` into `Vault::search`/`Vault::query`; in the scan, skip any row whose `embedder_model_id`/`dim` differ before calling `cosine_sim` (`similarity.rs:12`); keep the keyword + recency boosts unchanged
- [x] 3.3 Write a failing test asserting same-model results still rank by descending hybrid score exactly as before (regression guard on the unchanged scoring); make green if needed

## 4. Learned `/v1/embeddings` backend

- [x] 4.1 Write a failing test for a `LearnedEmbedder` against a mock `POST /v1/embeddings` server (mirroring `MockSwapServer` in `crates/smedja-adapter/src/local.rs:641`): a 200 with `{ "data": [{ "embedding": [...] }] }` yields that vector and the configured `model_id`/`dim`
- [x] 4.2 Implement `LearnedEmbedder` issuing `POST {endpoint}/v1/embeddings` with `{ "model", "input" }` and parsing `data[0].embedding`, reusing the `reqwest`/timeout pattern from `local.rs`; report the configured `model_id`/`dim`
- [x] 4.3 Write a failing test asserting an unreachable endpoint does NOT panic and the live `embed_query` path falls back to the FNV vector (Decision 4)
- [x] 4.4 Make 4.3 green: on transport error / non-success / timeout, fall back to `FnvEmbedder` for that call and log at debug (no turn-aborting error)

## 5. Config-driven embedder selection

- [x] 5.1 Write a failing test for an `[embedder]` config loader mirroring `bin/smdjad/src/methodology_config.rs`: `backend = "fnv"` resolves the FNV default; `backend = "learned"` with an endpoint resolves the learned backend; a missing/unparseable file resolves to the FNV default
- [x] 5.2 Implement the `[embedder]` block + loader (`backend`, learned `endpoint`, `model`, `dim`), never blocking startup on config trouble
- [x] 5.3 Write a failing test asserting that with `backend = "learned"` but the endpoint unreachable at startup health-check, the resolver returns the FNV backend (availability fallback, Decision 2/4)
- [x] 5.4 Implement startup resolution: probe the learned endpoint (reuse the `local.rs` health-check shape); on failure resolve `Arc<dyn Embedder>` to `FnvEmbedder`; make 5.3 green

## 6. Route all call sites through the port

- [x] 6.1 Thread the resolved `Arc<dyn Embedder>` to each `crate::embedder::embed(...)` call site and replace the direct call: `bin/smdjad/src/orchestrator/cold.rs:53,142`, `bin/smdjad/src/executor/mod.rs:466,518`, `bin/smdjad/src/lean_spec.rs:170,209,242`, `bin/smdjad/src/main.rs` (compact/checkpoint paths), `bin/smdjad/src/handlers/{checkpoint.rs:190,session.rs:535,task.rs:237}`
- [x] 6.2 At each store call site, set `VaultEntry.embedder_model_id`/`dim` from the embedder; at each search/query call site, pass the embedder's `model_id`/`dim`
- [x] 6.3 Call `Vault::set_embedder_identity` with the active embedder at vault open (`bin/smdjad/src/main.rs:113`)
- [x] 6.4 Update existing call-site tests (e.g. `cold.rs` `insert` helper, `executor/mod.rs` vault-search tests) to populate the new `VaultEntry` fields; run `cargo test -p smdjad` and fix fallout

## 7. Re-embed / backfill command

- [x] 7.1 Write a failing test: a vault seeded with FNV rows, after a backfill under a stub learned `Embedder`, has every row's `embedder_model_id`/`dim` equal to the learned model and the re-embedded vector matches the stub output
- [x] 7.2 Add a `Vault` iterate-and-rewrite helper (read rows in a namespace, rewrite embedding + `embedder_model_id` + `dim`) and a `vault.reembed` RPC handler in `bin/smdjad/src/main.rs` that drives it with the active learned `Embedder`; restartable and idempotent (re-embedding an already-active row is a no-op write)
- [x] 7.3 Write a failing test asserting backfill is a no-op for rows already at the active model (idempotency); make green
- [x] 7.4 After a full-namespace backfill, update the global `EmbedderIdentity` to the active model; add a test asserting `get_embedder_identity` reflects it

## 8. Verify

- [x] 8.1 Run `cargo test --workspace` — all green (no failures introduced)
- [x] 8.2 Run `cargo clippy -p smdjad -p smedja-vault -- -D warnings` — clean for the touched code
- [x] 8.3 Run `openspec validate vault-embedder --strict` — clean

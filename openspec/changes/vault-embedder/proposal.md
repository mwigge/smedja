## Why

Vault search runs on a bag-of-words FNV-1a hash embedder (`bin/smdjad/src/embedder.rs` — `embed`, `DIM = 128`, L2-normalised). It hashes lowercased whitespace tokens into 128 buckets and counts collisions, so two texts only score highly when they literally share words. Synonyms, paraphrases, and reworded recalls miss entirely: "auth token refresh" and "renew the session credential" share no buckets and score ~0. This weak semantic recall degrades every retrieval path that the embedder backs:

- **Cold-context retrieval** — `VaultColdStore::retrieve` (`bin/smdjad/src/orchestrator/cold.rs:53`) embeds the query with `crate::embedder::embed` and floors results at `MIN_COLD_SCORE = 0.05`; reworded queries fall below the floor and are dropped.
- **The `smedja_vault_search` tool** — `bin/smdjad/src/executor/mod.rs:466` embeds the query and calls `Vault::search`; agents get keyword recall only.
- **lean-specs detail recall** — `bin/smdjad/src/lean_spec.rs:170,209` embeds spec chunks and queries; semantic-spec lookup is keyword-bound.
- **The `compact` namespace** — `session.compact` writes summaries embedded with FNV (`bin/smdjad/src/main.rs:2037`, `handlers/checkpoint.rs:190`) that cold retrieval then queries.

The vault already anticipates a pluggable model: `EmbedderIdentity { model, dimensions }` is persisted in `vault_meta` and `Vault::insert` rejects dimension-mismatched embeddings (`crates/smedja-vault/src/vault.rs:63,158`). But nothing sets that identity, every call site hard-wires `crate::embedder::embed`, and `Vault::search` compares any stored BLOB against the query vector regardless of which model produced it — so the moment a second embedder exists, the full-scan cosine in `search`/`query` silently compares incomparable vectors.

This change introduces a pluggable, **learned** embedding backend behind a port — mirroring the existing `ColdStore` port (`crates/smedja-memory/src/cold.rs`) — to sharpen recall, while keeping FNV-1a as the offline default and fallback. It also closes the latent correctness gap: embeddings are tagged with their producing model and dimension, and search only compares same-model vectors.

## What Changes

- **Introduce an `Embedder` port** (a trait: `embed(text) -> Vec<f32>`, a stable `model_id`, and `dim`), mirroring the `ColdStore` port pattern. The daemon selects an implementation by config and availability; the FNV-1a implementation becomes the named offline default behind the port.
- **Add a learned embedding backend** behind the port — a `/v1/embeddings` client reusing the existing local-runner / OpenAI-compatible HTTP path (`crates/smedja-adapter/src/local.rs`, `bin/smdjad/src/provider_pool.rs`). When the endpoint is reachable, queries and stored content are embedded with the learned model; when it is not, the daemon falls back to FNV-1a.
- **Tag every vault row with its producing model** — extend `VaultEntry` with `embedder_model_id` + `dim`, persist them alongside the embedding BLOB, and make `Vault::search`/`Vault::query` compare **only** same-model vectors (rows from a different `model_id`/`dim` are excluded, never compared, never a crash).
- **Provide a re-embed / backfill path** — an RPC/`smj` command that walks existing rows (e.g. the FNV `compact` corpus) and re-embeds them under the active learned model, so an upgraded vault converges to one comparable model space.
- **Keep working offline** — with no learned embedder available the system runs unchanged on FNV-1a (recall is just weaker); a missing or unreachable model never hard-fails a turn, a search, or a store.

## Capabilities

### New Capabilities

- `embedder-port`: the daemon embeds all vault text through an `Embedder` port (trait yielding a vector, a stable `model_id`, and `dim`) rather than a hard-wired function, selecting an implementation by config/availability with the FNV-1a backend as the named default.
- `learned-embeddings`: a learned `/v1/embeddings`-backed `Embedder` sharpens semantic recall when its endpoint is reachable, and the daemon degrades to the FNV-1a backend when it is not — never hard-failing on an absent model.
- `embedding-versioning`: every vault row records the `model_id` and `dim` that produced its embedding; `Vault::search`/`query` compare only same-model vectors (mismatched rows are excluded, not crashed), and a re-embed/backfill command upgrades existing rows into the active model space.

## Impact

- `bin/smdjad/src/embedder.rs`: keep the FNV-1a `embed`/`DIM`; expose it through an `Embedder` impl named (e.g.) `fnv-bow-128` with `dim = 128`.
- New `Embedder` port (trait) + a learned `/v1/embeddings` implementation and an FNV implementation; the daemon resolves one at startup with FNV as the fallback.
- Every `crate::embedder::embed(...)` call site routes through the resolved port instead: `bin/smdjad/src/orchestrator/cold.rs`, `bin/smdjad/src/executor/mod.rs`, `bin/smdjad/src/lean_spec.rs`, `bin/smdjad/src/main.rs`, `bin/smdjad/src/handlers/{checkpoint.rs,session.rs,task.rs}`.
- `crates/smedja-vault/src/vault.rs`: add `embedder_model_id` + `dim` to `VaultEntry`, the schema (`vault_entries` columns + idempotent `ALTER TABLE`), and `insert`/`upsert`/`search`/`query`; same-model-only comparison in `search`/`query`; a `remove_namespace`/iterate hook for backfill.
- `bin/smdjad/src/main.rs`: `set_embedder_identity` to the active model at vault open; a `vault.reembed` RPC handler driving the backfill.
- `.smedja/config.toml` loader (mirroring `bin/smdjad/src/methodology_config.rs`): a `[embedder]` block selecting `backend` (`fnv` | `learned`) and the learned endpoint/model.
- README: vault-search semantic-recall claims become accurate; the offline FNV fallback is documented as the default.

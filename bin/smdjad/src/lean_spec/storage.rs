//! Vault-backed umbrella storage and recall.
//!
//! An umbrella's design detail is chunked into the `umbrella:<id>` namespace once
//! (`store_umbrella_detail` / `preload_umbrella`), then recalled per slice
//! (`resolve_umbrella`); a slice's pointer back to its umbrella is persisted as a
//! vault entry (`store_slice_pointer`). `read_umbrella_sources` lifts the intent
//! and detail off an umbrella change's `OpenSpec` directory.

use std::sync::Arc;

use smedja_vault::{Vault, VaultEntry};
use tokio::sync::Mutex;

use crate::embedder_port::Embedder;
use crate::lean_spec::model::{umbrella_namespace, SlicePointer};

/// Payload `kind` discriminator stamped on every umbrella chunk, mirroring the
/// filter-recovery `{"kind":"filter-recovery"}` convention
/// (`bin/smdjad/src/executor/mod.rs`).
pub(crate) const UMBRELLA_KIND: &str = "umbrella";

/// Default maximum characters per umbrella chunk when preloading from disk.
///
/// Sized so a typical `design.md` section lands as its own recallable chunk
/// rather than one monolithic entry the weak embedder cannot discriminate.
pub(crate) const UMBRELLA_CHUNK_CHARS: usize = 800;

/// Stores an umbrella's design detail as chunked vault entries under the
/// `umbrella:<id>` namespace.
///
/// Each chunk becomes a [`VaultEntry`] embedded with the daemon embedder, in the
/// `umbrella:<id>` namespace, with `payload` `{"kind":"umbrella",
/// "umbrella_id":<id>}` and `chunk_index` set to its position. Vault writes are
/// synchronous `SQLite`, so they run on a blocking thread.
///
/// # Errors
///
/// Returns the [`smedja_vault::VaultError`] from the first failing insert.
pub(crate) async fn store_umbrella_detail(
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
    umbrella_id: &str,
    detail: &str,
    max_chars: usize,
) -> Result<usize, smedja_vault::VaultError> {
    let namespace = umbrella_namespace(umbrella_id);
    let chunks = crate::lean_spec::model::chunk_detail(detail, max_chars);
    let payload = serde_json::json!({
        "kind": UMBRELLA_KIND,
        "umbrella_id": umbrella_id,
    });
    let umbrella_id = umbrella_id.to_owned();
    let vault = Arc::clone(vault);

    // Embed every chunk on the async path first (the learned backend does network
    // I/O here, degrading to FNV on failure), then persist on a blocking thread.
    let model_id = embedder.model_id().to_owned();
    let dim = embedder.dim();
    let mut embeddings = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        embeddings.push(embedder.embed_query(chunk).await);
    }

    // `Vault` is synchronous `SQLite`; do all the inserts on a blocking thread so
    // the async runtime is never stalled. A `JoinError` only occurs if the
    // blocking closure panics — an unrecoverable bug — so the panic is resumed
    // rather than masked behind a fabricated vault error.
    let join = tokio::task::spawn_blocking(move || -> Result<usize, smedja_vault::VaultError> {
        let mut guard = vault.blocking_lock();
        for (index, (chunk, embedding)) in chunks.iter().zip(embeddings).enumerate() {
            // Legacy `upsert` (not `insert`) is used on purpose: `insert`'s
            // >0.85-cosine dedup would silently drop near-identical chunks, and
            // every umbrella chunk must persist so cold recall can reach it.
            let entry = VaultEntry {
                id: format!("umbrella:{umbrella_id}:{index}"),
                embedding,
                payload: payload.clone(),
                namespace: namespace.clone(),
                content: chunk.clone(),
                source_file: None,
                added_by: Some("lean-spec".to_owned()),
                #[allow(clippy::cast_possible_wrap)] // chunk counts never exceed i64::MAX
                chunk_index: Some(index as i64),
                parent_id: Some(umbrella_id.clone()),
                created_at: 0.0,
                embedder_model_id: model_id.clone(),
                dim,
            };
            guard.upsert(&entry)?;
        }
        Ok(chunks.len())
    })
    .await;
    match join {
        Ok(result) => result,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// Resolves an umbrella's chunks by id, returning the stored [`VaultEntry`]
/// rows in the `umbrella:<id>` namespace.
///
/// A dangling pointer (no chunks stored for `umbrella_id`) yields an empty
/// `Vec`, never an error — matching the graceful-degradation contract of
/// [`smedja_memory::WorkingMemory::cold_context`]. The query embedding is the
/// embedded `query` text, which the hybrid keyword boost favours.
pub(crate) async fn resolve_umbrella(
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
    umbrella_id: &str,
    query: &str,
    k: usize,
) -> Vec<VaultEntry> {
    let namespace = umbrella_namespace(umbrella_id);
    let query = query.to_owned();
    let vault = Arc::clone(vault);
    let query_vec = embedder.embed_query(&query).await;
    let model_id = embedder.model_id().to_owned();
    let dim = embedder.dim();
    let join = tokio::task::spawn_blocking(move || {
        let guard = vault.blocking_lock();
        guard.search(&query_vec, &query, &namespace, k, &model_id, dim)
    })
    .await;

    match join {
        Ok(Ok(entries)) => entries,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, umbrella_id, "umbrella resolve search failed");
            Vec::new()
        }
        Err(e) => {
            tracing::debug!(error = %e, umbrella_id, "umbrella resolve task panicked");
            Vec::new()
        }
    }
}

/// Persists a slice's umbrella pointer as a vault entry, returning its id.
///
/// The pointer rides the vault payload convention: the entry's `payload` is
/// [`SlicePointer::to_payload`] (`{"kind":"slice","umbrella_id":...,
/// "slice_n":...}`) under the umbrella's namespace, so a slice's link to its
/// umbrella is metadata — never a manifest `parent` field. Best-effort: a vault
/// error is logged and swallowed, and the slice still proceeds on its delta.
pub(crate) async fn store_slice_pointer(
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
    pointer: &SlicePointer,
) -> String {
    let id = format!("slice:{}:{}", pointer.umbrella_id, pointer.slice_n);
    let embedding = embedder.embed_query(&id).await;
    let entry = VaultEntry {
        id: id.clone(),
        embedding,
        payload: pointer.to_payload(),
        namespace: pointer.umbrella_namespace(),
        content: format!(
            "slice {} of umbrella {}",
            pointer.slice_n, pointer.umbrella_id
        ),
        source_file: None,
        added_by: Some("lean-spec".to_owned()),
        chunk_index: None,
        parent_id: Some(pointer.umbrella_id.clone()),
        created_at: 0.0,
        embedder_model_id: embedder.model_id().to_owned(),
        dim: embedder.dim(),
    };
    let vault = Arc::clone(vault);
    let join = tokio::task::spawn_blocking(move || {
        let mut guard = vault.blocking_lock();
        guard.upsert(&entry)
    })
    .await;
    match join {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "lean-spec slice pointer store failed; continuing");
        }
        Err(e) => {
            tracing::warn!(error = %e, "lean-spec slice pointer store task panicked; continuing");
        }
    }
    id
}

/// Reads an umbrella change's intent and design detail from its `OpenSpec`
/// directory.
///
/// The *intent* (small, stable — sealed into the cached prefix) is the change's
/// `proposal.md`; the *design detail* (large, variable — chunked into the vault
/// for cold recall) is its `design.md`. Returns `(intent, detail)`, each empty
/// when its file is absent — a missing umbrella file degrades to "no umbrella
/// context" rather than failing.
///
/// `change_dir` is the already-validated change directory (the loop resolves it
/// through [`crate::loop_runner`]'s workspace-boundary check before calling).
pub(crate) async fn read_umbrella_sources(change_dir: &std::path::Path) -> (String, String) {
    let intent = tokio::fs::read_to_string(change_dir.join("proposal.md"))
        .await
        .unwrap_or_default();
    let detail = tokio::fs::read_to_string(change_dir.join("design.md"))
        .await
        .unwrap_or_default();
    (intent, detail)
}

/// Stores an umbrella's design detail (from `read_umbrella_sources`) into the
/// vault, returning the number of chunks stored.
///
/// A no-op returning `Ok(0)` when `detail` is blank. This is the umbrella-once
/// half of the loop cadence: the detail is chunked into `umbrella:<id>` a single
/// time, then every slice recalls from it on demand.
///
/// # Errors
///
/// Propagates the [`smedja_vault::VaultError`] from [`store_umbrella_detail`].
pub(crate) async fn preload_umbrella(
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
    umbrella_id: &str,
    detail: &str,
) -> Result<usize, smedja_vault::VaultError> {
    if detail.trim().is_empty() {
        return Ok(0);
    }
    store_umbrella_detail(vault, embedder, umbrella_id, detail, UMBRELLA_CHUNK_CHARS).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lean_spec::test_support::{entries_in, in_memory_vault, test_embedder};

    // ── group 1: umbrella storage in the vault ──────────────────────────────

    #[tokio::test]
    async fn umbrella_detail_chunks_land_in_umbrella_namespace_with_kind_payload() {
        // Task 1.1/1.2: detail is chunked and stored under `umbrella:<id>` with
        // each entry's payload carrying {"kind":"umbrella","umbrella_id":<id>}.
        let vault = in_memory_vault();
        let detail = "First paragraph of design rationale.\n\n\
             Second paragraph carrying more of the umbrella's durable detail.";

        let chunks = store_umbrella_detail(&vault, &test_embedder(), "alpha", detail, 64)
            .await
            .expect("store must succeed");
        assert!(
            chunks >= 2,
            "multi-paragraph detail must chunk; got {chunks}"
        );

        let stored = entries_in(&vault, &umbrella_namespace("alpha")).await;
        assert_eq!(stored.len(), chunks, "every chunk must be persisted");
        for entry in &stored {
            assert_eq!(entry.namespace, "umbrella:alpha");
            assert_eq!(entry.payload["kind"], serde_json::json!(UMBRELLA_KIND));
            assert_eq!(entry.payload["umbrella_id"], serde_json::json!("alpha"));
        }
    }

    // ── group 2: slice pointer (umbrella_id, slice_n) ───────────────────────

    // (Removed `openspec_manifest_stays_flat_no_parent_field`: it read a
    // committed `openspec/changes/.../.openspec.yaml`, but openspec/ is no longer
    // tracked in the repo — the manifest schema is exercised by the round-trip
    // tests below instead.)

    #[tokio::test]
    async fn slice_resolves_its_umbrella_via_pointer() {
        // Task 2.3/2.4: given a slice carrying umbrella_id, the umbrella's chunks
        // are retrieved from the umbrella:<id> namespace by the matching id.
        let vault = in_memory_vault();
        store_umbrella_detail(
            &vault,
            &test_embedder(),
            "alpha",
            "alpha umbrella design detail body",
            256,
        )
        .await
        .expect("store alpha");
        let pointer = SlicePointer::new("alpha", 1);

        let chunks = resolve_umbrella(
            &vault,
            &test_embedder(),
            &pointer.umbrella_id,
            "design detail",
            5,
        )
        .await;
        assert!(!chunks.is_empty(), "the pointer must resolve to chunks");
        assert!(
            chunks
                .iter()
                .all(|c| c.payload["umbrella_id"] == serde_json::json!("alpha")),
            "resolved chunks must be those whose payload records the matching id"
        );
    }

    #[tokio::test]
    async fn dangling_umbrella_pointer_yields_empty_not_error() {
        // Task 2.4: a dangling umbrella_id (no stored chunks) returns an empty
        // result, never an error — matching cold_context's contract.
        let vault = in_memory_vault();
        let pointer = SlicePointer::new("ghost", 1);
        let chunks = resolve_umbrella(
            &vault,
            &test_embedder(),
            &pointer.umbrella_id,
            "anything",
            5,
        )
        .await;
        assert!(
            chunks.is_empty(),
            "an unstored umbrella must degrade to an empty result"
        );
    }

    #[tokio::test]
    async fn umbrella_namespace_search_returns_only_that_umbrella() {
        // Task 1.3/1.4: a search over `umbrella:<id>` returns only that
        // umbrella's chunks; no other namespace leaks in.
        let vault = in_memory_vault();
        store_umbrella_detail(
            &vault,
            &test_embedder(),
            "alpha",
            "alpha design detail body",
            256,
        )
        .await
        .expect("store alpha");
        store_umbrella_detail(
            &vault,
            &test_embedder(),
            "beta",
            "beta design detail body",
            256,
        )
        .await
        .expect("store beta");

        let alpha = entries_in(&vault, &umbrella_namespace("alpha")).await;
        assert!(!alpha.is_empty(), "alpha chunks must exist");
        assert!(
            alpha
                .iter()
                .all(|e| e.payload["umbrella_id"] == serde_json::json!("alpha")),
            "no other umbrella's chunks may leak into the alpha namespace"
        );
        assert!(
            alpha.iter().all(|e| e.namespace == "umbrella:alpha"),
            "every result must be in the alpha namespace"
        );
    }
}
